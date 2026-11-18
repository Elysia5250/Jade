//! Phase 3 runtime concurrency stress tests:
//! concurrent Steer + FollowUp queuing + multi-subscriber fanout with mid-stream disconnects.

use super::protocol::SendMode;
use super::server::Runtime;
use crate::agent::{AgentEvent, AskBroker};
use crate::auth::{now_ms, record_provider_login, Credential, ProviderProtocol, StoredProvider};
use crate::permissions::{PermissionMode, SandboxOverride};
use envoy_service::AgentServer;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

fn short_tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = PathBuf::from("/tmp").join(format!(
        "ers-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

async fn start_test_daemon(data_dir: &Path) -> oneshot::Sender<()> {
    let server = AgentServer::open_store(data_dir.to_path_buf()).unwrap();
    let socket = crate::daemon::socket_path(data_dir);
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        crate::daemon::run_server(server, listener, socket, None, async {
            let _ = stop_rx.await;
        })
        .await;
    });
    stop_tx
}

async fn read_http_body(sock: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let Ok(n) = sock.read(&mut tmp).await else {
            break;
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let hdr = String::from_utf8_lossy(&buf[..pos]);
            let cl = hdr
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.eq_ignore_ascii_case("content-length") {
                        v.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            if buf.len() >= pos + 4 + cl {
                break;
            }
        }
    }
    let raw = String::from_utf8_lossy(&buf).to_string();
    raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_concurrent_steer_followup_and_multi_subscriber_fanout() {
    let dir = short_tmpdir("rt");
    let data = dir.join("data");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("info.txt"), "RT_OK").unwrap();

    let stop_daemon = start_test_daemon(&data).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let bodies_srv = Arc::clone(&bodies);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let b_srv = Arc::clone(&bodies_srv);
            tokio::spawn(async move {
                let body = read_http_body(&mut sock).await;
                b_srv.lock().unwrap().push(body.clone());
                let sse = if body.contains("FOLLOW_UP_") {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"followup done\"}}]}\n\ndata: [DONE]\n\n"
                } else if body.contains("RT_OK") {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"initial done\"}}]}\n\ndata: [DONE]\n\n"
                } else {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"info.txt\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}",
                    sse.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let prov = StoredProvider {
        id: "openai".into(),
        name: "Mock".into(),
        protocol: ProviderProtocol::OpenAiChat,
        base_url: format!("http://{addr}/v1"),
        credential: Credential::ApiKey {
            key: "sk-rt".into(),
        },
        custom_models: vec!["gpt-rt".into()],
        model_routes: BTreeMap::new(),
        updated_ms: now_ms(),
    };
    record_provider_login(&data, prov, Some("gpt-rt".into())).unwrap();

    let runtime = Runtime::new(data.clone());
    let live = runtime
        .ensure_with_hooks("s-stress".into(), ws.clone(), "local".into(), Vec::new())
        .await;

    let (mut rxs, drop_rx) = {
        let mut st = live.lock().unwrap();
        st.asks = AskBroker::new_headless();
        st.permissions.set_mode(PermissionMode::Auto);
        st.permissions.set_sandbox_override(Some(SandboxOverride::Off));
        let rxs = vec![st.subscribe(), st.subscribe(), st.subscribe()];
        let drop_rx = st.subscribe();
        (rxs, drop_rx)
    };

    let started = Instant::now();
    runtime.send("s-stress", "INITIAL_PROMPT".into(), SendMode::Prompt);

    // Concurrent steers and follow-ups while turn 1's first LLM call / tool batch is active.
    let mut senders = Vec::new();
    for i in 0..3 {
        let rt = Arc::clone(&runtime);
        senders.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(15)).await;
            rt.send("s-stress", format!("STEER_MSG_{i}"), SendMode::Steer);
        }));
    }
    for i in 0..3 {
        let rt = Arc::clone(&runtime);
        senders.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            rt.send("s-stress", format!("FOLLOW_UP_{i}"), SendMode::FollowUp);
        }));
    }
    drop(drop_rx); // simulate disconnected subscriber
    for h in senders {
        h.await.unwrap();
    }

    // Wait for 4 TurnDone events (1 initial + 3 follow-ups) across all 3 live subscribers.
    let mut seq_lists = Vec::new();
    for mut rx in rxs.drain(..) {
        let mut done_count = 0usize;
        let mut seqs = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(ev) = rx.recv().await {
                seqs.push(ev.seq);
                if matches!(ev.event, AgentEvent::TurnDone { .. }) {
                    done_count += 1;
                    if done_count == 4 {
                        break;
                    }
                } else if let AgentEvent::TurnError(err) = &ev.event {
                    panic!("unexpected TurnError: {err}");
                }
            }
        })
        .await
        .expect("timed out waiting for 4 TurnDone events");
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "non-monotonic seq: {seqs:?}");
        seq_lists.push(seqs);
    }

    assert_eq!(seq_lists[0], seq_lists[1]);
    assert_eq!(seq_lists[1], seq_lists[2]);

    let all_bodies = bodies.lock().unwrap().join("\n");
    for i in 0..3 {
        assert!(
            all_bodies.contains(&format!("STEER_MSG_{i}")),
            "missing STEER_MSG_{i} in LLM requests"
        );
        assert!(
            all_bodies.contains(&format!("FOLLOW_UP_{i}")),
            "missing FOLLOW_UP_{i} in LLM requests"
        );
    }

    runtime.shutdown_all().await;
    let _ = stop_daemon.send(());
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!(
        "[stress_runtime] 4 turns (3 concurrent steers + 3 followups, 3 live + 1 dropped subscriber) \
         events={} elapsed={:?}",
        seq_lists[0].len(),
        started.elapsed()
    );
}
