//! Concurrent session claims, bounded subscribers, and monitor cancellation.

use super::live::LivePhase;
use super::server::Runtime;
use super::SendMode;
use crate::agent::{AgentEvent, MonitorBroker, MonitorStartSpec};
use envoy_proto::{JobResult, JobState, SandboxPolicy, SubmitRequest, TaskSpec, DEFAULT_GROUP};
use envoy_service::{AgentServer, Remote, UnixDialer};
use std::path::PathBuf;
use std::time::Duration;

fn tmpdir(tag: &str) -> PathBuf {
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    let path = PathBuf::from("/tmp").join(format!(
        "ep-{tag}-{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[tokio::test]
async fn two_sends_and_two_rewinds_do_not_panic() {
    let dir = tmpdir("race");
    let runtime = Runtime::new(dir.clone());
    let live = runtime.ensure("s1".into(), dir.clone(), "local".into(), Vec::new());

    let first = {
        let runtime = std::sync::Arc::clone(&runtime);
        tokio::spawn(async move {
            runtime.send("s1", "one".into(), SendMode::Prompt);
        })
    };
    let second = {
        let runtime = std::sync::Arc::clone(&runtime);
        tokio::spawn(async move {
            runtime.send("s1", "two".into(), SendMode::Prompt);
        })
    };
    first.await.unwrap();
    second.await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let state = live.lock().expect("session lock poisoned");
        assert_eq!(state.phase, LivePhase::Idle);
        assert!(!state.running);
        assert!(state
            .events
            .iter()
            .any(|event| matches!(event.event, AgentEvent::TurnError(_))));
    }

    let rewind_a = {
        let runtime = std::sync::Arc::clone(&runtime);
        tokio::spawn(async move { runtime.rewind("s1", None).await })
    };
    let rewind_b = {
        let runtime = std::sync::Arc::clone(&runtime);
        tokio::spawn(async move { runtime.rewind("s1", None).await })
    };
    let left = rewind_a.await.expect("rewind task panicked");
    let right = rewind_b.await.expect("rewind task panicked");
    assert!(left.is_err(), "{left:?}");
    assert!(right.is_err(), "{right:?}");
    let state = live.lock().expect("session lock poisoned after rewind");
    assert_eq!(state.phase, LivePhase::Idle);
    assert!(!state.running);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_full_subscriber_is_dropped_and_session_reset_is_logged() {
    let dir = tmpdir("sub");
    let runtime = Runtime::new(dir.clone());
    let live = runtime.ensure("s1".into(), dir.clone(), "local".into(), Vec::new());
    let mut state = live.lock().unwrap();
    let rx = state.subscribe();
    for n in 0..2_048 {
        runtime.publish(&mut state, AgentEvent::TextDelta(format!("{n}")));
        if state.subs.is_empty() {
            break;
        }
    }
    assert!(state.subs.is_empty(), "slow subscriber stayed attached");
    assert!(state
        .events
        .iter()
        .any(|event| matches!(event.event, AgentEvent::SessionReset)));
    drop(rx);
    drop(state);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn close_session_cancels_an_owned_monitor_job() {
    let dir = tmpdir("mon");
    let sock = PathBuf::from(format!("/tmp/em-{}", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let server = AgentServer::open_store(dir.join("jobs")).unwrap();
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.serve_stream(envoy_core::Caller::Local, stream).await;
            });
        }
    });

    let remote = Remote::new(UnixDialer::new(sock.clone()));
    let job = remote
        .submit(
            "local",
            SubmitRequest {
                spec: TaskSpec::Shell {
                    command: "sleep 30".into(),
                },
                cwd: Some(dir.display().to_string()),
                env: Vec::new(),
                label: Some("monitor-owned".into()),
                group: DEFAULT_GROUP.to_string(),
                timeout_s: 60,
                sandbox: SandboxPolicy::Off,
            },
        )
        .await
        .unwrap();

    let runtime = Runtime::new(dir.clone());
    {
        let live = runtime.ensure("s1".into(), dir.clone(), "local".into(), Vec::new());
        // `start` publishes MonitorsSync, which locks the session. Hold the hub,
        // not the session mutex, across that call. Drop this clone before close
        // so the runtime's Arc is the last owner and Drop cancels the job.
        let hub = {
            let state = live.lock().unwrap();
            state.monitors.set_local_socket(Some(sock.clone()));
            state.monitors.clone()
        };
        hub.start(MonitorStartSpec {
            label: "watch".into(),
            job_id: job.id,
            command: "sleep 30".into(),
            pattern: "NO-SUCH-LINE".into(),
            debounce_ms: 1_000,
            max_triggers: None,
            owns_job: true,
            target: "local".into(),
        })
        .unwrap();
    }

    assert!(runtime.close_session("s1").await.unwrap());
    let mut cancelled = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(status) = remote.status("local", job.id).await {
            if let JobState::Done { result, .. } = status.state {
                cancelled = matches!(result, JobResult::Killed | JobResult::Failed { .. });
                if cancelled {
                    break;
                }
            }
        }
    }
    assert!(cancelled, "owned monitor job was not cancelled");
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_dir_all(&dir);
}
