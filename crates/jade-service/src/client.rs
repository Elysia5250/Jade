//! Wire-side client: one short request per stream.
//!
//! Nothing here holds a job open. `submit` returns the new job id, and `logs`
//! is a cursor-based watch that can be opened, dropped and reopened at will.

use anyhow::{bail, Result};
use jade_proto::codec::{read_framed, write_framed};
use jade_proto::{
    Call, Entry, Hello, Job, JobId, LogRecord, Op, Reply, SubmitRequest, AGENT_CHUNK,
    AGENT_VERSION, READ_LIMIT,
};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

/// How long a peer has to answer the handshake.
///
/// Generous on purpose: a routed stream (the topaz bridge) may have to dial
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
        &jade_proto::ClientMsg::Call(Call {
            version: AGENT_VERSION,
            op,
        }),
    )
    .await?;

    let first: Reply = match tokio::time::timeout(HELLO_TIMEOUT, read_framed(&mut stream)).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => bail!("{peer} closed the agent stream during the handshake ({e})"),
        Err(_) => {
            bail!("{peer} did not answer the agent handshake — is the agent service mounted there?")
        }
    };
    match first {
        Reply::Hello(hello) => Ok(Conn { stream, hello }),
        Reply::VersionMismatch {
            server_version,
            client_version,
        } => bail!(
            "{peer} wire protocol mismatch: server v{server_version}, client v{client_version}. Stop the old daemon with `jade daemon stop` and start this binary"
        ),
        Reply::Denied { reason } => bail!("{peer} refused: {reason}"),
        Reply::Failed { error } => bail!("{peer} failed: {error}"),
        other => bail!("{peer} sent an unexpected handshake reply: {other:?}"),
    }
}

pub(crate) async fn next_reply<S: AsyncRead + Unpin>(stream: &mut S, peer: &str) -> Result<Reply> {
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
                if !follow {
                    // One batch per non-follow call: the server closes the
                    // stream right after it, which is not an error.
                    return Ok((next, None));
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

/// Replace an exact snippet in a remote text file. Returns the number of
/// replacements (1 unless `replace_all`).
pub async fn edit<S>(
    stream: S,
    peer: &str,
    path: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<usize>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut conn = open(
        stream,
        peer,
        Op::Edit {
            path: path.to_string(),
            old: old.to_string(),
            new: new.to_string(),
            replace_all,
        },
    )
    .await?;
    match next_reply(&mut conn.stream, peer).await? {
        Reply::Edited { replacements } => Ok(replacements),
        Reply::Failed { error } => bail!(error),
        other => bail!("unexpected reply to an edit: {other:?}"),
    }
}

/// Streaming write: `open`, then `send` chunks, then `finish` to commit.
/// Dropping the handle before `finish` rolls the remote `.part` back.
pub struct WriteStream<S> {
    stream: S,
    peer: String,
    sent: u64,
}

impl<S: AsyncRead + AsyncWrite + Unpin> WriteStream<S> {
    pub async fn open(stream: S, peer: &str, path: &str, mode: Option<u32>) -> Result<Self> {
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
        use jade_proto::ClientMsg;
        for chunk in data.chunks(AGENT_CHUNK) {
            write_framed(&mut self.stream, &ClientMsg::Data(chunk.to_vec())).await?;
            self.sent += chunk.len() as u64;
        }
        Ok(())
    }

    pub async fn finish(mut self) -> Result<u64> {
        use jade_proto::ClientMsg;
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
    match next_reply(&mut conn.stream, peer).await? {
        Reply::Listing { entries } => Ok(entries),
        Reply::Done { .. } => Ok(Vec::new()),
        Reply::Failed { error } => bail!(error),
        other => bail!("unexpected reply to a list: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{glob, grep};
    use crate::server::AgentServer;
    use crate::transport::StaticHost;
    use jade_core::AgentConfig;
    use jade_proto::{JobResult, JobState, SandboxPolicy, TaskSpec, DEFAULT_GROUP};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn tmpdir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("jade-wire-{tag}-{}-{n}-{seq}", std::process::id()));
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
            sandbox: SandboxPolicy::Off,
        }
    }

    #[tokio::test]
    async fn glob_and_grep_roundtrip_over_wire() {
        let (dir, server) = server_with(true, &["caller-a"]);
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "pub fn hello() {}\n").unwrap();

        let paths = glob(
            serve_one(&server, "caller-a").await,
            "caller-a",
            dir.to_str().unwrap(),
            "**/*.rs",
            20,
        )
        .await
        .unwrap();
        assert_eq!(paths, vec!["src/lib.rs"]);

        let hits = grep(
            serve_one(&server, "caller-a").await,
            "caller-a",
            dir.to_str().unwrap(),
            "pub fn hello",
            Some("*.rs"),
            20,
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn serve_one(server: &AgentServer, caller: &str) -> tokio::io::DuplexStream {
        let (client, peer) = tokio::io::duplex(256 * 1024);
        let srv = server.clone();
        let caller = if caller == jade_proto::LOCAL_CALLER {
            crate::Caller::Local
        } else {
            crate::Caller::Remote(caller.to_string())
        };
        tokio::spawn(async move { srv.serve_stream(caller, peer).await });
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
            let mut conn = open(
                peer,
                "caller-a",
                Op::JobLog {
                    id: job.id,
                    offset: 0,
                    follow: true,
                },
            )
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
        assert!(matches!(
            got.state,
            JobState::Running { .. } | JobState::Queued { .. }
        ));

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

        // Surgical edit.
        let edited = dir.join("edit-me.txt");
        write(
            serve_one(&server, "caller-a").await,
            "caller-a",
            edited.to_str().unwrap(),
            b"alpha beta gamma",
            None,
        )
        .await
        .unwrap();
        let n = edit(
            serve_one(&server, "caller-a").await,
            "caller-a",
            edited.to_str().unwrap(),
            "beta",
            "BETA",
            false,
        )
        .await
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(&edited).unwrap(), b"alpha BETA gamma");
        let err = edit(
            serve_one(&server, "caller-a").await,
            "caller-a",
            edited.to_str().unwrap(),
            "missing",
            "x",
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("not found"), "{err}");

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
        let hello = probe(serve_one(&server, jade_proto::LOCAL_CALLER).await, "local")
            .await
            .unwrap();
        assert!(!hello.os.is_empty());

        let disabled = server_with(false, &[]).1;
        let hello = probe(
            serve_one(&disabled, jade_proto::LOCAL_CALLER).await,
            "local",
        )
        .await
        .unwrap();
        assert_eq!(hello.version, jade_proto::AGENT_VERSION);
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
