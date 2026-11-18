//! Phase 3 stress tests: 250-step agent loop + compaction + FD/RSS profile,
//! and 8-way parallel subagent fan-out + mid-flight abort.

use super::asks::AskBroker;
use super::config::TurnConfig;
use super::r#loop::{execute_loop, LoopOptions};
use super::subagent::runner::{execute_delegate, SubagentTaskInput};
use super::subagent::SubagentHost;
use crate::auth::{Credential, ModelRef, ProviderProtocol, StoredProvider};
use crate::hooks::HookEngine;
use crate::permissions::{PermissionEngine, PermissionMode};
use crate::session::{ChatMessage, ChatRole};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::mpsc;

fn short_tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = PathBuf::from("/tmp").join(format!(
        "eas-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn proc_rss_kb() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                let rest = l.strip_prefix("VmRSS:")?.trim();
                rest.split_whitespace().next()?.parse::<usize>().ok()
            })
        })
        .unwrap_or(0)
}

fn proc_fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(|it| it.count())
        .unwrap_or(0)
}

async fn start_test_daemon(data_dir: &Path) -> tokio::sync::oneshot::Sender<()> {
    let _ = envoy_core::AgentConfigStore::open(data_dir.to_path_buf()).unwrap();
    let socket = crate::daemon::socket_path(data_dir);
    let listener = UnixListener::bind(&socket).unwrap();
    let server = envoy_service::AgentServer::open_store(data_dir.to_path_buf()).unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        crate::daemon::run_server(server, listener, socket, None, async {
            let _ = stop_rx.await;
        })
        .await;
    });
    stop_tx
}

async fn read_http_req(sock: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
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
async fn stress_250_step_loop_with_compaction_and_fd_rss_profile() {
    let dir = short_tmpdir("250");
    let data = dir.join("data");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();

    // 16 KiB workspace file read repeatedly to grow prompt context and trigger compaction.
    let big_line = "stress-payload-line-0123456789abcdef0123456789abcdef\n";
    std::fs::write(ws.join("big.txt"), big_line.repeat(320)).unwrap();

    let stop_daemon = start_test_daemon(&data).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_srv = Arc::clone(&hits);
    let max_body_bytes = Arc::new(AtomicUsize::new(0));
    let max_body_srv = Arc::clone(&max_body_bytes);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let h = Arc::clone(&hits_srv);
            let mb = Arc::clone(&max_body_srv);
            tokio::spawn(async move {
                let body = read_http_req(&mut sock).await;
                mb.fetch_max(body.len(), Ordering::Relaxed);
                let step = h.fetch_add(1, Ordering::SeqCst);
                let sse = if body.contains("Summarize the conversation")
                    || body.contains("compact") && step > 240
                {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"compacted summary ok\"}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20}}\n\ndata: [DONE]\n\n".to_string()
                } else if step < 240 {
                    format!(
                        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c{step}\",\"function\":{{\"name\":\"read\",\"arguments\":\"{{\\\"path\\\":\\\"big.txt\\\",\\\"limit\\\":120}}\"}}}}]}}}}],\"usage\":{{\"prompt_tokens\":12000,\"completion_tokens\":25}}}}\n\ndata: [DONE]\n\n"
                    )
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"completed 240 steps\"}}],\"usage\":{\"prompt_tokens\":500,\"completion_tokens\":15}}\n\ndata: [DONE]\n\n".to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}",
                    sse.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let rss_before = proc_rss_kb();
    let fd_before = proc_fd_count();
    let started = Instant::now();

    let cfg = TurnConfig {
        data_dir: data.clone(),
        conduit: None,
        local_socket: None,
        target: "local".into(),
        cwd: ws.clone(),
        provider: StoredProvider {
            id: "test".into(),
            name: "Test".into(),
            protocol: ProviderProtocol::OpenAiChat,
            base_url: format!("http://{addr}/v1"),
            credential: Credential::ApiKey { key: "sk-test".into() },
            custom_models: vec!["gpt-5.4".into()],
            model_routes: BTreeMap::new(),
            updated_ms: 0,
        },
        model: ModelRef {
            provider_id: "test".into(),
            model_id: "gpt-5.4".into(),
        },
        history: vec![ChatMessage {
            role: ChatRole::User,
            content: "Run 240 read steps".into(),
            reasoning: None,
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
        }],
        asks: AskBroker::new_headless(),
        steer: Arc::new(Mutex::new(vec![])),
        permissions: PermissionEngine::new(
            data.clone(),
            ws.clone(),
            "s-250".into(),
            "local".into(),
            PermissionMode::Auto,
        ),
        monitors: None,
        hooks: None,
        hook_state: BTreeMap::new(),
        hook_state_sink: None,
        turn_seq: 1,
        max_steps: Some(250),
    };

    let opts = LoopOptions {
        system: "sys".into(),
        max_steps: 250,
        depth: 0,
        allowed_tools: None,
        connect_mcp: false,
        subagent_host: None,
        subagent_name: None,
    };

    let (tx, mut rx) = mpsc::unbounded_channel();
    let drain = tokio::spawn(async move {
        let mut count = 0usize;
        while rx.recv().await.is_some() {
            count += 1;
        }
        count
    });

    let outcome = execute_loop(cfg, opts, &tx).await.unwrap();
    drop(tx);
    let event_count = drain.await.unwrap();

    let elapsed = started.elapsed();
    let rss_after = proc_rss_kb();
    let fd_after = proc_fd_count();
    let total_hits = hits.load(Ordering::SeqCst);

    assert!(total_hits >= 241, "expected >=241 LLM calls, got {total_hits}");
    assert!(
        outcome
            .messages
            .last()
            .is_some_and(|m| m.content.contains("completed 240 steps")),
        "unexpected final msg: {:?}",
        outcome.messages.last()
    );
    assert!(
        fd_after <= fd_before + 6,
        "FD leak suspected: before={fd_before}, after={fd_after}"
    );
    let _ = stop_daemon.send(());
    let _ = std::fs::remove_dir_all(&dir);

    eprintln!(
        "[stress_250] steps=240 llm_hits={total_hits} events={event_count} elapsed={elapsed:?} \
         rss_kb={rss_before}->{rss_after} fds={fd_before}->{fd_after} max_req_bytes={}",
        max_body_bytes.load(Ordering::Relaxed)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_8way_parallel_subagents_and_midflight_abort() {
    let dir = short_tmpdir("sub8");
    let data = dir.join("data");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("note.txt"), "hello subagent").unwrap();

    let stop_daemon = start_test_daemon(&data).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stall = Arc::new(AtomicUsize::new(0));
    let stall_srv = Arc::clone(&stall);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let st = Arc::clone(&stall_srv);
            tokio::spawn(async move {
                let body = read_http_req(&mut sock).await;
                if st.load(Ordering::SeqCst) > 0 {
                    // Hold connection open only until the client aborts/closes it.
                    let mut buf = [0u8; 64];
                    while let Ok(n) = sock.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                    }
                    return;
                }
                let sse = if body.contains("note.txt") && body.contains("hello subagent") {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"subagent verified\"}}]}\n\ndata: [DONE]\n\n"
                } else {
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"sc1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"note.txt\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}",
                    sse.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let host = SubagentHost {
        data_dir: data.clone(),
        conduit: None,
        local_socket: None,
        target: "local".into(),
        cwd: ws.clone(),
        provider: StoredProvider {
            id: "test".into(),
            name: "Test".into(),
            protocol: ProviderProtocol::OpenAiChat,
            base_url: format!("http://{addr}/v1"),
            credential: Credential::ApiKey { key: "sk-test".into() },
            custom_models: vec!["m1".into()],
            model_routes: BTreeMap::new(),
            updated_ms: 0,
        },
        model: ModelRef {
            provider_id: "test".into(),
            model_id: "m1".into(),
        },
        asks: AskBroker::new_headless(),
        permissions: PermissionEngine::new(
            data.clone(),
            ws.clone(),
            "s-sub8".into(),
            "local".into(),
            PermissionMode::Auto,
        ),
        hooks: Some(HookEngine::new(data.clone(), ws.clone())),
        turn_seq: 1,
    };

    let tasks: Vec<SubagentTaskInput> = (0..8)
        .map(|i| SubagentTaskInput {
            agent: "explore".into(),
            prompt: format!("Check note.txt task {i}"),
            description: Some(format!("task-{i}")),
            model_override: None,
        })
        .collect();

    let (tx, _rx) = mpsc::unbounded_channel();
    let t0 = Instant::now();
    let (report, all_ok, _state) = execute_delegate(&host, "call-1", tasks.clone(), BTreeMap::new(), &tx)
        .await
        .unwrap();
    let batch_elapsed = t0.elapsed();
    assert!(all_ok, "expected all 8 parallel subagents ok:\n{report}");
    for i in 0..8 {
        assert!(report.contains(&format!("task-{i}")));
    }

    // Now test mid-flight abort of an 8-way stalled batch.
    stall.store(1, Ordering::SeqCst);
    let fd_before = proc_fd_count();
    let stalled_fut = execute_delegate(&host, "call-2", tasks, BTreeMap::new(), &tx);
    let res = tokio::time::timeout(Duration::from_millis(120), stalled_fut).await;
    assert!(res.is_err(), "stalled 8-way batch should time out cleanly");
    tokio::time::sleep(Duration::from_millis(30)).await;
    let fd_after = proc_fd_count();
    assert!(
        fd_after <= fd_before + 4,
        "FD leak after subagent abort: {fd_before}->{fd_after}"
    );

    let _ = stop_daemon.send(());
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("[stress_sub8] 8-way fan-out={batch_elapsed:?} fds_after_abort={fd_before}->{fd_after}");
}
