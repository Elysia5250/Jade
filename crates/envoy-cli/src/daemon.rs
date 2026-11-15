//! Local daemon lifecycle: socket paths, auto-start, stop.

use anyhow::{bail, Context, Result};
use envoy_service::AgentServer;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("envoy.sock")
}

fn pid_path(data_dir: &Path) -> PathBuf {
    data_dir.join("envoy.pid")
}

pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("envoy.out")
}

pub fn pid_file(data_dir: &Path) -> PathBuf {
    pid_path(data_dir)
}

/// A live daemon accepts connections on its socket.
pub async fn is_running(data_dir: &Path) -> bool {
    tokio::net::UnixStream::connect(socket_path(data_dir))
        .await
        .is_ok()
}

/// One daemon per data directory. `flock` is advisory, but every envoy entry
/// point goes through this file, so in practice it is the single-instance gate:
/// without it a second `envoy serve` would unlink the live socket, bind its
/// own, and both processes would write the same `jobs/state.json`.
pub struct InstanceLock {
    _file: std::fs::File,
}

impl InstanceLock {
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join("envoy.lock");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                bail!(
                    "another envoy daemon already owns {} (socket {}); stop it with `envoy daemon stop`",
                    data_dir.display(),
                    socket_path(data_dir).display()
                );
            }
        }
        use std::io::Write as _;
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}", std::process::id());
        Ok(Self { _file: file })
    }
}

/// Connect to the daemon, starting it detached if it is not up yet.
pub async fn ensure(data_dir: &Path) -> Result<()> {
    if is_running(data_dir).await {
        return Ok(());
    }
    spawn(data_dir)?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if is_running(data_dir).await {
            return Ok(());
        }
    }
    bail!(
        "the envoy daemon did not come up; see {}",
        log_path(data_dir).display()
    )
}

/// Spawn `envoy serve` detached from this process (own session, output to a
/// log file). Concurrent callers race harmlessly: the loser fails to bind and
/// exits.
pub fn spawn(data_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let exe = std::env::current_exe().context("locating the envoy binary")?;
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(data_dir))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--data-dir")
        .arg(data_dir)
        .arg("serve")
        .arg("--socket")
        .arg(socket_path(data_dir))
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("spawning the envoy daemon")?;
    Ok(())
}

/// Ask a running daemon to shut down. Returns whether a pid was signalled.
pub fn stop(data_dir: &Path) -> Result<bool> {
    let text = std::fs::read_to_string(pid_path(data_dir)).unwrap_or_default();
    let Some(pid) = text.trim().parse::<i32>().ok() else {
        return Ok(false);
    };
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid, libc::SIGTERM) != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Accept request streams until `shutdown` fires.
///
/// Every connection gets its own task: a job watch can live for minutes and
/// must never block `accept`, let alone other callers.
pub async fn run_server(
    server: AgentServer,
    listener: tokio::net::UnixListener,
    socket: PathBuf,
    pid_file: Option<PathBuf>,
    shutdown: impl std::future::Future<Output = ()>,
) {
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let server = server.clone();
                    tokio::spawn(async move {
                        if let Err(e) = server
                            .serve_stream(envoy_proto::LOCAL_CALLER, stream)
                            .await
                        {
                            tracing::debug!(error = %e, "client stream ended");
                        }
                    });
                }
                Err(e) => tracing::warn!(error = %e, "accept failed"),
            }
        }
    }
    // Orphaned children would keep running with no way to reach them.
    server.jobs().shutdown();
    let _ = std::fs::remove_file(&socket);
    if let Some(pid_file) = pid_file {
        let _ = std::fs::remove_file(pid_file);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use envoy_core::AgentConfigStore;
    use envoy_proto::{JobResult, TaskSpec, DEFAULT_GROUP};
    use envoy_service::{UnixDialer, Remote};

    fn tmpdir() -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-daemon-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn shell(command: &str) -> envoy_proto::SubmitRequest {
        envoy_proto::SubmitRequest {
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

    #[tokio::test]
    async fn daemon_runs_jobs_across_client_connections() {
        let dir = tmpdir();
        let _ = AgentConfigStore::open(dir.clone()).unwrap();
        let socket = socket_path(&dir);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = AgentServer::open_store(dir.clone()).unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let socket_for_task = socket.clone();
        let task = tokio::spawn(async move {
            run_server(server, listener, socket_for_task, None, async {
                let _ = stop_rx.await;
            })
            .await;
        });

        let remote = Remote::new(UnixDialer::new(socket.clone()));
        let job = remote.submit("local", shell("sleep 0.2; echo daemon-ok")).await.unwrap();
        assert!(job.id > 0);

        // A second, independent connection follows the same job.
        let mut text = String::new();
        let (_cursor, finished) = remote
            .logs("local", job.id, 0, true, |records| {
                for r in records {
                    text.push_str(&String::from_utf8_lossy(&r.data));
                }
            })
            .await
            .unwrap();
        assert_eq!(finished, Some(0));
        assert!(text.contains("daemon-ok"), "{text}");

        // Files travel over the same socket.
        let target = dir.join("hello.txt");
        let written = remote
            .write("local", target.to_str().unwrap(), b"hello", Some(0o600))
            .await
            .unwrap();
        assert_eq!(written, 5);
        let (_, _, data) = remote
            .read("local", target.to_str().unwrap(), 0, 0)
            .await
            .unwrap();
        assert_eq!(data, b"hello");

        // Cancel works on a fresh connection, and shutdown kills the rest.
        let long = remote.submit("local", shell("sleep 30")).await.unwrap();
        remote.cancel("local", long.id).await.unwrap();
        for _ in 0..300 {
            let now = remote.status("local", long.id).await.unwrap();
            if now.is_done() {
                assert_eq!(now.state.result(), Some(&JobResult::Killed));
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let _ = stop_tx.send(());
        task.await.unwrap();
        assert!(!socket.exists(), "the socket was not cleaned up");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
