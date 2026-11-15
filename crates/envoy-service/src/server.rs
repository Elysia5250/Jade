//! Wire-side agent server running over any `AsyncRead + AsyncWrite` stream.

use crate::transport::{AgentHost, StoreHost};
use envoy_core::{audit, gate, part_path, resolve, set_mode, truncate, Runner};
use envoy_proto::codec::{read_framed, write_framed};
use envoy_proto::{AgentCall, AgentEvent, AgentMsg, AgentOp, AGENT_CHUNK, AGENT_VERSION};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tracing::debug;

/// How long a client has to send its first frame.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct AgentServer {
    inner: Arc<Inner>,
}

struct Inner {
    host: Arc<dyn AgentHost>,
    runner: Runner,
    /// Running invocations, versus `config.max_jobs`.
    inflight: AtomicUsize,
}

impl AgentServer {
    pub fn new(root: PathBuf, host: Arc<dyn AgentHost>) -> Self {
        Self {
            inner: Arc::new(Inner {
                host,
                runner: Runner::new(root),
                inflight: AtomicUsize::new(0),
            }),
        }
    }

    pub fn open_store(root: PathBuf) -> Result<Self> {
        let host = Arc::new(StoreHost::open(root.clone())?);
        Ok(Self::new(root, host))
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.runner.root
    }

    pub fn runner(&self) -> &Runner {
        &self.inner.runner
    }

    /// Handle one inbound invocation stream end-to-end.
    pub async fn serve_stream<S>(&self, caller_id: &str, stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        serve(self.inner.clone(), caller_id, stream).await
    }
}

async fn serve<S>(inner: Arc<Inner>, caller_id: &str, stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);

    let call: AgentCall = match tokio::time::timeout(HELLO_TIMEOUT, read_framed(&mut rd)).await {
        Ok(Ok(AgentMsg::Call(c))) => c,
        Ok(Ok(_)) => {
            reply(
                &mut wr,
                AgentEvent::Failed {
                    error: "expected a call frame".into(),
                },
            )
            .await;
            return Ok(());
        }
        Ok(Err(e)) => return Err(e.context("reading agent call")),
        Err(_) => {
            reply(
                &mut wr,
                AgentEvent::Failed {
                    error: "no call within 5s".into(),
                },
            )
            .await;
            return Ok(());
        }
    };

    if call.version != AGENT_VERSION {
        reply(
            &mut wr,
            AgentEvent::Denied {
                reason: format!(
                    "agent protocol v{} on the peer, v{} here — upgrade both devices",
                    call.version, AGENT_VERSION
                ),
            },
        )
        .await;
        return Ok(());
    }

    // Gate before anything else: an unauthorized caller learns nothing beyond refusal.
    let cfg = inner.host.config();
    if let Some(reason) = gate(&cfg, caller_id) {
        audit(&inner.runner.root, caller_id, "denied", &reason, false, 0);
        reply(&mut wr, AgentEvent::Denied { reason }).await;
        return Ok(());
    }

    let start = Instant::now();
    write_framed(
        &mut wr,
        &AgentEvent::Hello {
            version: AGENT_VERSION,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            hostname: hostname(),
        },
    )
    .await?;

    let Some(_busy) = Busy::acquire(&inner, cfg.max_jobs) else {
        let reason = format!("busy: {} agent jobs already running", cfg.max_jobs);
        audit(&inner.runner.root, caller_id, "denied", &reason, false, 0);
        reply(&mut wr, AgentEvent::Denied { reason }).await;
        return Ok(());
    };

    let (op_name, detail) = describe(&call.op);
    let outcome = match call.op {
        AgentOp::Write { path, mode } => serve_write(&mut rd, &mut wr, &path, mode).await,
        op => {
            let (tx, mut rx) = mpsc::channel::<AgentEvent>(32);
            let cancel = Arc::new(tokio::sync::Notify::new());
            let cancel2 = cancel.clone();
            let runner = inner.runner.clone();
            let pump = tokio::spawn(async move {
                let mut probe = [0u8; 1];
                loop {
                    tokio::select! {
                        recv = rx.recv() => match recv {
                            Some(ev) => {
                                if write_framed(&mut wr, &ev).await.is_err() {
                                    cancel2.notify_one();
                                    break;
                                }
                            }
                            None => break,
                        },
                        r = rd.read(&mut probe) => match r {
                            // Client closed the stream: cancel the job.
                            Ok(0) | Err(_) => {
                                cancel2.notify_one();
                                break;
                            }
                            Ok(_) => {}
                        },
                    }
                }
            });
            let out = run_op(&runner, op, &tx, &cancel).await;
            drop(tx);
            let _ = pump.await;
            out
        }
    };
    let ms = start.elapsed().as_millis() as u64;
    audit(
        &inner.runner.root,
        caller_id,
        op_name,
        &truncate(&detail, 200),
        outcome.is_ok(),
        ms,
    );
    outcome
}

/// Execute a non-writing op, emitting exactly one terminal event.
async fn run_op(
    runner: &Runner,
    op: AgentOp,
    tx: &mpsc::Sender<AgentEvent>,
    cancel: &tokio::sync::Notify,
) -> Result<()> {
    let start = Instant::now();
    match op {
        AgentOp::Probe => {
            let _ = tx.send(AgentEvent::Done { code: 0, ms: 0 }).await;
        }
        AgentOp::Exec {
            argv,
            cwd,
            env,
            timeout_s,
        } => {
            runner
                .exec_into(&argv, cwd, &env, timeout_s, tx, cancel)
                .await?;
        }
        AgentOp::Read {
            path,
            offset,
            limit,
        } => match runner.read_range(&path, offset, limit).await {
            Ok((size, mode, data)) => {
                let _ = tx.send(AgentEvent::Meta { size, mode }).await;
                for chunk in data.chunks(AGENT_CHUNK) {
                    if tx
                        .send(AgentEvent::Out {
                            fd: 0,
                            data: chunk.to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                let _ = tx
                    .send(AgentEvent::Done {
                        code: 0,
                        ms: start.elapsed().as_millis() as u64,
                    })
                    .await;
            }
            Err(e) => {
                let _ = tx
                    .send(AgentEvent::Failed {
                        error: e.to_string(),
                    })
                    .await;
            }
        },
        AgentOp::List { path } => match runner.list_dir(&path).await {
            Ok(entries) => {
                let _ = tx.send(AgentEvent::Listing { entries }).await;
                let _ = tx.send(AgentEvent::Done { code: 0, ms: 0 }).await;
            }
            Err(e) => {
                let _ = tx
                    .send(AgentEvent::Failed {
                        error: e.to_string(),
                    })
                    .await;
            }
        },
        AgentOp::Write { .. } => unreachable!("write is handled directly on the halves"),
    }
    Ok(())
}

/// Streaming write: `Data` frames until `End` commits, EOF rolls back.
async fn serve_write<S: AsyncRead + AsyncWrite + Unpin>(
    rd: &mut ReadHalf<S>,
    wr: &mut WriteHalf<S>,
    path: &str,
    mode: Option<u32>,
) -> Result<()> {
    let path = resolve(path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
    }
    let part = part_path(&path);
    let mut f = match tokio::fs::File::create(&part).await {
        Ok(f) => f,
        Err(e) => {
            reply(
                wr,
                AgentEvent::Failed {
                    error: format!("create {}: {e}", part.display()),
                },
            )
            .await;
            return Ok(());
        }
    };
    let mut bytes = 0u64;
    loop {
        match read_framed::<_, AgentMsg>(rd).await {
            Ok(AgentMsg::Data(d)) => {
                if let Err(e) = f.write_all(&d).await {
                    drop(f);
                    let _ = tokio::fs::remove_file(&part).await;
                    reply(
                        wr,
                        AgentEvent::Failed {
                            error: format!("write {}: {e}", part.display()),
                        },
                    )
                    .await;
                    return Ok(());
                }
                bytes += d.len() as u64;
            }
            Ok(AgentMsg::End) => {
                f.flush().await.ok();
                drop(f);
                if let Some(m) = mode {
                    set_mode(&part, m);
                }
                if let Err(e) = tokio::fs::rename(&part, &path).await {
                    let _ = tokio::fs::remove_file(&part).await;
                    reply(
                        wr,
                        AgentEvent::Failed {
                            error: format!("rename into {}: {e}", path.display()),
                        },
                    )
                    .await;
                    return Ok(());
                }
                reply(wr, AgentEvent::Done { code: 0, ms: 0 }).await;
                debug!(path = %path.display(), bytes, "agent wrote a file");
                return Ok(());
            }
            Ok(AgentMsg::Call(_)) | Err(_) => {
                drop(f);
                let _ = tokio::fs::remove_file(&part).await;
                return Ok(());
            }
        }
    }
}

fn describe(op: &AgentOp) -> (&'static str, String) {
    match op {
        AgentOp::Exec { argv, .. } => ("exec", argv.join(" ")),
        AgentOp::Read { path, .. } => ("read", path.clone()),
        AgentOp::Write { path, .. } => ("write", path.clone()),
        AgentOp::List { path } => ("list", path.clone()),
        AgentOp::Probe => ("probe", String::new()),
    }
}

struct Busy<'a> {
    counter: &'a AtomicUsize,
}

impl<'a> Busy<'a> {
    fn acquire(inner: &'a Inner, max: usize) -> Option<Self> {
        let cur = inner.inflight.fetch_add(1, Ordering::SeqCst);
        if cur >= max.max(1) {
            inner.inflight.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self {
            counter: &inner.inflight,
        })
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn reply<W: AsyncWrite + Unpin>(wr: &mut W, event: AgentEvent) {
    let _ = write_framed(wr, &event).await;
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_default()
}
