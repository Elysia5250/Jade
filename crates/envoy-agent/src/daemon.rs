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

pub const LAUNCHD_LABEL: &str = "dev.envoy.agent";
pub const SYSTEMD_UNIT: &str = "envoy.service";

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("no HOME set; cannot locate user service dirs"))
}

pub fn launchd_plist_path() -> Result<PathBuf> {
    Ok(home()?
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

pub fn systemd_unit_path() -> Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".config")).ok())
        .ok_or_else(|| {
            anyhow::anyhow!("no HOME/XDG_CONFIG_HOME; cannot locate systemd user dir")
        })?;
    Ok(config.join("systemd/user").join(SYSTEMD_UNIT))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn launchd_plist(exe: &Path, data_dir: &Path) -> String {
    let exe = xml_escape(&exe.display().to_string());
    let dir = xml_escape(&data_dir.display().to_string());
    let log = xml_escape(&log_path(data_dir).display().to_string());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>Label</key><string>{LAUNCHD_LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         \x20   <string>{exe}</string>\n\
         \x20   <string>--data-dir</string>\n\
         \x20   <string>{dir}</string>\n\
         \x20   <string>serve</string>\n\
         \x20 </array>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><true/>\n\
         \x20 <key>ProcessType</key><string>Background</string>\n\
         \x20 <key>StandardOutPath</key><string>{log}</string>\n\
         \x20 <key>StandardErrorPath</key><string>{log}</string>\n\
         </dict>\n</plist>\n"
    )
}

fn systemd_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn systemd_unit(exe: &Path, data_dir: &Path) -> String {
    format!(
        "[Unit]\nDescription=Envoy agent daemon\nAfter=network-online.target\n\n\
         [Service]\nExecStart={} --data-dir {} serve\nRestart=on-failure\nRestartSec=5\n\n\
         [Install]\nWantedBy=default.target\n",
        systemd_quote(&exe.display().to_string()),
        systemd_quote(&data_dir.display().to_string())
    )
}

fn run(cmd: &mut std::process::Command) -> Result<()> {
    let out = cmd.output().context("spawn the service controller")?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "{} failed: {}",
            cmd.get_program().to_string_lossy(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

/// Install a user-level autostart unit for `envoy serve`.
///
/// The daemon keeps the local socket and the conduit accept pool; launching it
/// through the OS is what makes it survive logout/reboot.
pub fn install(exe: &Path, data_dir: &Path) -> Result<String> {
    std::fs::create_dir_all(data_dir)?;
    if cfg!(target_os = "macos") {
        let path = launchd_plist_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, launchd_plist(exe, data_dir))?;
        let _ =
            run(std::process::Command::new("launchctl")
                .args(["unload", &path.display().to_string()]));
        run(std::process::Command::new("launchctl").args([
            "load",
            "-w",
            &path.display().to_string(),
        ]))?;
        Ok(format!(
            "installed and loaded {} (logs: {})",
            path.display(),
            log_path(data_dir).display()
        ))
    } else {
        let path = systemd_unit_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, systemd_unit(exe, data_dir))?;
        run(std::process::Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        run(std::process::Command::new("systemctl").args([
            "--user",
            "enable",
            "--now",
            SYSTEMD_UNIT,
        ]))?;
        Ok(format!(
            "installed and started {} (logs: journalctl --user -u {SYSTEMD_UNIT})",
            path.display()
        ))
    }
}

pub fn uninstall() -> Result<String> {
    if cfg!(target_os = "macos") {
        let path = launchd_plist_path()?;
        if path.exists() {
            let _ = run(std::process::Command::new("launchctl").args([
                "unload",
                "-w",
                &path.display().to_string(),
            ]));
            std::fs::remove_file(&path)?;
        }
        Ok(format!("uninstalled {}", path.display()))
    } else {
        let path = systemd_unit_path()?;
        if path.exists() {
            let _ = run(std::process::Command::new("systemctl").args([
                "--user",
                "disable",
                "--now",
                SYSTEMD_UNIT,
            ]));
            std::fs::remove_file(&path)?;
            let _ = run(std::process::Command::new("systemctl").args(["--user", "daemon-reload"]));
        }
        Ok(format!("uninstalled {}", path.display()))
    }
}

/// One-line autostart report for `daemon status`.
pub fn autostart_line() -> String {
    if cfg!(target_os = "macos") {
        let installed = launchd_plist_path().map(|p| p.exists()).unwrap_or(false);
        format!("launchd installed={installed}")
    } else {
        let installed = systemd_unit_path().map(|p| p.exists()).unwrap_or(false);
        let active = std::process::Command::new("systemctl")
            .args(["--user", "is-active", SYSTEMD_UNIT])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".into());
        format!("systemd installed={installed} active={active}")
    }
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

async fn agent_sock_ready(data_dir: &Path) -> bool {
    tokio::net::UnixStream::connect(crate::runtime::socket_path(data_dir))
        .await
        .is_ok()
}

/// Connect to the daemon, starting it detached if it is not up yet.
///
/// `is_running` only checks the job socket. This also waits until `agent.sock`
/// accepts a connection, which is what the console attaches to.
pub async fn ensure(data_dir: &Path) -> Result<()> {
    if is_running(data_dir).await {
        wait_for_agent_sock(data_dir).await?;
        return probe_agent_protocol(data_dir).await;
    }
    spawn(data_dir)?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if is_running(data_dir).await && agent_sock_ready(data_dir).await {
            return probe_agent_protocol(data_dir).await;
        }
    }
    bail!(
        "the envoy daemon did not come up; see {}",
        log_path(data_dir).display()
    )
}

/// Confirm `agent.sock` speaks this binary's protocol. A mismatch returns a
/// hint to stop the old daemon. This never signals the running process.
async fn probe_agent_protocol(data_dir: &Path) -> Result<()> {
    let mut stream = tokio::net::UnixStream::connect(crate::runtime::socket_path(data_dir))
        .await
        .context("connecting to agent.sock for the version handshake")?;
    let result = tokio::time::timeout(Duration::from_secs(5), handshake_probe(&mut stream)).await;
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(annotate_handshake(err)),
        Err(_) => bail!(
            "agent.sock did not finish the version handshake within 5s. Stop the old daemon with `envoy daemon stop` and start this binary again"
        ),
    }
}

async fn handshake_probe(stream: &mut tokio::net::UnixStream) -> Result<()> {
    use crate::runtime::hello::{
        protocol_mismatch_message, read_hello, write_hello, AGENT_PROTOCOL_VERSION,
    };
    write_hello(stream, 0).await?;
    let hello = read_hello(stream).await?;
    if hello.protocol_version != AGENT_PROTOCOL_VERSION {
        bail!(
            "{}",
            protocol_mismatch_message(hello.protocol_version, AGENT_PROTOCOL_VERSION)
        );
    }
    Ok(())
}

fn annotate_handshake(err: anyhow::Error) -> anyhow::Error {
    if err.to_string().contains("envoy daemon stop") {
        return err;
    }
    anyhow::anyhow!(
        "{err:#}. Stop the old daemon with `envoy daemon stop` and start this binary again"
    )
}

async fn wait_for_agent_sock(data_dir: &Path) -> Result<()> {
    for _ in 0..100 {
        if agent_sock_ready(data_dir).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!(
        "envoy is listening but agent.sock is not ready; see {}",
        log_path(data_dir).display()
    )
}

/// In-process job servers bind `envoy.sock` without `envoy serve`. `ensure`
/// also requires `agent.sock` to accept, so those tests park an idle listener.
#[cfg(test)]
pub(crate) fn park_agent_sock(data_dir: &Path) {
    let path = crate::runtime::socket_path(data_dir);
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)
        .unwrap_or_else(|err| panic!("binding {}: {err}", path.display()));
    tokio::spawn(async move {
        use crate::runtime::hello::{read_hello, write_hello};
        while let Ok((mut stream, _)) = listener.accept().await {
            if read_hello(&mut stream).await.is_err() {
                continue;
            }
            let _ = write_hello(&mut stream, 0).await;
        }
    });
}

/// Spawn `envoy serve` detached from this process (own session, output to a
/// log file). Concurrent callers race harmlessly: the loser fails to bind and
/// exits.
pub fn spawn(data_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let exe = std::env::current_exe().context("locating the envoy binary")?;
    let log = log_path(data_dir);
    envoy_core::audit::rotate_if_oversized(&log, envoy_core::audit::LOG_ROTATE_BYTES);
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
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
            // Double-fork: the intermediate exits so the daemon is reparented
            // and this process can wait without leaving a zombie.
            let pid = libc::fork();
            if pid < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if pid > 0 {
                libc::_exit(0);
            }
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("spawning the envoy daemon")?;
    let _ = child.wait();
    Ok(())
}

/// Ask a running daemon to shut down. Returns whether a pid was signalled.
pub fn stop(data_dir: &Path) -> Result<bool> {
    let text = std::fs::read_to_string(pid_path(data_dir)).unwrap_or_default();
    let Some(pid) = text.trim().parse::<i32>().ok() else {
        return Ok(false);
    };
    if pid <= 0 || pid == std::process::id() as i32 {
        return Ok(false);
    }
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
                    #[cfg(unix)]
                    {
                        let same_user = stream
                            .peer_cred()
                            .map(|cred| cred.uid() == unsafe { libc::geteuid() })
                            .unwrap_or(false);
                        if !same_user {
                            tracing::warn!("rejected local socket connection from another uid");
                            continue;
                        }
                    }
                    let server = server.clone();
                    tokio::spawn(async move {
                        if let Err(e) = server
                            .serve_stream(envoy_core::Caller::Local, stream)
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
    #[test]
    fn autostart_units_carry_exe_and_data_dir() {
        let exe = Path::new("/Users/u/.local/bin/envoy");
        let dir = Path::new("/Users/u/.envoy");
        let plist = launchd_plist(exe, dir);
        assert!(plist.starts_with("<?xml"));
        assert!(plist.contains(&format!("<string>{LAUNCHD_LABEL}</string>")));
        assert!(plist.contains("<string>/Users/u/.local/bin/envoy</string>"));
        assert!(plist.contains("<string>/Users/u/.envoy</string>"));
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("StandardErrorPath"));

        let unit = systemd_unit(exe, dir);
        assert!(
            unit.contains(
                "ExecStart=\"/Users/u/.local/bin/envoy\" --data-dir \"/Users/u/.envoy\" serve"
            )
        );
        assert!(unit.contains("Restart=on-failure"));
    }

    use super::*;
    use envoy_core::AgentConfigStore;
    use envoy_proto::{JobResult, TaskSpec, DEFAULT_GROUP};
    use envoy_service::{Remote, UnixDialer};

    fn tmpdir() -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir();
        let base = if base.to_string_lossy().len() > 60 && Path::new("/tmp").is_dir() {
            PathBuf::from("/tmp")
        } else {
            base
        };
        let p = base.join(format!("ed-{n}"));
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
            sandbox: envoy_proto::SandboxPolicy::Off,
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
        let job = remote
            .submit("local", shell("sleep 0.2; echo daemon-ok"))
            .await
            .unwrap();
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

    #[test]
    fn stop_refuses_non_positive_and_self_pids() {
        let dir = tmpdir();
        for bad in ["0", "-1", "-42", &std::process::id().to_string()] {
            std::fs::write(pid_path(&dir), bad).unwrap();
            assert!(!stop(&dir).unwrap(), "must refuse pid {bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
