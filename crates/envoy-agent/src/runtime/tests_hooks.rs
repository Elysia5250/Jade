use super::*;
use crate::agent::{AgentEvent, TurnConfig};
use crate::auth::{Credential, ModelRef, ProviderProtocol, StoredProvider};
use crate::hooks::config::user_hooks_path;
use crate::permissions::{PermissionEngine, PermissionMode};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::mpsc;

fn short_tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = PathBuf::from("/tmp").join(format!(
        "eh-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
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

async fn start_mock_llm() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let bodies_srv = Arc::clone(&bodies);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            while let Ok(n) = sock.read(&mut tmp).await {
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
            let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            bodies_srv.lock().unwrap().push(body);

            let sse =
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}",
                sse.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    (addr, bodies)
}

#[tokio::test]
async fn session_start_and_user_prompt_submit_thread_hook_context_and_block() {
    let dir = short_tmpdir("rt1");
    let data = dir.join("data");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    let _stop = start_test_daemon(&data).await;
    let (addr, bodies) = start_mock_llm().await;

    std::fs::write(
        user_hooks_path(&data),
        r#"{
          "hooks": {
            "SessionStart": [
              {
                "id": "init-session",
                "command": "echo '{\"statePatch\":{\"init\":\"ok\"},\"additionalContext\":\"session-banner\"}'"
              }
            ],
            "UserPromptSubmit": [
              {
                "id": "prompt-guard",
                "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); p=c.get(\"prompt\",\"\"); assert c[\"state\"][\"init\"]==\"ok\"; print(json.dumps({\"decision\":\"block\",\"reason\":\"no forbidden\"}) if \"FORBIDDEN\" in p else json.dumps({\"statePatch\":{\"prompt_seen\":True},\"updatedPrompt\":\"rewritten: \"+p}))'"
              }
            ]
          }
        }"#,
    )
    .unwrap();

    let runtime = Runtime::new(data.clone());
    let live = runtime
        .ensure_with_hooks("s1".into(), ws.clone(), "local".into(), vec![])
        .await;
    {
        let st = live.lock().unwrap();
        assert_eq!(st.pending_session_context, vec!["session-banner"]);
        assert_eq!(
            st.hook_state.lock().unwrap().get("init"),
            Some(&json!("ok"))
        );
    }

    let provider = StoredProvider {
        id: "test".into(),
        name: "Test".into(),
        protocol: ProviderProtocol::OpenAiChat,
        base_url: format!("http://{addr}/v1"),
        credential: Credential::ApiKey {
            key: "sk-test".into(),
        },
        custom_models: vec!["m1".into()],
        model_routes: BTreeMap::new(),
        updated_ms: 0,
    };

    let mk_cfg = |turn_seq: u64, live: &Arc<Mutex<super::server::Live>>| {
        let st = live.lock().unwrap();
        let hook_state = st.hook_state.lock().unwrap().clone();
        TurnConfig {
            data_dir: data.clone(),
            conduit: None,
            local_socket: None,
            target: "local".into(),
            cwd: ws.clone(),
            provider: provider.clone(),
            model: ModelRef {
                provider_id: "test".into(),
                model_id: "m1".into(),
            },
            history: st.messages.clone(),
            asks: Arc::clone(&st.asks),
            steer: Arc::clone(&st.steer),
            permissions: PermissionEngine::new(
                data.clone(),
                ws.clone(),
                "s1".into(),
                "local".into(),
                PermissionMode::Default,
            ),
            monitors: None,
            hooks: Some(Arc::clone(&st.hooks)),
            hook_state,
            hook_state_sink: Some(Arc::clone(&st.hook_state)),
            turn_seq,
            max_steps: None,
        }
    };

    // Blocked UserPromptSubmit emits TurnError and leaves messages empty.
    let (tx1, mut rx1) = mpsc::unbounded_channel();
    Arc::clone(&runtime)
        .run_turn_with_hooks(
            "s1".into(),
            Arc::clone(&live),
            Some("FORBIDDEN input".into()),
            mk_cfg(1, &live),
            tx1,
        )
        .await;
    let mut saw_block_err = false;
    while let Ok(ev) = rx1.try_recv() {
        if let AgentEvent::TurnError(e) = ev {
            assert!(e.contains("no forbidden"));
            saw_block_err = true;
        }
    }
    assert!(saw_block_err);
    assert!(live.lock().unwrap().messages.is_empty());

    // Allowed prompt consumes pending SessionStart context, rewrites prompt, and calls LLM.
    let (tx2, mut rx2) = mpsc::unbounded_channel();
    Arc::clone(&runtime)
        .run_turn_with_hooks(
            "s1".into(),
            Arc::clone(&live),
            Some("hello world".into()),
            mk_cfg(2, &live),
            tx2,
        )
        .await;
    let mut saw_done = false;
    while let Ok(ev) = rx2.try_recv() {
        if matches!(ev, AgentEvent::TurnDone { .. }) {
            saw_done = true;
        }
    }
    assert!(saw_done);
    assert!(live.lock().unwrap().pending_session_context.is_empty());
    assert_eq!(
        live.lock()
            .unwrap()
            .hook_state
            .lock()
            .unwrap()
            .get("prompt_seen"),
        Some(&json!(true))
    );
    let sent = bodies.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("rewritten: hello world"));
    assert!(sent[0].contains("session-banner"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn post_rewind_and_session_end_bounded_lifecycle() {
    let dir = short_tmpdir("rt2");
    let data = dir.join("data");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    let log_file = dir.join("end.log");
    let file = ws.join("a.txt");
    std::fs::write(&file, "v0\n").unwrap();

    let hook_json = format!(
        r#"{{
          "hooks": {{
            "PostRewind": [
              {{
                "id": "post-rw",
                "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); print(json.dumps({{\"statePatch\":{{\"rewound\":c[\"rewind_checkpoint_id\"]}}}}))'"
              }}
            ],
            "SessionEnd": [
              {{
                "id": "log-end",
                "priority": 1,
                "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); open(\"{log_path}\",\"a\").write(c[\"session_id\"]+\":\"+str(c.get(\"shutdown_reason\"))+\"\\n\")'"
              }},
              {{
                "id": "slow-cleanup",
                "priority": 2,
                "command": "sleep 5"
              }}
            ]
          }}
        }}"#,
        log_path = log_file.display()
    );
    std::fs::write(user_hooks_path(&data), hook_json).unwrap();

    let runtime = Runtime::new(data.clone());
    let live1 = runtime
        .ensure_with_hooks("s1".into(), ws.clone(), "local".into(), vec![])
        .await;
    let _live2 = runtime
        .ensure_with_hooks("s2".into(), ws.clone(), "local".into(), vec![])
        .await;

    super::ops::record_turn_checkpoint(&live1, Some("turn 0"));
    let expected_cp = {
        let mut st = live1.lock().unwrap();
        let cp = st.active_checkpoint_id.unwrap();
        st.checkpoints.record_file_change(
            cp,
            file.display().to_string(),
            true,
            "v0\n".into(),
            true,
        );
        st.active_checkpoint_id = None;
        cp
    };
    std::fs::write(&file, "v1\n").unwrap();

    runtime.rewind("s1", None).await.unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "v0\n");
    assert_eq!(
        live1
            .lock()
            .unwrap()
            .hook_state
            .lock()
            .unwrap()
            .get("rewound"),
        Some(&json!(expected_cp))
    );

    // Explicit CloseSession fires SessionEnd for s1 (slow hook bounded by 1500ms).
    let start_close = Instant::now();
    assert!(runtime.close_session("s1").await.unwrap());
    assert!(start_close.elapsed() < Duration::from_millis(2400));

    // Daemon shutdown_all runs bounded SessionEnd across remaining sessions within ~2s.
    let start_shutdown = Instant::now();
    runtime.shutdown_all().await;
    assert!(start_shutdown.elapsed() < Duration::from_millis(2500));

    let log = std::fs::read_to_string(&log_file).unwrap();
    assert!(log.contains("s1:session_closed"));
    assert!(log.contains("s2:daemon_shutdown"));
    // s1 already ended cleanly, must not fire twice on shutdown_all
    assert_eq!(log.lines().filter(|l| l.starts_with("s1:")).count(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}
