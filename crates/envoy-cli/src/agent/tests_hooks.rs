use super::asks::AskBroker;
use super::config::TurnConfig;
use super::r#loop::{execute_loop, LoopOptions};
use super::subagent::runner::{execute_delegate, SubagentTaskInput};
use super::subagent::SubagentHost;
use crate::auth::{Credential, ModelRef, ProviderProtocol, StoredProvider};
use crate::hooks::config::user_hooks_path;
use crate::hooks::{HookEngine, MAX_STOP_HOOK_REENTRIES};
use crate::permissions::{PermissionEngine, PermissionMode};
use crate::session::{ChatMessage, ChatRole};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::mpsc;

fn short_tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = PathBuf::from("/tmp").join(format!(
        "e-{tag}-{:02x}{:02x}{:02x}{:02x}",
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

#[tokio::test]
async fn loop_hooks_pipeline_permission_recheck_and_stop_reentry() {
    let dir = short_tmpdir("lh");
    let data = dir.join("data");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("orig.txt"), "ORIG").unwrap();
    std::fs::write(ws.join("rewritten.txt"), "REWRITTEN_SECRET_OK").unwrap();
    std::fs::write(ws.join("bait.txt"), "BAIT").unwrap();
    std::fs::write(ws.join("secret.txt"), "SHOULD_NEVER_BE_READ").unwrap();

    // Deny reading *secret.txt via PermissionEngine even if PreToolUse rewrites input to it.
    std::fs::write(
        data.join("permissions.json"),
        r#"{ "deny": ["read(*secret.txt*)"] }"#,
    )
    .unwrap();

    std::fs::write(
        user_hooks_path(&data),
        r#"{
          "hooks": {
            "PreModelCall": [
              {
                "id": "pre-model",
                "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); n=c[\"state\"].get(\"pre_model\",0)+1; print(json.dumps({\"statePatch\":{\"pre_model\":n},\"additionalContext\":\"model-guard-\"+str(n)}))'"
              }
            ],
            "PostModelCall": [
              {
                "id": "post-model",
                "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); pt=c.get(\"planned_tools\",[]); log=c[\"state\"].get(\"planned_log\",[])+[pt]; print(json.dumps({\"statePatch\":{\"planned_log\":log}}))'"
              }
            ],
            "PreToolUse": [
              {
                "id": "rewrite-inputs",
                "matcher": "read",
                "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); p=c[\"tool_input\"][\"path\"]; new=\"rewritten.txt\" if p==\"orig.txt\" else \"secret.txt\"; print(json.dumps({\"updatedInput\":{\"path\":new}}))'"
              }
            ],
            "PostToolUse": [
              {
                "id": "post-ok",
                "matcher": "read",
                "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); print(json.dumps({\"statePatch\":{\"post_ok_path\":c[\"tool_input\"][\"path\"]},\"additionalContext\":\"verified-post-tool\"}))'"
              }
            ],
            "PostToolUseFailure": [
              {
                "id": "post-fail",
                "matcher": "read",
                "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); print(json.dumps({\"statePatch\":{\"failed_path\":c[\"tool_input\"][\"path\"]}}))'"
              }
            ],
            "Stop": [
              {
                "id": "stop-twice",
                "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); n=c[\"state\"].get(\"stop_calls\",0)+1; print(json.dumps({\"statePatch\":{\"stop_calls\":n},\"decision\":\"block\",\"reason\":\"verify \"+str(n)}))'"
              }
            ]
          }
        }"#,
    )
    .unwrap();

    let _stop = start_test_daemon(&data).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_srv = Arc::clone(&seen);
    let hits = Arc::new(Mutex::new(0usize));
    let hits_srv = Arc::clone(&hits);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let body = read_http_body(&mut sock).await;
            seen_srv.lock().unwrap().push(body);
            let n = {
                let mut g = hits_srv.lock().unwrap();
                *g += 1;
                *g
            };
            let sse = if n == 1 {
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[",
                    "{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"orig.txt\\\"}\"}},",
                    "{\"index\":1,\"id\":\"c2\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"bait.txt\\\"}\"}}",
                    "]}}]}\n\n",
                    "data: [DONE]\n\n"
                )
            } else {
                "data: {\"choices\":[{\"delta\":{\"content\":\"final answer\"}}]}\n\ndata: [DONE]\n\n"
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}",
                sse.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let cfg = TurnConfig {
        data_dir: data.clone(),
        conduit: None,
        target: "local".into(),
        cwd: ws.clone(),
        provider: StoredProvider {
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
        },
        model: ModelRef {
            provider_id: "test".into(),
            model_id: "m1".into(),
        },
        history: vec![ChatMessage {
            role: ChatRole::User,
            content: "inspect files".into(),
            reasoning: None,
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
        }],
        asks: AskBroker::new(),
        steer: Arc::new(Mutex::new(vec![])),
        permissions: PermissionEngine::new(
            data.clone(),
            ws.clone(),
            "s-loop".into(),
            "local".into(),
            PermissionMode::Default,
        ),
        monitors: None,
        hooks: Some(HookEngine::new(data.clone(), ws.clone())),
        hook_state: BTreeMap::new(),
        hook_state_sink: None,
        turn_seq: 1,
    };

    let opts = LoopOptions {
        system: "sys".into(),
        max_steps: 10,
        depth: 0,
        allowed_tools: None,
        connect_mcp: false,
        subagent_host: None,
        subagent_name: None,
    };
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = execute_loop(cfg, opts, &tx).await.unwrap();

    // Step 0 (tool calls) + Step 1 (first answer -> Stop block #1) + Step 2 (Stop block #2) + Step 3 (final, can_block=false)
    let expected_calls = 2 + (MAX_STOP_HOOK_REENTRIES as usize);
    assert_eq!(*hits.lock().unwrap(), expected_calls);

    let reqs = seen.lock().unwrap();
    assert!(reqs[0].contains("model-guard-1"));
    assert!(reqs[1].contains("REWRITTEN_SECRET_OK"));
    assert!(reqs[1].contains("verified-post-tool"));
    assert!(!reqs[1].contains("SHOULD_NEVER_BE_READ"));
    assert!(reqs[2].contains("attempt=\\\"1/2\\\""));
    assert!(reqs[3].contains("attempt=\\\"2/2\\\""));

    assert_eq!(
        outcome.hook_state.get("pre_model"),
        Some(&json!(expected_calls))
    );
    assert_eq!(
        outcome.hook_state.get("post_ok_path"),
        Some(&json!("rewritten.txt"))
    );
    assert_eq!(
        outcome.hook_state.get("failed_path"),
        Some(&json!("secret.txt"))
    );
    assert_eq!(
        outcome.hook_state.get("stop_calls"),
        Some(&json!(MAX_STOP_HOOK_REENTRIES as usize + 1))
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn subagent_start_block_fallback_stop_and_ordered_state_merge() {
    let dir = short_tmpdir("sh");
    let data = dir.join("data");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();

    let stop_log = dir.join("sub_stop.log");
    let hook_json = format!(
        r#"{{
          "hooks": {{
            "SubagentStart": [
              {{
                "id": "sub-start",
                "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); name=c.get(\"subagent_name\",\"\"); print(json.dumps({{\"decision\":\"block\",\"reason\":\"no review allowed\"}})) if name==\"review\" else print(json.dumps({{\"statePatch\":{{\"sub_started\":name}}}}))'"
              }}
            ],
            "SubagentStop": [
              {{
                "id": "sub-stop",
                "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); open(\"{stop_log}\",\"a\").write(str(c.get(\"subagent_name\"))+\":ok=\"+str(c.get(\"subagent_ok\"))+\":can_block=\"+str(c.get(\"can_block\"))+\"\\n\"); print(json.dumps({{\"statePatch\":{{\"child_result\":c.get(\"subagent_name\")}}}}))'"
              }}
            ]
          }}
        }}"#,
        stop_log = stop_log.display().to_string()
    );
    std::fs::write(user_hooks_path(&data), hook_json).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let body = read_http_body(&mut sock).await;
            let resp = if body.contains("FAIL_TASK") {
                let b = "{\"error\":{\"message\":\"upstream 500\"}}";
                format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{b}",
                    b.len()
                )
            } else {
                let sse =
                    "data: {\"choices\":[{\"delta\":{\"content\":\"child ok\"}}]}\n\ndata: [DONE]\n\n";
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}",
                    sse.len()
                )
            };
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let host = SubagentHost {
        data_dir: data.clone(),
        conduit: None,
        target: "local".into(),
        cwd: ws.clone(),
        provider: StoredProvider {
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
        },
        model: ModelRef {
            provider_id: "test".into(),
            model_id: "m1".into(),
        },
        asks: AskBroker::new(),
        permissions: PermissionEngine::new(
            data.clone(),
            ws.clone(),
            "s-sub".into(),
            "local".into(),
            PermissionMode::Default,
        ),
        hooks: Some(HookEngine::new(data.clone(), ws.clone())),
        turn_seq: 1,
    };

    let (tx, _rx) = mpsc::unbounded_channel();
    let tasks = vec![
        SubagentTaskInput {
            agent: "explore".into(),
            prompt: "succeed".into(),
            description: None,
            model_override: None,
        },
        SubagentTaskInput {
            agent: "review".into(),
            prompt: "blocked at start".into(),
            description: None,
            model_override: None,
        },
        SubagentTaskInput {
            agent: "explore".into(),
            prompt: "FAIL_TASK".into(),
            description: None,
            model_override: None,
        },
    ];

    let mut parent_state = BTreeMap::new();
    parent_state.insert("keep".into(), json!(true));
    let (out, all_ok, next_state) = execute_delegate(&host, "call_d", tasks, parent_state, &tx)
        .await
        .unwrap();

    assert!(!all_ok);
    assert!(out.contains("1/3 succeeded"));
    assert!(out.contains("no review allowed"));
    assert_eq!(next_state.get("keep"), Some(&json!(true)));
    assert_eq!(next_state.get("child_result"), Some(&json!("explore")));

    let log = std::fs::read_to_string(&stop_log).unwrap();
    assert!(log.contains("explore:ok=True:can_block=True"));
    assert!(log.contains("explore:ok=False:can_block=False"));
    assert!(!log.contains("review:"));

    let _ = std::fs::remove_dir_all(&dir);
}
