//! Phase 1 stress tests for `envoy-service`: high-concurrency Unix socket RPC,
//! streaming log backpressure, and aborted client resilience.

use envoy_core::AgentConfig;
use envoy_proto::{JobState, SandboxPolicy, SubmitRequest, TaskSpec, DEFAULT_GROUP};
use envoy_service::{AgentDialer, AgentServer, Remote, StaticHost, UnixDialer};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UnixListener;

fn tmpdir(tag: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Keep socket paths well under macOS SUN_LEN (104 bytes).
    let p = PathBuf::from("/tmp").join(format!("es-{tag}-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn shell_req(command: &str) -> SubmitRequest {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_24_concurrent_clients_with_aborted_followers() {
    let dir = tmpdir("unix-24");
    let sock = dir.join("envoy.sock");
    let listener = UnixListener::bind(&sock).unwrap();

    let host = Arc::new(StaticHost::new(AgentConfig {
        enabled: true,
        allow: vec!["stress-client".into()],
        max_jobs: 8,
    }));
    let server = AgentServer::new(dir.clone(), host);

    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let srv = server.clone();
    let accept_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break; };
                    let s = srv.clone();
                    tokio::spawn(async move {
                        let _ = s.serve_stream("stress-client", stream).await;
                    });
                }
            }
        }
    });

    let started = Instant::now();
    let dialer: Box<dyn AgentDialer> = Box::new(UnixDialer::new(sock.clone()));
    let remote = Arc::new(Remote::new(dialer));

    // Spawn 12 normal full-lifecycle workers + 12 workers whose log stream is aborted mid-flight.
    let mut workers = Vec::new();
    for i in 0..24 {
        let rem = Arc::clone(&remote);
        let abort_midway = i % 2 == 1;
        workers.push(tokio::spawn(async move {
            let cmd = format!(
                "python3 -c 'import time,sys; [print(f\"c{i}-{{k}}\", flush=True) or time.sleep(0.01) for k in range(15)]'"
            );
            let job = rem.submit("local", shell_req(&cmd)).await.unwrap();
            if abort_midway {
                let rem2 = Arc::clone(&rem);
                let h = tokio::spawn(async move {
                    let _ = rem2.logs("local", job.id, 0, true, |_| {}).await;
                });
                tokio::time::sleep(Duration::from_millis(35)).await;
                h.abort();
                // Wait for job completion via status polling to ensure server didn't hang.
                loop {
                    let st = rem.status("local", job.id).await.unwrap();
                    if st.state.is_done() {
                        assert_eq!(st.state.exit_code(), Some(0));
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
            } else {
                let mut cursor = 0u64;
                let mut out = String::new();
                loop {
                    let (next, finished) = rem
                        .logs("local", job.id, cursor, true, |recs| {
                            for r in recs {
                                out.push_str(&String::from_utf8_lossy(&r.data));
                            }
                        })
                        .await
                        .unwrap();
                    cursor = next;
                    if let Some(code) = finished {
                        assert_eq!(code, 0);
                        break;
                    }
                }
                assert!(out.contains(&format!("c{i}-14")), "missing tail for {i}: {out}");
            }
        }));
    }

    for w in workers {
        tokio::time::timeout(Duration::from_secs(30), w)
            .await
            .expect("worker timed out")
            .unwrap();
    }

    // Verify server still answers cleanly after connection aborts.
    let list = remote.jobs("local").await.unwrap();
    assert_eq!(list.len(), 24);
    assert!(list
        .iter()
        .all(|j| matches!(j.state, JobState::Done { .. })));

    let _ = stop_tx.send(());
    let _ = accept_task.await;
    eprintln!(
        "[stress_server] 24 concurrent clients (12 aborted mid-stream) finished in {:?}",
        started.elapsed()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
