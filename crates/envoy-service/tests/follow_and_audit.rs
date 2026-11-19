//! Phase 3 regressions: log followers must not drop the tail of a short job,
//! and failed RPCs must be audited as `"ok": false`.

use envoy_core::AgentConfig;
use envoy_proto::{SandboxPolicy, SubmitRequest, TaskSpec, DEFAULT_GROUP};
use envoy_service::{AgentDialer, AgentServer, Caller, Remote, StaticHost, UnixDialer};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn tmpdir(tag: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = PathBuf::from("/tmp").join(format!("envoy-p3-{tag}-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn shell(command: &str) -> SubmitRequest {
    SubmitRequest {
        spec: TaskSpec::Shell {
            command: command.to_string(),
        },
        cwd: None,
        env: Vec::new(),
        label: None,
        group: DEFAULT_GROUP.to_string(),
        timeout_s: 30,
        sandbox: SandboxPolicy::Off,
    }
}

struct Running {
    dir: PathBuf,
    remote: Remote<Box<dyn AgentDialer>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    accept: tokio::task::JoinHandle<()>,
}

impl Running {
    async fn start(tag: &str) -> Self {
        let dir = tmpdir(tag);
        let sock = dir.join("envoy.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let host = Arc::new(StaticHost::new(AgentConfig {
            enabled: true,
            allow: vec![],
            max_jobs: 4,
        }));
        let server = AgentServer::new(dir.clone(), host);
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let accept = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let server = server.clone();
                        tokio::spawn(async move {
                            let _ = server.serve_stream(Caller::Local, stream).await;
                        });
                    }
                }
            }
        });
        let dialer: Box<dyn AgentDialer> = Box::new(UnixDialer::new(sock));
        Self {
            dir,
            remote: Remote::new(dialer),
            stop: Some(stop_tx),
            accept,
        }
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = self.accept.await;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn follow_log_returns_the_full_tail_of_a_large_fast_job() {
    let srv = Running::start("follow").await;
    let bytes = 300_000usize;
    let cmd = format!("python3 -c 'import sys; sys.stdout.buffer.write(b\"Z\"*{bytes})'");
    let job = srv.remote.submit("local", shell(&cmd)).await.unwrap();
    let collected = tokio::time::timeout(Duration::from_secs(15), async {
        let mut got = Vec::new();
        let followed = srv
            .remote
            .logs("local", job.id, 0, true, |records| {
                for record in records {
                    got.extend_from_slice(&record.data);
                }
            })
            .await?;
        Ok::<_, anyhow::Error>((followed, got))
    })
    .await
    .expect("follow_log hung")
    .unwrap();
    let (followed, got) = collected;
    assert_eq!(followed.1, Some(0), "exit {:?}", followed);
    assert_eq!(
        got.len(),
        bytes,
        "lost {} bytes",
        bytes.saturating_sub(got.len())
    );
    assert!(got.iter().all(|b| *b == b'Z'));
    srv.stop().await;
}

#[tokio::test]
async fn missing_job_follow_fails_instead_of_waiting_forever() {
    let srv = Running::start("missing").await;
    let err = tokio::time::timeout(Duration::from_secs(3), async {
        srv.remote
            .logs("local", 999_999, 0, true, |_| {})
            .await
    })
    .await
    .expect("follow of a missing job hung")
    .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("no such job") || text.contains("no longer available"),
        "{text}"
    );
    srv.stop().await;
}

#[tokio::test]
async fn failed_rpc_is_audited_as_not_ok() {
    let srv = Running::start("audit").await;
    let err = srv.remote.status("local", 999_999).await.unwrap_err();
    assert!(err.to_string().contains("no such job"), "{err}");
    let audit = std::fs::read_to_string(srv.dir.join("agent/audit.log")).unwrap();
    assert!(
        audit.lines().any(|line| line.contains("\"ok\":false") && line.contains("job-status")),
        "{audit}"
    );
    srv.stop().await;
}
