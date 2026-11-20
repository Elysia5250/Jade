//! Detached command execution.
//!
//! `run_job` is what a job supervisor awaits: it spawns the child in its own
//! process group, tees stdout/stderr into the job log, and maps exit, timeout
//! and cancellation onto a [`JobResult`]. It never talks to a client — that is
//! the whole point of the interaction/execution split.

use crate::log::LogWriter;
use crate::sandbox;
use envoy_proto::{JobResult, SandboxPolicy, AGENT_CHUNK, DEFAULT_EXEC_TIMEOUT, MAX_EXEC_TIMEOUT};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, Notify};
use tracing::{debug, warn};

/// Everything needed to start one process.
#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub timeout_s: u64,
    pub sandbox: SandboxPolicy,
}

trait AsyncReadUnpin: tokio::io::AsyncRead + Unpin {}
impl<T: tokio::io::AsyncRead + Unpin> AsyncReadUnpin for T {}

/// Run `spec` to completion, feeding output into `log`.
///
/// * `pid_slot` publishes the process-group leader as soon as it exists, so a
///   cancel from another task has something to signal.
/// * `cancel` fires when a caller cancelled the job.
pub async fn run_job(
    spec: ExecSpec,
    mut log: LogWriter,
    pid_slot: Arc<AtomicU32>,
    cancel: Arc<Notify>,
) -> JobResult {
    let start = Instant::now();
    if spec.argv.is_empty() {
        return JobResult::FailedToSpawn {
            error: "empty argv".into(),
        };
    }

    if !matches!(
        spec.sandbox,
        SandboxPolicy::Off | SandboxPolicy::DangerFullAccess
    ) {
        for (k, v) in &spec.env {
            if sandbox::is_dangerous_env_var(k) || v.contains('\0') {
                return JobResult::FailedToSpawn {
                    error: format!("refusing dangerous env var `{k}` under sandbox"),
                };
            }
        }
    }

    let prepared = match sandbox::prepare_command_with_env(
        &spec.argv,
        &spec.cwd,
        &spec.sandbox,
        &spec.env,
    ) {
        Ok(p) => p,
        Err(e) => {
            return JobResult::FailedToSpawn {
                error: format!("sandbox setup failed: {e:#}"),
            }
        }
    };
    // Declared before the process guard so the directory is removed after the
    // child has been reaped or killed.
    let _scratch = sandbox::ScratchGuard(prepared.scratch_dir.clone());

    let mut cmd = tokio::process::Command::new(&prepared.argv[0]);
    cmd.args(&prepared.argv[1..]);
    let effective_cwd = prepared.cwd_override.as_deref().unwrap_or(&spec.cwd);
    if !effective_cwd.is_empty() {
        cmd.current_dir(effective_cwd);
    }
    // Do not inherit the daemon's environment wholesale: a token in the
    // service's env must not leak into every remote command.
    cmd.env_clear();
    for (k, v) in base_env() {
        cmd.env(k, v);
    }
    if prepared.landlock.is_some() {
        if let Some(ref scratch) = prepared.scratch_dir {
            let s = scratch.display().to_string();
            cmd.env("TMPDIR", &s);
            cmd.env("TMP", &s);
            cmd.env("TEMP", &s);
        }
    }
    if prepared.apply_env_in_child {
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own group: a shell's grandchildren die with it.
        cmd.as_std_mut().process_group(0);
        if let Some(plan) = prepared.landlock {
            unsafe {
                cmd.as_std_mut()
                    .pre_exec(move || sandbox::apply_landlock_in_child(&plan));
            }
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return JobResult::FailedToSpawn {
                error: format!("spawn {}: {e}", prepared.argv[0]),
            }
        }
    };
    let pid = child.id().unwrap_or(0);
    pid_slot.store(pid, Ordering::SeqCst);
    // Backstop for every cancellation path, including this future being
    // dropped mid-await.
    let mut guard = KillOnDrop { pid, armed: true };

    let (out_tx, mut out_rx) = mpsc::channel::<(u8, Vec<u8>)>(64);
    let mut readers = Vec::new();
    for (fd, pipe) in [
        (
            0u8,
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn AsyncReadUnpin + Send>),
        ),
        (
            1u8,
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn AsyncReadUnpin + Send>),
        ),
    ] {
        let Some(mut pipe) = pipe else { continue };
        let out_tx = out_tx.clone();
        readers.push(tokio::spawn(async move {
            let mut buf = vec![0u8; AGENT_CHUNK];
            loop {
                match pipe.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out_tx.send((fd, buf[..n].to_vec())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }));
    }
    drop(out_tx);

    let timeout_s = match spec.timeout_s {
        0 => DEFAULT_EXEC_TIMEOUT,
        n => n.min(MAX_EXEC_TIMEOUT),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_s);
    let mut timed_out = false;

    // Cancel and the deadline outrank output. A biased poll that prefers
    // `out_rx` lets `yes` starve both the timeout and an explicit cancel.
    let status = loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => break None,
            _ = tokio::time::sleep_until(deadline) => {
                timed_out = true;
                break None;
            }
            st = child.wait() => break Some(st),
            Some((fd, data)) = out_rx.recv() => {
                log.record(fd, &data);
                if tokio::time::Instant::now() >= deadline {
                    timed_out = true;
                    break None;
                }
            }
        }
    };

    // Normal exit: drain whatever the pipes still hold before reporting.
    // One deadline for the whole drain — a grandchild that inherited the
    // pipe and writes slowly must not reset the timer forever.
    if let Some(status) = status {
        drain_output(&mut out_rx, &mut log).await;
        stop_readers(readers);
        guard.armed = false;
        debug!(
            pid,
            ms = start.elapsed().as_millis() as u64,
            "job process finished"
        );
        return match status {
            Ok(st) => match st.code() {
                Some(0) => JobResult::Success,
                Some(code) => JobResult::Failed { code },
                None => JobResult::Errored {
                    error: "terminated by a signal".into(),
                },
            },
            Err(e) => {
                warn!(pid, error = %e, "waiting for the job process failed");
                JobResult::Errored {
                    error: format!("wait: {e}"),
                }
            }
        };
    }

    // Killed: cancel or timeout. Kill the whole group and reap.
    guard.kill();
    guard.armed = false;
    let _ = child.wait().await;
    drain_output(&mut out_rx, &mut log).await;
    stop_readers(readers);
    if timed_out {
        JobResult::TimedOut
    } else {
        JobResult::Killed
    }
}

/// How long a finished (or killed) job may spend flushing pipe bytes.
const DRAIN_BUDGET: Duration = Duration::from_millis(200);

/// Read leftover pipe bytes until `DRAIN_BUDGET` elapses or the channel closes.
///
/// The deadline is fixed outside the loop. A per-message timeout never ends
/// while a grandchild writes faster than that timeout.
async fn drain_output(out_rx: &mut mpsc::Receiver<(u8, Vec<u8>)>, log: &mut LogWriter) {
    let deadline = tokio::time::Instant::now() + DRAIN_BUDGET;
    while let Ok(Some((fd, data))) = tokio::time::timeout_at(deadline, out_rx.recv()).await {
        log.record(fd, &data);
    }
}

fn stop_readers(readers: Vec<tokio::task::JoinHandle<()>>) {
    for reader in readers {
        reader.abort();
    }
}

/// A child whose group is killed if this guard drops while armed.
struct KillOnDrop {
    pid: u32,
    armed: bool,
}

impl KillOnDrop {
    fn kill(&self) {
        kill_group(self.pid);
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if self.armed {
            kill_group(self.pid);
        }
    }
}

/// Kill a whole process group by its leader pid.
pub fn kill_group(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn base_env() -> Vec<(String, String)> {
    const KEEP: &[&str] = &[
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "TMPDIR",
    ];
    KEEP.iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{read_from, LogWriter};
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-exec-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn spec(argv: Vec<&str>, timeout_s: u64) -> ExecSpec {
        ExecSpec {
            argv: argv.into_iter().map(str::to_string).collect(),
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            env: Vec::new(),
            timeout_s,
            sandbox: SandboxPolicy::Off,
        }
    }

    #[tokio::test]
    async fn streams_output_and_reports_exit() {
        let dir = tmpdir("exit");
        let path = dir.join("1.log");
        let wake = Arc::new(Notify::new());
        let log = LogWriter::create(path.clone(), wake).unwrap();
        let pid = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(Notify::new());
        let result = run_job(
            spec(vec!["sh", "-c", "echo out; echo err >&2; exit 3"], 10),
            log,
            pid.clone(),
            cancel,
        )
        .await;
        assert_eq!(result, JobResult::Failed { code: 3 });
        assert!(pid.load(Ordering::SeqCst) > 0);
        let (recs, _) = read_from(&path, 0, 4096).unwrap();
        let out: String = recs
            .iter()
            .filter(|r| r.fd == 0)
            .map(|r| String::from_utf8_lossy(&r.data).to_string())
            .collect();
        let err: String = recs
            .iter()
            .filter(|r| r.fd == 1)
            .map(|r| String::from_utf8_lossy(&r.data).to_string())
            .collect();
        assert!(out.contains("out"), "{out}");
        assert!(err.contains("err"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_group() {
        let dir = tmpdir("timeout");
        let marker = dir.join("late");
        let log = LogWriter::create(dir.join("1.log"), Arc::new(Notify::new())).unwrap();
        let cancel = Arc::new(Notify::new());
        let script = format!("sleep 2; touch {}", marker.display());
        let result = run_job(
            spec(vec!["sh", "-c", &script], 1),
            log,
            Arc::new(AtomicU32::new(0)),
            cancel,
        )
        .await;
        assert_eq!(result, JobResult::TimedOut);
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!marker.exists(), "the group survived the timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancel_kills_a_running_job() {
        let dir = tmpdir("cancel");
        let log = LogWriter::create(dir.join("1.log"), Arc::new(Notify::new())).unwrap();
        let cancel = Arc::new(Notify::new());
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            c2.notify_one();
        });
        let result = run_job(
            spec(vec!["sh", "-c", "sleep 30"], 60),
            log,
            Arc::new(AtomicU32::new(0)),
            cancel,
        )
        .await;
        assert_eq!(result, JobResult::Killed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_missing_program_is_reported_not_fatal() {
        let dir = tmpdir("spawn");
        let log = LogWriter::create(dir.join("1.log"), Arc::new(Notify::new())).unwrap();
        let result = run_job(
            spec(vec!["definitely-not-a-real-binary-xyz"], 5),
            log,
            Arc::new(AtomicU32::new(0)),
            Arc::new(Notify::new()),
        )
        .await;
        assert!(
            matches!(result, JobResult::FailedToSpawn { .. }),
            "{result:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn workspace_write_without_a_backend_fails_closed() {
        if !matches!(sandbox::probe_backend(), sandbox::SandboxBackend::None) {
            return;
        }
        let dir = tmpdir("nosandbox");
        let path = dir.join("1.log");
        let log = LogWriter::create(path.clone(), Arc::new(Notify::new())).unwrap();
        let mut job = spec(vec!["sh", "-c", "echo sandbox-ok"], 10);
        job.sandbox = SandboxPolicy::WorkspaceWrite {
            cwd: dir.display().to_string(),
            extra_rw: vec![],
            allow_net: false,
        };
        let result = run_job(
            job,
            log,
            Arc::new(AtomicU32::new(0)),
            Arc::new(Notify::new()),
        )
        .await;
        assert!(
            matches!(result, JobResult::FailedToSpawn { .. }),
            "{result:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sandbox_allows_cwd_write_while_protecting_git_env_and_outside_paths() {
        if !sandbox::capabilities().enforces_workspace_boundary() {
            return;
        }
        let work = tmpdir("sb-work");
        let outside = tmpdir("sb-outside");
        std::fs::create_dir_all(work.join(".git")).unwrap();
        std::fs::write(work.join(".env"), "TOKEN=secret_val\n").unwrap();
        let log_dir = tmpdir("sb-log");
        let log_path = log_dir.join("1.log");
        let log = LogWriter::create(log_path.clone(), Arc::new(Notify::new())).unwrap();

        let script = format!(
            "echo ok > allowed.txt; \
             touch .git/hook 2>/dev/null && echo GIT_WRITABLE; \
             touch {}/escape 2>/dev/null && echo OUTSIDE_WRITABLE; \
             cat .env; echo done",
            outside.display()
        );
        let res = run_job(
            ExecSpec {
                argv: vec!["sh".into(), "-c".into(), script],
                cwd: work.display().to_string(),
                env: Vec::new(),
                timeout_s: 15,
                sandbox: SandboxPolicy::WorkspaceWrite {
                    cwd: work.display().to_string(),
                    extra_rw: Vec::new(),
                    allow_net: false,
                },
            },
            log,
            Arc::new(AtomicU32::new(0)),
            Arc::new(Notify::new()),
        )
        .await;
        assert_eq!(res, JobResult::Success);
        assert_eq!(
            std::fs::read_to_string(work.join("allowed.txt")).unwrap(),
            "ok\n"
        );
        assert!(!work.join(".git/hook").exists());
        assert!(!outside.join("escape").exists());
        let (recs, _) = read_from(&log_path, 0, 8192).unwrap();
        let out: String = recs
            .iter()
            .map(|r| String::from_utf8_lossy(&r.data).to_string())
            .collect();
        assert!(!out.contains("GIT_WRITABLE"), "{out}");
        assert!(!out.contains("OUTSIDE_WRITABLE"), "{out}");
        assert!(!out.contains("secret_val"), "{out}");
        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&log_dir);
    }

    #[tokio::test]
    async fn sandbox_rejects_dangerous_env_vars() {
        let dir = tmpdir("danger-env");
        let log = LogWriter::create(dir.join("1.log"), Arc::new(Notify::new())).unwrap();
        let res = run_job(
            ExecSpec {
                argv: vec!["sh".into(), "-c".into(), "echo hi".into()],
                cwd: dir.display().to_string(),
                env: vec![("LD_PRELOAD".into(), "/tmp/evil.so".into())],
                timeout_s: 5,
                sandbox: SandboxPolicy::WorkspaceWrite {
                    cwd: dir.display().to_string(),
                    extra_rw: Vec::new(),
                    allow_net: false,
                },
            },
            log,
            Arc::new(AtomicU32::new(0)),
            Arc::new(Notify::new()),
        )
        .await;
        assert!(
            matches!(res, JobResult::FailedToSpawn { ref error } if error.contains("LD_PRELOAD")),
            "{res:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn chatty_output_cannot_starve_the_timeout() {
        let dir = tmpdir("yes");
        let log = LogWriter::create(dir.join("1.log"), Arc::new(Notify::new())).unwrap();
        let started = Instant::now();
        let result = run_job(
            spec(vec!["sh", "-c", "while true; do echo x; done"], 1),
            log,
            Arc::new(AtomicU32::new(0)),
            Arc::new(Notify::new()),
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(result, JobResult::TimedOut);
        assert!(
            elapsed <= Duration::from_millis(2500),
            "timeout starved by output: {elapsed:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
