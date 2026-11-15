//! Local execution & filesystem primitives shared by CLI, MCP, and wire service.

use crate::job_log::{prune_jobs, JobLog};
use envoy_proto::{
    AgentEvent, Entry, EntryKind, AGENT_CHUNK, DEFAULT_EXEC_TIMEOUT, MAX_EXEC_TIMEOUT, READ_LIMIT,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Directory listing ceiling (a runaway `/` should not fill a frame).
const MAX_ENTRIES: usize = 1000;

/// Executes operations against this machine. `root` is the data dir (job
/// logs live under `root/agent/jobs`).
#[derive(Clone)]
pub struct Runner {
    pub root: PathBuf,
}

impl Runner {
    pub fn new(root: PathBuf) -> Self {
        prune_jobs(&root);
        Self { root }
    }

    /// Run `argv` in its own process group and stream `Started`, `Out*`,
    /// `Done` (or `Failed`) into `tx`. The returned future ends only after
    /// the child is reaped; if `tx` is closed or `cancel` fires, the whole
    /// group is killed.
    pub async fn exec_into(
        &self,
        argv: &[String],
        cwd: Option<String>,
        env: &[(String, String)],
        timeout_s: u64,
        tx: &mpsc::Sender<AgentEvent>,
        cancel: &tokio::sync::Notify,
    ) -> Result<()> {
        let start = Instant::now();
        if argv.is_empty() {
            let _ = tx
                .send(AgentEvent::Failed {
                    error: "empty argv".into(),
                })
                .await;
            return Ok(());
        }
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        let dir = cwd.unwrap_or_else(home_dir);
        if !dir.is_empty() {
            cmd.current_dir(&dir);
        }
        // Do not inherit the daemon's environment wholesale: a token in the
        // service's env must not leak into every remote command.
        cmd.env_clear();
        for (k, v) in base_env() {
            cmd.env(k, v);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own group: a shell's grandchildren die with it.
            cmd.as_std_mut().process_group(0);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx
                    .send(AgentEvent::Failed {
                        error: format!("spawn {}: {e}", argv[0]),
                    })
                    .await;
                return Ok(());
            }
        };
        let pid = child.id().unwrap_or(0);
        // Backstop for every cancellation path, including this future being
        // dropped mid-await.
        let mut guard = KillOnDrop { pid, armed: true };
        if tx.send(AgentEvent::Started { pid }).await.is_err() {
            return Ok(());
        }
        let mut log = JobLog::create(&self.root, pid);

        let (out_tx, mut out_rx) = mpsc::channel::<(u8, Vec<u8>)>(64);
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
            tokio::spawn(async move {
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
            });
        }
        drop(out_tx);

        let timeout_s = match timeout_s {
            0 => DEFAULT_EXEC_TIMEOUT,
            n => n.min(MAX_EXEC_TIMEOUT),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_s);
        let mut timed_out = false;
        let mut cancelled = false;

        let status = loop {
            tokio::select! {
                // Output first so a chatty child never starves exit detection.
                biased;
                Some((fd, data)) = out_rx.recv() => {
                    log.write(fd, &data);
                    if tx.send(AgentEvent::Out { fd, data }).await.is_err() {
                        cancelled = true;
                        break None;
                    }
                }
                status = child.wait() => break Some(status),
                _ = cancel.notified() => {
                    cancelled = true;
                    break None;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    timed_out = true;
                    break None;
                }
            }
        };

        if let Some(status) = status {
            let code = match status {
                Ok(st) => st.code().unwrap_or(-1),
                Err(e) => {
                    warn!(pid, error = %e, "waiting for the agent child failed");
                    -1
                }
            };
            while let Ok(Some((fd, data))) =
                tokio::time::timeout(Duration::from_millis(200), out_rx.recv()).await
            {
                log.write(fd, &data);
                let _ = tx.send(AgentEvent::Out { fd, data }).await;
            }
            let _ = tx
                .send(AgentEvent::Done {
                    code,
                    ms: start.elapsed().as_millis() as u64,
                })
                .await;
            log.finish(code);
            guard.armed = false;
            info!(pid, code, "agent command finished");
            return Ok(());
        }

        // Killed: cancel, timeout or a dead consumer.
        guard.kill();
        guard.armed = false;
        let _ = child.wait().await;
        if timed_out {
            let _ = tx
                .send(AgentEvent::Failed {
                    error: format!("command timed out after {timeout_s}s (killed)"),
                })
                .await;
            log.finish(-9);
        } else if cancelled {
            log.finish(-9);
            debug!(pid, "agent command cancelled");
        }
        Ok(())
    }

    /// Read at most `limit` bytes at `offset`; returns (whole-file size,
    /// mode, bytes read).
    pub async fn read_range(&self, path: &str, offset: u64, limit: u64) -> Result<(u64, u32, Vec<u8>)> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let path = resolve(path);
        let mut f = tokio::fs::File::open(&path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        let meta = f.metadata().await?;
        let limit = if limit == 0 {
            READ_LIMIT
        } else {
            limit.min(READ_LIMIT)
        };
        f.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut data = Vec::new();
        f.take(limit).read_to_end(&mut data).await?;
        Ok((meta.len(), mode_of(&meta), data))
    }

    /// One directory level, sorted by name, capped at [`MAX_ENTRIES`].
    pub async fn list_dir(&self, path: &str) -> Result<Vec<Entry>> {
        let path = resolve(path);
        let mut rd = tokio::fs::read_dir(&path)
            .await
            .with_context(|| format!("list {}", path.display()))?;
        let mut entries = Vec::new();
        while let Some(e) = rd.next_entry().await? {
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(meta) = e.metadata().await else {
                continue;
            };
            let ft = meta.file_type();
            let kind = if ft.is_symlink() {
                EntryKind::Symlink
            } else if ft.is_dir() {
                EntryKind::Dir
            } else if ft.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            entries.push(Entry {
                name,
                kind,
                size: meta.len(),
                mode: mode_of(&meta),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Write a file through a sibling `.part` and rename it into place.
    /// Parent directories are created; an existing file is replaced.
    pub async fn write_file(&self, path: &str, data: &[u8], mode: Option<u32>) -> Result<u64> {
        let path = resolve(path);
        write_atomic(&path, data, mode).await?;
        debug!(path = %path.display(), bytes = data.len(), "agent wrote a file");
        Ok(data.len() as u64)
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

/// Write through a sibling `.part` and rename into place.
pub async fn write_atomic(path: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
    }
    let part = part_path(path);
    tokio::fs::write(&part, data)
        .await
        .with_context(|| format!("write {}", part.display()))?;
    if let Some(m) = mode {
        set_mode(&part, m);
    }
    tokio::fs::rename(&part, path)
        .await
        .with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

trait AsyncReadUnpin: tokio::io::AsyncRead + Unpin {}
impl<T: tokio::io::AsyncRead + Unpin> AsyncReadUnpin for T {}

#[cfg(unix)]
fn kill_group(pid: u32) {
    if pid == 0 {
        return;
    }
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

fn base_env() -> Vec<(String, String)> {
    const KEEP: &[&str] = &[
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "TMPDIR",
    ];
    KEEP.iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
}

/// Resolve `path` the way a shell would: relative names land in `$HOME`.
pub fn resolve(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        PathBuf::from(home_dir()).join(p)
    }
}

/// Sibling temp file for a write; renamed into place only on commit.
pub fn part_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let pid = std::process::id();
    path.with_file_name(format!(".{name}.part-{pid}"))
}

/// Permission bits of a file, `0o644` where the platform has no mode.
pub fn mode_of(meta: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o644
    }
}

/// Apply permission bits; failures are best effort on filesystems without modes.
pub fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn resolve_sends_relative_paths_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(resolve("x/y.txt"), PathBuf::from(home).join("x/y.txt"));
        assert_eq!(resolve("/tmp/a"), PathBuf::from("/tmp/a"));
    }

    #[test]
    fn part_path_is_a_hidden_sibling() {
        let p = part_path(Path::new("/tmp/dir/data.bin"));
        assert_eq!(p.parent().unwrap(), Path::new("/tmp/dir"));
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with(".data.bin.part-"), "{name}");
    }

    #[tokio::test]
    async fn read_range_honours_offset_and_limit() {
        let dir = tmpdir("read");
        let path = dir.join("data.txt");
        std::fs::write(&path, b"0123456789").unwrap();
        let runner = Runner::new(dir.clone());
        let (size, _, data) = runner
            .read_range(path.to_str().unwrap(), 2, 4)
            .await
            .unwrap();
        assert_eq!(size, 10);
        assert_eq!(data, b"2345");
        let (_, _, all) = runner.read_range(path.to_str().unwrap(), 0, 0).await.unwrap();
        assert_eq!(all, b"0123456789");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_file_commits_and_preserves_mode() {
        let dir = tmpdir("write");
        let path = dir.join("bin/tool.sh");
        let runner = Runner::new(dir.clone());
        let n = runner
            .write_file(path.to_str().unwrap(), b"#!/bin/sh\n", Some(0o755))
            .await
            .unwrap();
        assert_eq!(n, 10);
        assert_eq!(std::fs::read(&path).unwrap(), b"#!/bin/sh\n");
        let mode = mode_of(&std::fs::metadata(&path).unwrap());
        assert_eq!(mode & 0o111, 0o111, "executable bits lost: {mode:o}");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".part-"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_dir_sorts_and_tags_kinds() {
        let dir = tmpdir("list");
        std::fs::write(dir.join("b.txt"), b"x").unwrap();
        std::fs::write(dir.join("a.txt"), b"yy").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let runner = Runner::new(dir.clone());
        let entries = runner.list_dir(dir.to_str().unwrap()).await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "sub"]);
        assert_eq!(entries[0].size, 2);
        assert!(matches!(entries[2].kind, EntryKind::Dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exec_streams_output_and_reports_exit() {
        let dir = tmpdir("exec");
        let runner = Runner::new(dir.clone());
        let (tx, mut rx) = mpsc::channel(64);
        let cancel = tokio::sync::Notify::new();
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo out; echo err >&2; exit 3".to_string(),
        ];
        runner
            .exec_into(&argv, None, &[], 10, &tx, &cancel)
            .await
            .unwrap();
        drop(tx);
        let mut stdout = String::new();
        let mut code = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::Out { fd: 0, data } => stdout.push_str(&String::from_utf8_lossy(&data)),
                AgentEvent::Done { code: c, .. } => code = Some(c),
                _ => {}
            }
        }
        assert!(stdout.contains("out"), "{stdout}");
        assert_eq!(code, Some(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exec_timeout_kills_the_group() {
        let dir = tmpdir("timeout");
        let runner = Runner::new(dir.clone());
        let marker = dir.join("late");
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = tokio::sync::Notify::new();
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("sleep 2; touch {}", marker.display()),
        ];
        runner.exec_into(&argv, None, &[], 1, &tx, &cancel).await.unwrap();
        drop(tx);
        let mut failed = false;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Failed { .. } = ev {
                failed = true;
            }
        }
        assert!(failed, "no timeout event");
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!marker.exists(), "the group survived the timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
