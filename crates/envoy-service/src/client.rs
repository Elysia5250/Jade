//! Wire-side agent client over any `AsyncRead + AsyncWrite` stream or [`AgentDialer`].

use crate::transport::{AgentDialer, BoxStream};
use envoy_proto::codec::{read_framed, write_framed};
use envoy_proto::{
    AgentCall, AgentEvent, AgentMsg, AgentOp, Entry, AGENT_CHUNK, AGENT_SERVICE_ID, AGENT_VERSION,
    READ_LIMIT,
};
use anyhow::{bail, Result};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

/// Facts a peer reports in the handshake.
#[derive(Debug, Clone)]
pub struct Hello {
    pub os: String,
    pub arch: String,
    pub hostname: String,
}

/// Send an [`AgentOp`] call over `stream` and consume the handshake reply.
pub async fn open_call<S>(mut stream: S, peer_label: &str, op: AgentOp) -> Result<(S, Hello)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_framed(
        &mut stream,
        &AgentMsg::Call(AgentCall {
            version: AGENT_VERSION,
            op,
        }),
    )
    .await?;
    let first: AgentEvent =
        match tokio::time::timeout(Duration::from_secs(5), read_framed(&mut stream)).await {
            Ok(Ok(ev)) => ev,
            Ok(Err(e)) => {
                bail!("{peer_label} closed the agent stream during the handshake ({e})")
            }
            Err(_) => bail!(
                "{peer_label} did not answer the agent handshake — is the agent service mounted there?"
            ),
        };
    match first {
        AgentEvent::Hello {
            os,
            arch,
            hostname,
            ..
        } => Ok((
            stream,
            Hello {
                os,
                arch,
                hostname,
            },
        )),
        AgentEvent::Denied { reason } => bail!("{peer_label} refused: {reason}"),
        AgentEvent::Failed { error } => bail!("{peer_label} failed: {error}"),
        other => bail!("{peer_label} sent an unexpected handshake reply: {other:?}"),
    }
}

pub async fn probe<S>(stream: S, peer_label: &str) -> Result<Hello>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (_s, hello) = open_call(stream, peer_label, AgentOp::Probe).await?;
    Ok(hello)
}

/// Forward every event of a running call into `tx`.
pub async fn pump<S>(mut stream: S, tx: tokio::sync::mpsc::Sender<AgentEvent>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            r = read_framed::<_, AgentEvent>(&mut stream) => match r {
                Ok(ev) => {
                    let done = matches!(ev, AgentEvent::Done { .. } | AgentEvent::Failed { .. });
                    if tx.send(ev).await.is_err() || done {
                        return;
                    }
                }
                Err(_) => return,
            },
            _ = tx.closed() => return,
        }
    }
}

/// Streaming read: `Meta` on open, then chunks until `Done`.
pub struct ReadStream<S> {
    stream: S,
    pub size: u64,
    pub mode: u32,
    done: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin> ReadStream<S> {
    pub async fn open(
        stream: S,
        peer_label: &str,
        path: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Self> {
        let (stream, _hello) = open_call(
            stream,
            peer_label,
            AgentOp::Read {
                path: path.to_string(),
                offset,
                limit,
            },
        )
        .await?;
        let mut this = Self {
            stream,
            size: 0,
            mode: 0,
            done: false,
        };
        match this.event().await? {
            AgentEvent::Meta { size, mode } => {
                this.size = size;
                this.mode = mode;
                Ok(this)
            }
            AgentEvent::Failed { error } => bail!(error),
            other => bail!("expected file metadata, got {other:?}"),
        }
    }

    async fn event(&mut self) -> Result<AgentEvent> {
        read_framed(&mut self.stream).await
    }

    /// Next chunk, or `None` at end of file.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>> {
        if self.done {
            return Ok(None);
        }
        loop {
            match self.event().await? {
                AgentEvent::Out { data, .. } => return Ok(Some(data)),
                AgentEvent::Done { .. } => {
                    self.done = true;
                    return Ok(None);
                }
                AgentEvent::Failed { error } => bail!(error),
                _ => {}
            }
        }
    }
}

/// Streaming write: open, `send` chunks, `finish` commits (or dropping the
/// handle rolls the remote `.part` back).
pub struct WriteStream<S> {
    stream: S,
    sent: u64,
}

impl<S: AsyncRead + AsyncWrite + Unpin> WriteStream<S> {
    pub async fn open(
        stream: S,
        peer_label: &str,
        path: &str,
        mode: Option<u32>,
    ) -> Result<Self> {
        let (stream, _hello) = open_call(
            stream,
            peer_label,
            AgentOp::Write {
                path: path.to_string(),
                mode,
            },
        )
        .await?;
        Ok(Self { stream, sent: 0 })
    }

    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(AGENT_CHUNK) {
            write_framed(&mut self.stream, &AgentMsg::Data(chunk.to_vec())).await?;
            self.sent += chunk.len() as u64;
        }
        Ok(())
    }

    pub async fn finish(mut self) -> Result<u64> {
        write_framed(&mut self.stream, &AgentMsg::End).await?;
        match read_framed::<_, AgentEvent>(&mut self.stream).await? {
            AgentEvent::Done { .. } => Ok(self.sent),
            AgentEvent::Failed { error } => bail!(error),
            other => bail!("unexpected reply to a write commit: {other:?}"),
        }
    }
}

/// Whole-file read (bounded by `READ_LIMIT`).
pub async fn read<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    peer_label: &str,
    path: &str,
    offset: u64,
    limit: u64,
) -> Result<(u64, u32, Vec<u8>)> {
    let mut s = ReadStream::open(stream, peer_label, path, offset, limit).await?;
    let mut data = Vec::new();
    while let Some(chunk) = s.next().await? {
        if data.len() as u64 + chunk.len() as u64 > READ_LIMIT + 1024 {
            bail!("peer sent more than the read limit");
        }
        data.extend_from_slice(&chunk);
    }
    Ok((s.size, s.mode, data))
}

pub async fn write<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    peer_label: &str,
    path: &str,
    data: &[u8],
    mode: Option<u32>,
) -> Result<u64> {
    let mut w = WriteStream::open(stream, peer_label, path, mode).await?;
    w.send(data).await?;
    w.finish().await
}

pub async fn list<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    peer_label: &str,
    path: &str,
) -> Result<Vec<Entry>> {
    let (mut stream, _hello) = open_call(
        stream,
        peer_label,
        AgentOp::List {
            path: path.to_string(),
        },
    )
    .await?;
    loop {
        match read_framed::<_, AgentEvent>(&mut stream).await? {
            AgentEvent::Listing { entries } => return Ok(entries),
            AgentEvent::Done { .. } => return Ok(Vec::new()),
            AgentEvent::Failed { error } => bail!(error),
            _ => {}
        }
    }
}

/// High-level client backed by any [`AgentDialer`] adapter.
pub struct AgentClient<D> {
    dialer: D,
}

impl<D: AgentDialer> AgentClient<D> {
    pub fn new(dialer: D) -> Self {
        Self { dialer }
    }

    pub async fn open_call(&self, peer: &str, op: AgentOp) -> Result<(BoxStream, Hello)> {
        let stream = self.dialer.open_stream(peer, AGENT_SERVICE_ID).await?;
        open_call(stream, peer, op).await
    }

    pub async fn probe(&self, peer: &str) -> Result<Hello> {
        let stream = self.dialer.open_stream(peer, AGENT_SERVICE_ID).await?;
        probe(stream, peer).await
    }

    pub async fn read(
        &self,
        peer: &str,
        path: &str,
        offset: u64,
        limit: u64,
    ) -> Result<(u64, u32, Vec<u8>)> {
        let stream = self.dialer.open_stream(peer, AGENT_SERVICE_ID).await?;
        read(stream, peer, path, offset, limit).await
    }

    pub async fn write(
        &self,
        peer: &str,
        path: &str,
        data: &[u8],
        mode: Option<u32>,
    ) -> Result<u64> {
        let stream = self.dialer.open_stream(peer, AGENT_SERVICE_ID).await?;
        write(stream, peer, path, data, mode).await
    }

    pub async fn list(&self, peer: &str, path: &str) -> Result<Vec<Entry>> {
        let stream = self.dialer.open_stream(peer, AGENT_SERVICE_ID).await?;
        list(stream, peer, path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::AgentServer;
    use crate::transport::StaticHost;
    use envoy_core::AgentConfig;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("agent-svc-{tag}-{n}"));
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

    #[tokio::test]
    async fn wire_probe_exec_write_read_list_over_duplex() {
        let (dir, server) = server_with(true, &["caller-a"]);

        // 1. Probe
        let (c1, s1) = tokio::io::duplex(64 * 1024);
        let srv = server.clone();
        let h = tokio::spawn(async move { srv.serve_stream("caller-a", s1).await });
        let hello = probe(c1, "caller-a").await.unwrap();
        assert!(!hello.os.is_empty());
        h.await.unwrap().unwrap();

        // 2. Write + Read + List
        let target = dir.join("sub/hello.txt");
        let (c2, s2) = tokio::io::duplex(64 * 1024);
        let srv = server.clone();
        let h = tokio::spawn(async move { srv.serve_stream("caller-a", s2).await });
        let written = write(
            c2,
            "caller-a",
            target.to_str().unwrap(),
            b"hello wire world",
            Some(0o640),
        )
        .await
        .unwrap();
        assert_eq!(written, 16);
        h.await.unwrap().unwrap();

        let (c3, s3) = tokio::io::duplex(64 * 1024);
        let srv = server.clone();
        let h = tokio::spawn(async move { srv.serve_stream("caller-a", s3).await });
        let (size, _, content) = read(c3, "caller-a", target.to_str().unwrap(), 6, 4)
            .await
            .unwrap();
        assert_eq!(size, 16);
        assert_eq!(content, b"wire");
        h.await.unwrap().unwrap();

        let (c4, s4) = tokio::io::duplex(64 * 1024);
        let srv = server.clone();
        let h = tokio::spawn(async move { srv.serve_stream("caller-a", s4).await });
        let entries = list(c4, "caller-a", dir.join("sub").to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "hello.txt");
        h.await.unwrap().unwrap();

        // 3. Exec via pump
        let (c5, s5) = tokio::io::duplex(64 * 1024);
        let srv = server.clone();
        let h = tokio::spawn(async move { srv.serve_stream("caller-a", s5).await });
        let (ch, _) = open_call(
            c5,
            "caller-a",
            AgentOp::Exec {
                argv: vec!["sh".into(), "-c".into(), "echo ping".into()],
                cwd: None,
                env: vec![],
                timeout_s: 10,
            },
        )
        .await
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let p = tokio::spawn(pump(ch, tx));
        let mut saw_ping = false;
        let mut exit_code = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::Out { fd: 0, data } => {
                    if String::from_utf8_lossy(&data).contains("ping") {
                        saw_ping = true;
                    }
                }
                AgentEvent::Done { code, .. } => exit_code = Some(code),
                _ => {}
            }
        }
        assert!(saw_ping);
        assert_eq!(exit_code, Some(0));
        p.await.unwrap();
        h.await.unwrap().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn wire_gate_refuses_unauthorized_caller_and_write_drop_rolls_back() {
        let (dir, server) = server_with(true, &["caller-a"]);

        // Denied caller
        let (c1, s1) = tokio::io::duplex(64 * 1024);
        let srv = server.clone();
        let h = tokio::spawn(async move { srv.serve_stream("intruder", s1).await });
        let err = probe(c1, "intruder").await.unwrap_err().to_string();
        assert!(err.contains("refused"), "{err}");
        h.await.unwrap().unwrap();

        // Write dropped before `finish()` leaves destination untouched and removes `.part`
        let target = dir.join("aborted.txt");
        let (c2, s2) = tokio::io::duplex(64 * 1024);
        let srv = server.clone();
        let h = tokio::spawn(async move { srv.serve_stream("caller-a", s2).await });
        let mut ws = WriteStream::open(c2, "caller-a", target.to_str().unwrap(), None)
            .await
            .unwrap();
        ws.send(b"partial").await.unwrap();
        drop(ws);
        h.await.unwrap().unwrap();
        assert!(!target.exists());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".part-"))
            .collect();
        assert!(leftovers.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
