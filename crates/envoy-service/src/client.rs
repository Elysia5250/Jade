//! Wire-side client: one short request per stream.
//!
//! Nothing here holds a job open. `submit` returns the new job id, and `logs`
//! is a cursor-based watch that can be opened, dropped and reopened at will.

use crate::transport::{AgentDialer, BoxStream};
use envoy_proto::codec::{read_framed, write_framed};
use envoy_proto::{
    Call, Entry, Hello, Job, JobId, LogRecord, Op, Reply, SubmitRequest, AGENT_CHUNK,
    AGENT_SERVICE_ID, AGENT_VERSION, READ_LIMIT,
};
use anyhow::{bail, Result};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

/// How long a peer has to answer the handshake.
///
/// Generous on purpose: a routed stream (the conduit bridge) may have to dial
/// the peer before any byte reaches its agent, and a cold relay join is allowed
/// to take up to 12s upstream.
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);

/// An opened request stream plus the peer's handshake.
pub struct Conn<S> {
    pub stream: S,
    pub hello: Hello,
}

/// Send one request and consume the handshake reply.
pub async fn open<S>(stream: S, peer: &str, op: Op) -> Result<Conn<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut stream = stream;
    write_framed(
        &mut stream,
        &envoy_proto::ClientMsg::Call(Call {
            version: AGENT_VERSION,
            op,
        }),
    )
    .await?;

    let first: Reply = match tokio::time::timeout(HELLO_TIMEOUT, read_framed(&mut stream)).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => bail!("{peer} closed the agent stream during the handshake ({e})"),
        Err(_) => bail!(
            "{peer} did not answer the agent handshake — is the agent service mounted there?"
        ),
    };
    match first {
        Reply::Hello(hello) => Ok(Conn { stream, hello }),
        Reply::Denied { reason } => bail!("{peer} refused: {reason}"),
        Reply::Failed { error } => bail!("{peer} failed: {error}"),
        other => bail!("{peer} sent an unexpected handshake reply: {other:?}"),
    }
}

async fn next_reply<S: AsyncRead + Unpin>(stream: &mut S, peer: &str) -> Result<Reply> {
    match read_framed::<_, Reply>(stream).await {
        Ok(reply) => Ok(reply),
        Err(e) => bail!("{peer} closed the agent stream ({e})"),
    }
}

pub async fn probe<S>(stream: S, peer: &str) -> Result<Hello>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    Ok(open(stream, peer, Op::Probe).await?.hello)
}

/// Enqueue a job. Returns as soon as the target machine has accepted it.
pub async fn submit<S>(stream: S, peer: &str, req: SubmitRequest) -> Result<Job>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut conn = open(stream, peer, Op::Submit(req)).await?;
    match next_reply(&mut conn.stream, peer).await? {
        Reply::Accepted { job } => Ok(job),
        Reply::Failed { error } => bail!(error),
        other => bail!("unexpected reply to a submit: {other:?}"),
    }
}

pub async fn jobs<S>(stream: S, peer: &str) -> Result<Vec<Job>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut conn = open(stream, peer, Op::Jobs).await?;
    match next_reply(&mut conn.stream, peer).await? {
        Reply::Jobs { jobs } => Ok(jobs),
        Reply::Failed { error } => bail!(error),
        other => bail!("unexpected reply to a job list: {other:?}"),
    }
}

pub async fn status<S>(stream: S, peer: &str, id: JobId) -> Result<Job>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut conn = open(stream, peer, Op::JobStatus { id }).await?;
    match next_reply(&mut conn.stream, peer).await? {
        Reply::Job { job } => Ok(job),
        Reply::Failed { error } => bail!(error),
        other => bail!("unexpected reply to a job status: {other:?}"),
    }
}

pub async fn cancel<S>(stream: S, peer: &str, id: JobId) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut conn = open(stream, peer, Op::JobCancel { id }).await?;
    match next_reply(&mut conn.stream, peer).await? {
        Reply::Done { .. } => Ok(()),
        Reply::Failed { error } => bail!(error),
        other => bail!("unexpected reply to a cancel: {other:?}"),
    }
}

/// Read (and optionally follow) a job log. `sink` sees every batch, including
/// empty ones; the returned cursor is safe to resume from later.
pub async fn logs<S, F>(
    stream: S,
    peer: &str,
    id: JobId,
    offset: u64,
    follow: bool,
    mut sink: F,
) -> Result<(u64, Option<i32>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(&[LogRecord]),
{
    let mut conn = open(stream, peer, Op::JobLog { id, offset, follow }).await?;
    loop {
        match next_reply(&mut conn.stream, peer).await? {
            Reply::Log {
                records,
                next,
                finished,
                ..
            } => {
                sink(&records);
                if let Some(code) = finished {
                    return Ok((next, Some(code)));
                }
            }
            Reply::Failed { error } => bail!(error),
            other => bail!("unexpected reply to a job log: {other:?}"),
        }
    }
}

/// Whole-file read (bounded by [`READ_LIMIT`]).
pub async fn read<S>(
    stream: S,
    peer: &str,
    path: &str,
    offset: u64,
    limit: u64,
) -> Result<(u64, u32, Vec<u8>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut conn = open(
        stream,
        peer,
        Op::Read {
            path: path.to_string(),
            offset,
            limit,
        },
    )
    .await?;
    let mut size = 0;
    let mut mode = 0;
    let mut data = Vec::new();
    loop {
        match next_reply(&mut conn.stream, peer).await? {
            Reply::Meta { size: s, mode: m } => {
                size = s;
                mode = m;
            }
            Reply::Data { data: chunk } => {
                if data.len() as u64 + chunk.len() as u64 > READ_LIMIT + 1024 {
                    bail!("peer sent more than the read limit");
                }
                data.extend_from_slice(&chunk);
            }
            Reply::Done { .. } => break,
            Reply::Failed { error } => bail!(error),
            other => bail!("unexpected reply to a read: {other:?}"),
        }
    }
    Ok((size, mode, data))
}

/// Streaming write: `open`, then `send` chunks, then `finish` to commit.
/// Dropping the handle before `finish` rolls the remote `.part` back.
pub struct WriteStream<S> {
    stream: S,
    peer: String,
    sent: u64,
}

impl<S: AsyncRead + AsyncWrite + Unpin> WriteStream<S> {
    pub async fn open(
        stream: S,
        peer: &str,
        path: &str,
        mode: Option<u32>,
    ) -> Result<Self> {
        let conn = open(
            stream,
            peer,
            Op::Write {
                path: path.to_string(),
                mode,
            },
        )
        .await?;
        Ok(Self {
            stream: conn.stream,
            peer: peer.to_string(),
            sent: 0,
        })
    }

    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        use envoy_proto::ClientMsg;
        for chunk in data.chunks(AGENT_CHUNK) {
            write_framed(&mut self.stream, &ClientMsg::Data(chunk.to_vec())).await?;
            self.sent += chunk.len() as u64;
        }
        Ok(())
    }

    pub async fn finish(mut self) -> Result<u64> {
        use envoy_proto::ClientMsg;
        write_framed(&mut self.stream, &ClientMsg::End).await?;
        match next_reply(&mut self.stream, &self.peer).await? {
            Reply::Done { .. } => Ok(self.sent),
            Reply::Failed { error } => bail!(error),
            other => bail!("unexpected reply to a write commit: {other:?}"),
        }
    }
}

pub async fn write<S>(
    stream: S,
    peer: &str,
    path: &str,
    data: &[u8],
    mode: Option<u32>,
) -> Result<u64>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut writer = WriteStream::open(stream, peer, path, mode).await?;
    writer.send(data).await?;
    writer.finish().await
}

pub async fn list<S>(stream: S, peer: &str, path: &str) -> Result<Vec<Entry>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut conn = open(
        stream,
        peer,
        Op::List {
            path: path.to_string(),
        },
    )
    .await?;
    loop {
        match next_reply(&mut conn.stream, peer).await? {
            Reply::Listing { entries } => return Ok(entries),
            Reply::Done { .. } => return Ok(Vec::new()),
            Reply::Failed { error } => bail!(error),
            other => bail!("unexpected reply to a list: {other:?}"),
        }
    }
}

/// High-level client backed by any [`AgentDialer`] adapter.
pub struct Remote<D> {
    dialer: D,
}

impl<D: AgentDialer> Remote<D> {
    pub fn new(dialer: D) -> Self {
        Self { dialer }
    }

    pub fn dialer(&self) -> &D {
        &self.dialer
    }

    async fn dial(&self, peer: &str) -> Result<BoxStream> {
        self.dialer.open_stream(peer, AGENT_SERVICE_ID).await
    }

    pub async fn probe(&self, peer: &str) -> Result<Hello> {
        probe(self.dial(peer).await?, peer).await
    }

    pub async fn submit(&self, peer: &str, req: SubmitRequest) -> Result<Job> {
        submit(self.dial(peer).await?, peer, req).await
    }

    pub async fn jobs(&self, peer: &str) -> Result<Vec<Job>> {
        jobs(self.dial(peer).await?, peer).await
    }

    pub async fn status(&self, peer: &str, id: JobId) -> Result<Job> {
        status(self.dial(peer).await?, peer, id).await
    }

    pub async fn cancel(&self, peer: &str, id: JobId) -> Result<()> {
        cancel(self.dial(peer).await?, peer, id).await
    }

    pub async fn logs<F>(
        &self,
        peer: &str,
        id: JobId,
        offset: u64,
        follow: bool,
        sink: F,
    ) -> Result<(u64, Option<i32>)>
    where
        F: FnMut(&[LogRecord]),
    {
        logs(self.dial(peer).await?, peer, id, offset, follow, sink).await
    }

    pub async fn read(
        &self,
        peer: &str,
        path: &str,
        offset: u64,
        limit: u64,
    ) -> Result<(u64, u32, Vec<u8>)> {
        read(self.dial(peer).await?, peer, path, offset, limit).await
    }

    pub async fn write(
        &self,
        peer: &str,
        path: &str,
        data: &[u8],
        mode: Option<u32>,
    ) -> Result<u64> {
        write(self.dial(peer).await?, peer, path, data, mode).await
    }

    /// Open a streaming write, for payloads too large to hold in memory.
    pub async fn open_write(
        &self,
        peer: &str,
        path: &str,
        mode: Option<u32>,
    ) -> Result<WriteStream<BoxStream>> {
        let stream = self.dial(peer).await?;
        WriteStream::open(stream, peer, path, mode).await
    }

    pub async fn list(&self, peer: &str, path: &str) -> Result<Vec<Entry>> {
        list(self.dial(peer).await?, peer, path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::AgentServer;
    use crate::transport::StaticHost;
    use envoy_core::AgentConfig;
    use envoy_proto::JobResult;
    use envoy_proto::{JobState, TaskSpec, DEFAULT_GROUP};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-wire-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn server_with(enabled: bool, allow: &[&str]) -> (PathBuf, AgentServer) {
        let dir = tmpdir("wire");
        let host = Arc::new(StaticHost::new(AgentConfig {
            enabled,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            max_jobs: 2,
        }));
        (dir.clone(), AgentServer::new(dir, host))
    }

    fn shell(command: &str) -> SubmitRequest {
        SubmitRequest {
            spec: TaskSpec::Shell {
                command: command.into(),
            },
            cwd: None,
            env: Vec::new(),
            label: None,
            group: DEFAULT_GROUP.into(),
            timeout_s: 30,
        }
    }

    async fn serve_one(server: &AgentServer, caller: &str) -> tokio::io::DuplexStream {
        let (client, peer) = tokio::io::duplex(256 * 1024);
        let srv = server.clone();
        let caller = caller.to_string();
        tokio::spawn(async move { srv.serve_stream(&caller, peer).await });
        client
    }

    #[tokio::test]
    async fn submit_returns_a_job_that_keeps_running_detached() {
        let (dir, server) = server_with(true, &["caller-a"]);
        let peer = serve_one(&server, "caller-a").await;
        let job = submit(peer, "caller-a", shell("sleep 0.3; echo detached-ok"))
            .await
            .unwrap();
        assert!(job.id > 0);
        assert!(!job.is_done(), "submit must not wait for the child");

        // Watch it from a *second* stream while the first one is gone.
        let peer = serve_one(&server, "caller-a").await;
        let mut text = String::new();
        let (_cursor, finished) = logs(peer, "caller-a", job.id, 0, true, |records| {
            for r in records {
                text.push_str(&String::from_utf8_lossy(&r.data));
            }
        })
        .await
        .unwrap();
        assert_eq!(finished, Some(0));
        assert!(text.contains("detached-ok"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_dropped_watcher_does_not_kill_the_job() {
        let (dir, server) = server_with(true, &["caller-a"]);
        let job = submit(
            serve_one(&server, "caller-a").await,
            "caller-a",
            shell("sleep 0.4; echo survived"),
        )
        .await
        .unwrap();

        {
            let peer = serve_one(&server, "caller-a").await;
            let mut conn = open(peer, "caller-a", Op::JobLog { id: job.id, offset: 0, follow: true })
                .await
                .unwrap();
            // Read the handshake-confirmed stream for a moment, then hang up.
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(80),
                next_reply(&mut conn.stream, "caller-a"),
            )
            .await;
        }

        let mut text = String::new();
        let (_c, finished) = logs(
            serve_one(&server, "caller-a").await,
            "caller-a",
            job.id,
            0,
            true,
            |records| {
                for r in records {
                    text.push_str(&String::from_utf8_lossy(&r.data));
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(finished, Some(0), "the job died with its watcher");
        assert!(text.contains("survived"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn jobs_status_cancel_and_files_roundtrip() {
        let (dir, server) = server_with(true, &["caller-a"]);

        let long = submit(
            serve_one(&server, "caller-a").await,
            "caller-a",
            shell("sleep 30"),
        )
        .await
        .unwrap();
        let listed = jobs(serve_one(&server, "caller-a").await, "caller-a")
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, long.id);

        let got = status(serve_one(&server, "caller-a").await, "caller-a", long.id)
            .await
            .unwrap();
        assert!(matches!(got.state, JobState::Running { .. } | JobState::Queued { .. }));

        cancel(serve_one(&server, "caller-a").await, "caller-a", long.id)
            .await
            .unwrap();
        for _ in 0..300 {
            let now = status(serve_one(&server, "caller-a").await, "caller-a", long.id)
                .await
                .unwrap();
            if now.is_done() {
                assert_eq!(now.state.result(), Some(&JobResult::Killed));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Files: write + read + list.
        let target = dir.join("sub/hello.txt");
        let written = write(
            serve_one(&server, "caller-a").await,
            "caller-a",
            target.to_str().unwrap(),
            b"hello wire world",
            Some(0o640),
        )
        .await
        .unwrap();
        assert_eq!(written, 16);

        let (size, _, content) = read(
            serve_one(&server, "caller-a").await,
            "caller-a",
            target.to_str().unwrap(),
            6,
            4,
        )
        .await
        .unwrap();
        assert_eq!(size, 16);
        assert_eq!(content, b"wire");

        let entries = list(
            serve_one(&server, "caller-a").await,
            "caller-a",
            dir.join("sub").to_str().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "hello.txt");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unauthorized_callers_are_refused_and_local_bypasses() {
        let (dir, server) = server_with(true, &["caller-a"]);

        let err = probe(serve_one(&server, "intruder").await, "intruder")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("refused"), "{err}");

        // Local callers are the same user by construction, so they skip the
        // remote allowlist but still see the policy for everyone else.
        let hello = probe(serve_one(&server, envoy_proto::LOCAL_CALLER).await, "local")
            .await
            .unwrap();
        assert!(!hello.os.is_empty());

        let disabled = server_with(false, &[]).1;
        let hello = probe(serve_one(&disabled, envoy_proto::LOCAL_CALLER).await, "local")
            .await
            .unwrap();
        assert_eq!(hello.version, envoy_proto::AGENT_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_dropped_write_rolls_the_part_file_back() {
        let (dir, server) = server_with(true, &["caller-a"]);
        let target = dir.join("aborted.txt");
        {
            let mut ws = WriteStream::open(
                serve_one(&server, "caller-a").await,
                "caller-a",
                target.to_str().unwrap(),
                None,
            )
            .await
            .unwrap();
            ws.send(b"partial").await.unwrap();
            // Dropped without `finish`.
        }
        // Give the server a moment to notice EOF and clean up.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!target.exists());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".part-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
