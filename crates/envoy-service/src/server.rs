//! The wire-side agent server: one short request per stream.
//!
//! Job execution is detached. `Submit` answers as soon as the job is in the
//! table, and `JobLog` is a *watch*, so a caller can disappear without
//! affecting the job and reattach later at its last cursor.

use crate::policy;
use crate::transport::AgentHost;
use anyhow::Result;
use envoy_core::{audit, fs, gate, job, search, truncate, Caller, Jobs};
use envoy_proto::codec::{read_framed, write_framed};
use envoy_proto::{
    Call, ClientMsg, Hello, JobId, Op, Reply, VersionMismatch, AGENT_CHUNK, AGENT_VERSION,
    WIRE_VERSION,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::Semaphore;
use tracing::debug;

tokio::task_local! {
    static REPLY_FAILED: Arc<AtomicBool>;
}

fn mark_reply_failed() {
    let _ = REPLY_FAILED.try_with(|flag| flag.store(true, Ordering::Relaxed));
}

/// How long a client has to send its first frame.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// How much log payload one `JobLog` reply carries.
const LOG_BATCH: usize = 64 * 1024;
/// Idle poll interval for a log follower (wakes are advisory, not a guarantee).
const LOG_TICK: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub struct AgentServer {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AgentServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentServer")
            .field("root", &self.inner.root)
            .field("device", &self.inner.device)
            .finish()
    }
}

struct Inner {
    host: Arc<dyn AgentHost>,
    jobs: Jobs,
    root: PathBuf,
    /// Set when this server is mounted on a fabric: the authenticated device id.
    device: Option<String>,
    streams: Arc<Semaphore>,
}

impl AgentServer {
    pub fn new(root: PathBuf, host: Arc<dyn AgentHost>) -> Self {
        let max_jobs = host.config().max_jobs;
        Self {
            inner: Arc::new(Inner {
                host,
                jobs: Jobs::open(root.clone(), max_jobs),
                root,
                device: None,
                streams: Arc::new(Semaphore::new(policy::MAX_CONCURRENT_STREAMS)),
            }),
        }
    }

    /// Same as [`AgentServer::new`], but reports the fabric device id in the
    /// handshake so callers can confirm which machine answered.
    pub fn with_device(root: PathBuf, host: Arc<dyn AgentHost>, device: Option<String>) -> Self {
        let mut server = Self::new(root, host);
        Arc::get_mut(&mut server.inner)
            .expect("freshly built server")
            .device = device;
        server
    }

    pub fn open_store(root: PathBuf) -> Result<Self> {
        let host = Arc::new(crate::transport::StoreHost::open(root.clone())?);
        Ok(Self::new(root, host))
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.root
    }

    /// The shared job table (used by embedded hosts and tests).
    pub fn jobs(&self) -> &Jobs {
        &self.inner.jobs
    }

    pub(crate) fn host(&self) -> &dyn AgentHost {
        self.inner.host.as_ref()
    }

    pub(crate) fn device(&self) -> Option<&str> {
        self.inner.device.as_deref()
    }

    /// Handle one request stream end-to-end.
    pub async fn serve_stream<S>(&self, caller: Caller, stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        serve(self.inner.clone(), caller, stream).await
    }
}

async fn serve<S>(inner: Arc<Inner>, caller: Caller, stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);

    let Ok(_stream_permit) = inner.streams.clone().try_acquire_owned() else {
        reply(
            &mut wr,
            Reply::Denied {
                reason: "too many concurrent agent streams".into(),
            },
        )
        .await?;
        return Ok(());
    };

    let first: ClientMsg = match tokio::time::timeout(HELLO_TIMEOUT, read_framed(&mut rd)).await {
        Ok(Ok(msg)) => msg,
        Ok(Err(e)) => {
            if let Some(mismatch) = e.downcast_ref::<VersionMismatch>() {
                reply(
                    &mut wr,
                    Reply::VersionMismatch {
                        server_version: mismatch.local_version,
                        client_version: mismatch.peer_version,
                    },
                )
                .await?;
                return Ok(());
            }
            return Err(e.context("reading the agent call"));
        }
        Err(_) => {
            reply(
                &mut wr,
                Reply::Failed {
                    error: "no call within 5s".into(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    let call: Call = match first {
        ClientMsg::Call(call) => call,
        _ => {
            reply(
                &mut wr,
                Reply::Failed {
                    error: "expected a call frame".into(),
                },
            )
            .await?;
            return Ok(());
        }
    };

    if call.version != AGENT_VERSION {
        reply(
            &mut wr,
            Reply::VersionMismatch {
                server_version: WIRE_VERSION,
                client_version: u16::from(call.version),
            },
        )
        .await?;
        return Ok(());
    }

    // Gate before anything else: an unauthorized caller learns nothing beyond refusal.
    let cfg = inner.host.config();
    let caller_label = caller.as_str().to_string();
    if let Some(reason) = gate(&cfg, &caller) {
        audit(&inner.root, &caller_label, "denied", &reason, false, 0);
        reply(&mut wr, Reply::Denied { reason }).await?;
        return Ok(());
    }

    let op = if !caller.is_local() {
        match policy::guard_remote_op(call.op, &inner.root) {
            Ok(guarded) => guarded,
            Err(reason) => {
                audit(&inner.root, &caller_label, "denied", &reason, false, 0);
                reply(&mut wr, Reply::Denied { reason }).await?;
                return Ok(());
            }
        }
    } else {
        call.op
    };

    // `agent.json` is re-read per request, so a changed `max_jobs` applies
    // without restarting the daemon.
    inner.jobs.set_max_jobs(cfg.max_jobs);

    let hello = Reply::Hello(Hello {
        version: AGENT_VERSION,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        hostname: hostname(),
        device: inner.device.clone(),
    });
    if write_framed(&mut wr, &hello).await.is_err() {
        return Ok(());
    }

    let start = Instant::now();
    let (op_name, detail) = describe(&op);
    let failed = Arc::new(AtomicBool::new(false));
    let outcome = REPLY_FAILED
        .scope(failed.clone(), dispatch(&inner, &caller, op, &mut rd, &mut wr))
        .await;
    let ms = start.elapsed().as_millis() as u64;
    // `dispatch` returns Ok after a `Reply::Failed` / `Reply::Denied`. The
    // audit flag follows the reply the peer actually got, and a failed send.
    let ok = outcome.is_ok() && !failed.load(Ordering::Relaxed);
    audit(
        &inner.root,
        &caller_label,
        op_name,
        &truncate(&detail, 200),
        ok,
        ms,
    );
    outcome
}

async fn dispatch<S>(
    inner: &Inner,
    caller: &Caller,
    op: Op,
    rd: &mut ReadHalf<S>,
    wr: &mut WriteHalf<S>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let caller_str = caller.as_str();
    match op {
        Op::Probe => reply(wr, Reply::Done { code: 0, ms: 0 }).await?,
        Op::Submit(req) => match inner.jobs.submit(req, caller_str) {
            Ok(job) => reply(wr, Reply::Accepted { job }).await?,
            Err(e) => {
                reply(
                    wr,
                    Reply::Failed {
                        error: format!("{e:#}"),
                    },
                )
                .await?;
            }
        },
        Op::Jobs => {
            reply(
                wr,
                Reply::Jobs {
                    jobs: inner.jobs.list(),
                },
            )
            .await?;
        }
        Op::JobStatus { id } => match inner.jobs.get(id) {
            Some(job) => reply(wr, Reply::Job { job }).await?,
            None => {
                reply(
                    wr,
                    Reply::Failed {
                        error: format!("no such job: {id}"),
                    },
                )
                .await?;
            }
        },
        Op::JobCancel { id } => match inner.jobs.cancel(id) {
            Ok(()) => reply(wr, Reply::Done { code: 0, ms: 0 }).await?,
            Err(e) => {
                reply(
                    wr,
                    Reply::Failed {
                        error: format!("{e:#}"),
                    },
                )
                .await?;
            }
        },
        Op::JobLog { id, offset, follow } => follow_log(inner, id, offset, follow, wr).await?,
        Op::Read {
            path,
            offset,
            limit,
        } => read_op(&path, offset, limit, wr).await?,
        Op::List { path } => list_op(&path, wr).await?,
        Op::Edit {
            path,
            old,
            new,
            replace_all,
        } => match fs::edit_file(&path, &old, &new, replace_all).await {
            Ok(replacements) => reply(wr, Reply::Edited { replacements }).await?,
            Err(e) => {
                reply(
                    wr,
                    Reply::Failed {
                        error: format!("{e:#}"),
                    },
                )
                .await?;
            }
        },
        Op::Write { path, mode } => serve_write(rd, wr, &path, mode).await?,
        Op::Glob {
            root,
            pattern,
            max_results,
        } => glob_op(&root, &pattern, max_results, wr).await?,
        Op::Grep {
            root,
            pattern,
            include,
            max_results,
        } => grep_op(&root, &pattern, include.as_deref(), max_results, wr).await?,
    }
    Ok(())
}

/// Stream a job's log from `offset`, optionally following until it finishes.
///
/// `finished` is set only on the last frame. A fast exit that wrote more than
/// one batch used to return after the first 64 KiB and drop the tail.
async fn follow_log<W: AsyncWrite + Unpin>(
    inner: &Inner,
    id: JobId,
    mut offset: u64,
    follow: bool,
    wr: &mut W,
) -> Result<()> {
    let Some(path) = inner.jobs.log_path(id) else {
        reply(
            wr,
            Reply::Failed {
                error: format!("no such job: {id}"),
            },
        )
        .await?;
        return Ok(());
    };
    let wake = inner.jobs.wake();
    loop {
        let (records, next) = match job::read_available(&path, offset, LOG_BATCH) {
            Ok(v) => v,
            Err(e) => {
                reply(
                    wr,
                    Reply::Failed {
                        error: format!("reading job log: {e}"),
                    },
                )
                .await?;
                return Ok(());
            }
        };
        let progressed = !records.is_empty();
        offset = next;
        let Some(snapshot) = inner.jobs.get(id) else {
            reply(
                wr,
                Reply::Failed {
                    error: format!("job {id} is no longer available"),
                },
            )
            .await?;
            return Ok(());
        };
        let done = snapshot.state.exit_code();
        let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(offset);
        // Done, and either the cursor is at EOF or this read made no progress
        // (a partial trailing record will not grow after the writer exits).
        let complete = done.is_some() && (offset >= file_len || !progressed);
        if progressed || complete || !follow {
            reply(
                wr,
                Reply::Log {
                    records,
                    next: offset,
                    log_bytes: file_len,
                    finished: if complete { done } else { None },
                },
            )
            .await?;
        }
        if complete || !follow {
            return Ok(());
        }
        if done.is_some() {
            continue;
        }
        if !progressed {
            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(LOG_TICK) => {}
            }
        }
    }
}

async fn read_op<W: AsyncWrite + Unpin>(
    path: &str,
    offset: u64,
    limit: u64,
    wr: &mut W,
) -> Result<()> {
    let start = Instant::now();
    match fs::read_range(path, offset, limit).await {
        Ok((size, mode, data)) => {
            reply(wr, Reply::Meta { size, mode }).await?;
            for chunk in data.chunks(AGENT_CHUNK) {
                reply(
                    wr,
                    Reply::Data {
                        data: chunk.to_vec(),
                    },
                )
                .await?;
            }
            reply(
                wr,
                Reply::Done {
                    code: 0,
                    ms: start.elapsed().as_millis() as u64,
                },
            )
            .await?;
        }
        Err(e) => {
            reply(
                wr,
                Reply::Failed {
                    error: e.to_string(),
                },
            )
            .await?;
        }
    }
    Ok(())
}

async fn list_op<W: AsyncWrite + Unpin>(path: &str, wr: &mut W) -> Result<()> {
    match fs::list_dir(path).await {
        Ok(entries) => {
            reply(wr, Reply::Listing { entries }).await?;
            reply(wr, Reply::Done { code: 0, ms: 0 }).await?;
        }
        Err(e) => {
            reply(
                wr,
                Reply::Failed {
                    error: e.to_string(),
                },
            )
            .await?;
        }
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
    let path = fs::resolve(path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
    }
    let part = fs::part_path(&path);
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(mode.unwrap_or(0o600));
    let mut f = match opts.open(&part).await {
        Ok(f) => f,
        Err(e) => {
            reply(
                wr,
                Reply::Failed {
                    error: format!("create {}: {e}", part.display()),
                },
            )
            .await?;
            return Ok(());
        }
    };
    let mut bytes = 0u64;
    loop {
        match tokio::time::timeout(policy::WRITE_FRAME_TIMEOUT, read_framed::<_, ClientMsg>(rd))
            .await
        {
            Ok(Ok(ClientMsg::Data(d))) => {
                if bytes + (d.len() as u64) > policy::MAX_WRITE_BYTES {
                    drop(f);
                    let _ = tokio::fs::remove_file(&part).await;
                    reply(
                        wr,
                        Reply::Failed {
                            error: format!(
                                "write exceeds {} byte limit",
                                policy::MAX_WRITE_BYTES
                            ),
                        },
                    )
                    .await?;
                    return Ok(());
                }
                if let Err(e) = f.write_all(&d).await {
                    drop(f);
                    let _ = tokio::fs::remove_file(&part).await;
                    reply(
                        wr,
                        Reply::Failed {
                            error: format!("write {}: {e}", part.display()),
                        },
                    )
                    .await?;
                    return Ok(());
                }
                bytes += d.len() as u64;
            }
            Ok(Ok(ClientMsg::End)) => {
                f.flush().await.ok();
                drop(f);
                if let Some(m) = mode {
                    fs::set_mode(&part, m);
                }
                if let Err(e) = tokio::fs::rename(&part, &path).await {
                    let _ = tokio::fs::remove_file(&part).await;
                    reply(
                        wr,
                        Reply::Failed {
                            error: format!("rename into {}: {e}", path.display()),
                        },
                    )
                    .await?;
                    return Ok(());
                }
                reply(wr, Reply::Done { code: 0, ms: 0 }).await?;
                debug!(path = %path.display(), bytes, "agent wrote a file");
                return Ok(());
            }
            Ok(Ok(ClientMsg::Call(_))) | Ok(Err(_)) => {
                drop(f);
                let _ = tokio::fs::remove_file(&part).await;
                return Ok(());
            }
            Err(_) => {
                drop(f);
                let _ = tokio::fs::remove_file(&part).await;
                reply(
                    wr,
                    Reply::Failed {
                        error: "write stream timed out".into(),
                    },
                )
                .await?;
                return Ok(());
            }
        }
    }
}

fn describe(op: &Op) -> (&'static str, String) {
    let detail = match op {
        Op::Probe => String::new(),
        Op::Submit(req) => req.spec.display(),
        Op::Jobs => String::new(),
        Op::JobStatus { id } => format!("#{id}"),
        Op::JobLog { id, .. } => format!("#{id}"),
        Op::JobCancel { id } => format!("#{id}"),
        Op::Read { path, .. } => path.clone(),
        Op::Write { path, .. } => path.clone(),
        Op::List { path } => path.clone(),
        Op::Edit { path, .. } => path.clone(),
        Op::Glob { root, pattern, .. } => format!("{root}:{pattern}"),
        Op::Grep { root, pattern, .. } => format!("{root}:{pattern}"),
    };
    (op.name(), detail)
}

async fn glob_op<W: AsyncWrite + Unpin>(
    root: &str,
    pattern: &str,
    max_results: u32,
    wr: &mut W,
) -> Result<()> {
    match search::glob_files(root, pattern, max_results).await {
        Ok(paths) => {
            reply(wr, Reply::GlobMatches { paths }).await?;
            reply(wr, Reply::Done { code: 0, ms: 0 }).await?;
        }
        Err(e) => {
            reply(
                wr,
                Reply::Failed {
                    error: format!("{e:#}"),
                },
            )
            .await?;
        }
    }
    Ok(())
}

async fn grep_op<W: AsyncWrite + Unpin>(
    root: &str,
    pattern: &str,
    include: Option<&str>,
    max_results: u32,
    wr: &mut W,
) -> Result<()> {
    match search::grep_files(root, pattern, include, max_results).await {
        Ok(matches) => {
            reply(wr, Reply::GrepMatches { matches }).await?;
            reply(wr, Reply::Done { code: 0, ms: 0 }).await?;
        }
        Err(e) => {
            reply(
                wr,
                Reply::Failed {
                    error: format!("{e:#}"),
                },
            )
            .await?;
        }
    }
    Ok(())
}

/// Send one reply. `Failed` / `Denied` clear the audit success flag. A write
/// error is logged and aborts the request instead of being ignored.
async fn reply<W: AsyncWrite + Unpin>(wr: &mut W, event: Reply) -> Result<()> {
    if matches!(event, Reply::Failed { .. } | Reply::Denied { .. }) {
        mark_reply_failed();
    }
    if let Err(e) = send(wr, event).await {
        debug!(error = %e, "reply send failed");
        mark_reply_failed();
        return Err(anyhow::anyhow!("reply send failed: {e}"));
    }
    Ok(())
}

async fn send<W: AsyncWrite + Unpin>(wr: &mut W, event: Reply) -> std::io::Result<()> {
    write_framed(wr, &event)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
}

fn hostname() -> String {
    envoy_core::system_hostname()
}
