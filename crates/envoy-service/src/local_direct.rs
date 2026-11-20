//! Direct in-process backend for [`super::Remote`] when talking to the local [`AgentServer`].
//!
//! Avoids Unix-socket self-connects, postcard framing, and self-loopback audit
//! records when the caller and job server share a process.

use super::AgentServer;
use anyhow::{bail, Context, Result};
use envoy_core::{fs, job, search};
use envoy_proto::{Entry, GrepHit, Hello, Job, JobId, LogRecord, SubmitRequest, AGENT_VERSION};
use std::time::Duration;

const LOG_BATCH: usize = 64 * 1024;
const LOG_TICK: Duration = Duration::from_millis(200);

pub(crate) fn probe(server: &AgentServer) -> Hello {
    Hello {
        version: AGENT_VERSION,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        hostname: envoy_core::system_hostname(),
        device: server.device().map(str::to_string),
    }
}

pub(crate) fn submit(server: &AgentServer, req: SubmitRequest) -> Result<Job> {
    let cfg = server.host().config();
    server.jobs().set_max_jobs(cfg.max_jobs);
    server.jobs().submit(req, "local")
}

pub(crate) fn jobs(server: &AgentServer) -> Result<Vec<Job>> {
    Ok(server.jobs().list())
}

pub(crate) fn status(server: &AgentServer, id: JobId) -> Result<Job> {
    server
        .jobs()
        .get(id)
        .with_context(|| format!("no such job: {id}"))
}

pub(crate) fn cancel(server: &AgentServer, id: JobId) -> Result<()> {
    server.jobs().cancel(id)
}

pub(crate) async fn logs<F>(
    server: &AgentServer,
    id: JobId,
    mut offset: u64,
    follow: bool,
    mut sink: F,
) -> Result<(u64, Option<i32>)>
where
    F: FnMut(&[LogRecord]),
{
    let Some(path) = server.jobs().log_path(id) else {
        bail!("no such job: {id}");
    };
    let wake = server.jobs().wake();
    loop {
        let (records, next) = job::read_available(&path, offset, LOG_BATCH)
            .map_err(|e| anyhow::anyhow!("reading job log: {e}"))?;
        let progressed = !records.is_empty();
        offset = next;
        let Some(snapshot) = server.jobs().get(id) else {
            bail!("job {id} is no longer available");
        };
        let done = snapshot.state.exit_code();
        let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(offset);
        let complete = done.is_some() && (offset >= file_len || !progressed);
        if progressed {
            sink(&records);
        }
        if complete || !follow {
            return Ok((offset, if complete { done } else { None }));
        }
        if done.is_some() {
            continue;
        }
        tokio::select! {
            _ = wake.notified() => {}
            _ = tokio::time::sleep(LOG_TICK) => {}
        }
    }
}

pub(crate) async fn read(path: &str, offset: u64, limit: u64) -> Result<(u64, u32, Vec<u8>)> {
    fs::read_range(path, offset, limit).await
}

pub(crate) async fn write(path: &str, data: &[u8], mode: Option<u32>) -> Result<u64> {
    fs::write_file(path, data, mode).await
}

pub(crate) async fn edit(path: &str, old: &str, new: &str, replace_all: bool) -> Result<usize> {
    fs::edit_file(path, old, new, replace_all).await
}

pub(crate) async fn list(path: &str) -> Result<Vec<Entry>> {
    fs::list_dir(path).await
}

pub(crate) async fn glob(root: &str, pattern: &str, max_results: u32) -> Result<Vec<String>> {
    search::glob_files(root, pattern, max_results).await
}

pub(crate) async fn grep(
    root: &str,
    pattern: &str,
    include: Option<&str>,
    max_results: u32,
) -> Result<Vec<GrepHit>> {
    search::grep_files(root, pattern, include, max_results).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{dialer_for, StaticHost};
    use crate::Remote;
    use envoy_core::AgentConfig;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn tmpdir(tag: &str) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "envoy-direct-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn local_direct_runs_ops_without_self_audit_log() {
        let dir = tmpdir("audit");
        let work = dir.join("ws");
        std::fs::create_dir_all(&work).unwrap();
        let target = work.join("hello.txt");

        let host = Arc::new(StaticHost {
            config: AgentConfig::default(),
        });
        let server = AgentServer::new(dir.clone(), host);
        let remote = Remote::with_local(dialer_for(&dir, "local", None), Some(server));

        let hello = remote.probe("local").await.unwrap();
        assert_eq!(hello.version, AGENT_VERSION);

        remote
            .write("local", target.to_str().unwrap(), b"alpha\nbeta\n", None)
            .await
            .unwrap();
        let (_sz, _mode, bytes) = remote
            .read("local", target.to_str().unwrap(), 0, 1024)
            .await
            .unwrap();
        assert_eq!(bytes, b"alpha\nbeta\n");

        let job = remote
            .submit(
                "local",
                SubmitRequest {
                    spec: envoy_proto::TaskSpec::Shell {
                        command: "printf 'ok\\n'".into(),
                    },
                    cwd: Some(work.display().to_string()),
                    env: Vec::new(),
                    label: None,
                    group: "default".into(),
                    timeout_s: 10,
                    sandbox: envoy_proto::SandboxPolicy::Off,
                },
            )
            .await
            .unwrap();
        let mut out = Vec::new();
        let (_next, code) = remote
            .logs("local", job.id, 0, true, |recs| {
                for r in recs {
                    out.extend_from_slice(&r.data);
                }
            })
            .await
            .unwrap();
        assert_eq!(code, Some(0));
        assert_eq!(out, b"ok\n");
        assert!(!dir.join("agent/audit.log").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
