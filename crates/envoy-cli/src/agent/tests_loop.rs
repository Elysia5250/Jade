use super::asks::AskBroker;
use super::context::{resolve_budget, TokenCalibration};
use super::events::AgentEvent;
use super::r#loop::call_provider_with_overflow_retry;
use super::tools::ToolFilter;
use super::TurnConfig;
use crate::auth::{Credential, ModelRef, ProviderProtocol, StoredProvider};
use crate::permissions::{PermissionEngine, PermissionMode};
use crate::session::{ChatMessage, ChatRole, ToolCall};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[tokio::test]
async fn overflow_triggers_one_compaction_retry_and_second_failure_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(Mutex::new(0usize));
    let hits_srv = Arc::clone(&hits);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let n = {
                let mut g = hits_srv.lock().unwrap();
                *g += 1;
                *g
            };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let resp = if n == 1 {
                let body =
                    "{\"error\":{\"message\":\"maximum context length is 65536 tokens; too many\"}}";
                format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
            } else if n == 2 {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"## Goal\\n- Fixed\"}}]}\n\ndata: [DONE]\n\n";
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
            } else {
                let body = "{\"error\":{\"message\":\"context_length_exceeded\"}}";
                format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
            };
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let dir = std::env::temp_dir().join(format!("envoy-loop-retry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let provider = StoredProvider {
        id: "test".into(),
        name: "Test".into(),
        protocol: ProviderProtocol::OpenAiChat,
        base_url: format!("http://{addr}/v1"),
        credential: Credential::ApiKey {
            key: "sk-test".into(),
        },
        custom_models: vec![],
        model_routes: BTreeMap::new(),
        updated_ms: 0,
    };
    let mut history = vec![
        ChatMessage {
            role: ChatRole::User,
            content: "old request".into(),
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "exec".into(),
                arguments: "{}".into(),
            }],
            tool_call_id: None,
            tool_name: None,
        },
        ChatMessage {
            role: ChatRole::User,
            content: "latest request".into(),
            reasoning: None,
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
        },
    ];
    let cfg = TurnConfig {
        data_dir: dir.clone(),
        conduit: None,
        local_socket: None,
        target: "local".into(),
        cwd: dir.clone(),
        provider: provider.clone(),
        model: ModelRef {
            provider_id: "test".into(),
            model_id: "test-model".into(),
        },
        history: history.clone(),
        asks: AskBroker::new(),
        steer: Arc::new(Mutex::new(Vec::new())),
        permissions: PermissionEngine::new(
            dir.clone(),
            dir.clone(),
            "test-s".into(),
            "local".into(),
            PermissionMode::Default,
        ),
        monitors: None,
        hooks: None,
        hook_state: BTreeMap::new(),
        hook_state_sink: None,
        turn_seq: 0,
        max_steps: None,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut prov = provider;
    let mut plan = resolve_budget(&dir, "test", "test-model");
    let res = call_provider_with_overflow_retry(
        &reqwest::Client::new(),
        &cfg,
        &mut prov,
        "sys",
        &mut history,
        &ToolFilter::default(),
        &[],
        TokenCalibration::default(),
        &mut plan,
        None,
        0,
        &tx,
    )
    .await;

    assert!(res.is_err());
    assert_eq!(*hits.lock().unwrap(), 3);
    let mut saw_compacted = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, AgentEvent::Compacted { .. }) {
            saw_compacted = true;
        }
    }
    assert!(saw_compacted);
    assert_eq!(plan.hard_window, 65_536);
    let _ = std::fs::remove_dir_all(&dir);
}
