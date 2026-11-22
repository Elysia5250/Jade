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

#[test]
fn interactive_default_tool_steps_is_100() {
    assert_eq!(super::r#loop::DEFAULT_TOOL_STEPS, 100);
}

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

    let dir = std::env::temp_dir().join(format!("jade-loop-retry-{}", std::process::id()));
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
        fetched_models: vec![],
        model_routes: BTreeMap::new(),
        extra_headers: Default::default(),
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

            reasoning_blocks: Vec::new(),
            images: Vec::new(),
        },
        ChatMessage {
            role: ChatRole::User,
            content: "latest request".into(),
            reasoning: None,
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,

            reasoning_blocks: Vec::new(),
            images: Vec::new(),
        },
    ];
    let cfg = TurnConfig {
        data_dir: dir.clone(),
        conduit: None,
        local_socket: Some(dir.join("unused.sock")),
        local_server: None,
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
        mcp_pool: None,
        tools: None,
        providers: None,
        hooks: None,
        hook_state: BTreeMap::new(),
        hook_state_sink: None,
        turn_seq: 0,
        max_steps: None,
        usage: None,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut prov = provider;
    let mut plan = resolve_budget(&dir, "test", "test-model");
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    let mut cache_read = 0u64;
    let mut cache_create = 0u64;
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
        &mut total_in,
        &mut total_out,
        &mut cache_read,
        &mut cache_create,
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

#[tokio::test]
async fn custom_tool_and_provider_registries_run_end_to_end() {
    use super::providers::{ModelProvider, ProviderRegistry, StreamReply, StreamRequest};
    use super::r#loop::{execute_loop, LoopOptions};
    use super::tools::{Tool, ToolExecContext, ToolExecResult, ToolRegistry, ToolSpec};
    use crate::session::ToolCall;
    use futures::future::BoxFuture;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct EchoTool;
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "custom_echo".into(),
                description: "Echo a custom payload".into(),
                parameters: json!({
                    "type": "object",
                    "properties": { "msg": { "type": "string" } },
                    "required": ["msg"]
                }),
                mutates: false,
            }
        }
        fn summarize(&self, args: &Value) -> (String, String) {
            (format!("echo {}", args["msg"].as_str().unwrap_or("")), String::new())
        }
        fn execute<'a>(
            &'a self,
            _ctx: &'a ToolExecContext<'a>,
            args: Value,
        ) -> BoxFuture<'a, anyhow::Result<ToolExecResult>> {
            Box::pin(async move {
                let msg = args["msg"].as_str().unwrap_or("");
                Ok(ToolExecResult::ok(format!("echoed:{msg}")))
            })
        }
    }

    struct FakeProvider {
        calls: AtomicUsize,
        saw_custom_schema: Mutex<bool>,
    }
    impl ModelProvider for FakeProvider {
        fn key(&self) -> &str {
            "fake-custom-prov"
        }
        fn stream<'a>(&'a self, req: StreamRequest<'a>) -> BoxFuture<'a, anyhow::Result<StreamReply>> {
            Box::pin(async move {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if req
                    .extra_tools
                    .iter()
                    .any(|t| t["function"]["name"] == "custom_echo")
                {
                    *self.saw_custom_schema.lock().unwrap() = true;
                }
                if n == 0 {
                    Ok(StreamReply {
                        tool_calls: vec![ToolCall {
                            id: "tc-1".into(),
                            name: "custom_echo".into(),
                            arguments: json!({"msg": "hello-trait"}).to_string(),
                        }],
                        input_tokens: 100,
                        output_tokens: 20,
                        calibrate: true,
                        ..StreamReply::default()
                    })
                } else {
                    let tool_out = req
                        .messages
                        .iter()
                        .rev()
                        .find(|m| m.role == ChatRole::Tool)
                        .map(|m| m.content.clone())
                        .unwrap_or_default();
                    Ok(StreamReply {
                        text: format!("final:{tool_out}"),
                        input_tokens: 120,
                        output_tokens: 15,
                        calibrate: true,
                        ..StreamReply::default()
                    })
                }
            })
        }
    }

    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let dir = std::env::temp_dir().join(format!(
        "jade-loop-registries-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut tools = ToolRegistry::with_builtins();
    tools.register(Arc::new(EchoTool));
    assert!(!tools.is_mutating("custom_echo"));
    assert!(tools.is_mutating("exec"));

    let fake = Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        saw_custom_schema: Mutex::new(false),
    });
    let mut providers = ProviderRegistry::with_builtins();
    providers.register(fake.clone());

    let provider = StoredProvider {
        id: "fake-custom-prov".into(),
        name: "Fake".into(),
        protocol: ProviderProtocol::OpenAiChat,
        base_url: "http://127.0.0.1:1/v1".into(),
        credential: Credential::ApiKey { key: "k".into() },
        custom_models: vec!["m1".into()],
        fetched_models: Vec::new(),
        model_routes: BTreeMap::new(),
        extra_headers: BTreeMap::new(),
        updated_ms: 0,
    };
    let cfg = TurnConfig {
        data_dir: dir.clone(),
        conduit: None,
        local_socket: Some(dir.join("unused.sock")),
        local_server: None,
        target: "local".into(),
        cwd: dir.clone(),
        provider,
        model: ModelRef {
            provider_id: "fake-custom-prov".into(),
            model_id: "m1".into(),
        },
        history: vec![ChatMessage {
            role: ChatRole::User,
            content: "call custom_echo".into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            reasoning_blocks: Vec::new(),
            images: Vec::new(),
        }],
        asks: AskBroker::new_headless(),
        steer: Arc::new(Mutex::new(Vec::new())),
        permissions: PermissionEngine::new(
            dir.clone(),
            dir.clone(),
            "s-reg".into(),
            "local".into(),
            PermissionMode::Turbo,
        ),
        monitors: None,
        mcp_pool: None,
        tools: Some(Arc::new(tools)),
        providers: Some(Arc::new(providers)),
        hooks: None,
        hook_state: BTreeMap::new(),
        hook_state_sink: None,
        turn_seq: 1,
        max_steps: Some(4),
        usage: None,
    };
    let opts = LoopOptions {
        system: "sys".into(),
        max_steps: 4,
        depth: 0,
        allowed_tools: None,
        connect_mcp: false,
        subagent_host: None,
        subagent_name: None,
    };
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = execute_loop(cfg, opts, &tx).await.unwrap();
    assert!(*fake.saw_custom_schema.lock().unwrap());
    let last = outcome.messages.last().unwrap();
    assert_eq!(last.content, "final:echoed:hello-trait");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn model_driven_plan_mode_lifecycle_enforces_plan_file_and_restores_mode() {
    use super::providers::{ModelProvider, ProviderRegistry, StreamReply, StreamRequest};
    use super::r#loop::{execute_loop, LoopOptions};
    use crate::auth::{Credential, ModelRef, ProviderProtocol, StoredProvider};
    use crate::hooks::config::user_hooks_path;
    use crate::hooks::HookEngine;
    use crate::permissions::PermissionMode;
    use crate::session::ToolCall;
    use futures::future::BoxFuture;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct PlanFlowProvider {
        step: AtomicUsize,
        saw_plan_env: Mutex<bool>,
        saw_blocked_write: Mutex<bool>,
    }
    impl ModelProvider for PlanFlowProvider {
        fn key(&self) -> &str {
            "fake-plan-prov"
        }
        fn stream<'a>(&'a self, req: StreamRequest<'a>) -> BoxFuture<'a, anyhow::Result<StreamReply>> {
            Box::pin(async move {
                let n = self.step.fetch_add(1, Ordering::SeqCst);
                let has_mode_tool = crate::agent::tools::openai_tools_json_filtered(
                    req.permissions,
                    req.tool_filter,
                    req.extra_tools,
                )
                .iter()
                .any(|t| t["function"]["name"] == "mode");
                assert!(has_mode_tool, "interactive root turn should expose mode tool");

                let reply = match n {
                    // Step 0: enter_plan + write non-plan file in the SAME batch.
                    0 => StreamReply {
                        tool_calls: vec![
                            ToolCall {
                                id: "c-enter".into(),
                                name: "mode".into(),
                                arguments: r#"{"action":"enter_plan","reason":"make a plan"}"#.into(),
                            },
                            ToolCall {
                                id: "c-bad-write".into(),
                                name: "write".into(),
                                arguments: r#"{"path":"src.txt","content":"nope"}"#.into(),
                            },
                        ],
                        input_tokens: 100,
                        output_tokens: 10,
                        calibrate: true,
                        ..StreamReply::default()
                    },
                    // Step 1: verify plan environment note & write denial, then write plan file.
                    1 => {
                        if req.messages.iter().any(|m| m.content.contains("Current mode: `plan`")) {
                            *self.saw_plan_env.lock().unwrap() = true;
                        }
                        if req.messages.iter().any(|m| {
                            m.role == ChatRole::Tool
                                && m.tool_call_id.as_deref() == Some("c-bad-write")
                                && m.content.contains("plan mode is read-only")
                        }) {
                            *self.saw_blocked_write.lock().unwrap() = true;
                        }
                        StreamReply {
                            tool_calls: vec![ToolCall {
                                id: "c-plan-write".into(),
                                name: "write".into(),
                                arguments: r##"{"path":".jade/plans/feat.md","content":"# Plan\n1. Ship\n"}"##.into(),
                            }],
                            input_tokens: 120,
                            output_tokens: 12,
                            calibrate: true,
                            ..StreamReply::default()
                        }
                    }
                    // Step 2: request exit_plan -> asks user to approve restore to accept-edits.
                    2 => StreamReply {
                        tool_calls: vec![ToolCall {
                            id: "c-exit".into(),
                            name: "mode".into(),
                            arguments: r#"{"action":"exit_plan","reason":"plan ready"}"#.into(),
                        }],
                        input_tokens: 140,
                        output_tokens: 10,
                        calibrate: true,
                        ..StreamReply::default()
                    },
                    // Step 3: now back in accept-edits, write workspace file.
                    3 => StreamReply {
                        tool_calls: vec![ToolCall {
                            id: "c-good-write".into(),
                            name: "write".into(),
                            arguments: r#"{"path":"src.txt","content":"shipped"}"#.into(),
                        }],
                        input_tokens: 150,
                        output_tokens: 10,
                        calibrate: true,
                        ..StreamReply::default()
                    },
                    _ => StreamReply {
                        text: "done".into(),
                        input_tokens: 160,
                        output_tokens: 5,
                        calibrate: true,
                        ..StreamReply::default()
                    },
                };
                Ok(reply)
            })
        }
    }

    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let root = std::env::temp_dir().join(format!(
        "jade-loop-mode-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    let data = root.join("data");
    let work = root.join("work");
    let hook_log = root.join("hook_modes.log");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        user_hooks_path(&data),
        serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "id": "log-write-mode",
                    "matcher": "write",
                    "command": format!(
                        "python3 -c 'import sys,json; d=json.load(sys.stdin); open({:?}, \"a\").write(d[\"permission_mode\"] + \"\\n\")'",
                        hook_log.display().to_string()
                    )
                }]
            }
        })
        .to_string(),
    )
    .unwrap();

    let fake = Arc::new(PlanFlowProvider {
        step: AtomicUsize::new(0),
        saw_plan_env: Mutex::new(false),
        saw_blocked_write: Mutex::new(false),
    });
    let mut providers = ProviderRegistry::with_builtins();
    providers.register(fake.clone());

    let perms = PermissionEngine::new(
        data.clone(),
        work.clone(),
        "s-plan-loop".into(),
        "local".into(),
        PermissionMode::AcceptEdits,
    );
    let asks = AskBroker::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let answerer = asks.clone();
    let watcher = tokio::spawn(async move {
        let mut saw_say = false;
        let mut modes = Vec::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::Say { text, .. } if text.contains("accept-edits -> plan") => {
                    saw_say = true;
                }
                AgentEvent::PermissionModeChanged { mode } => modes.push(mode),
                AgentEvent::Ask { id, options, .. } => {
                    assert_eq!(options[0].label, "Approve (accept-edits)");
                    answerer.answer(id, options[0].label.clone());
                }
                _ => {}
            }
        }
        (saw_say, modes)
    });

    let provider = StoredProvider {
        id: "fake-plan-prov".into(),
        name: "Fake".into(),
        protocol: ProviderProtocol::OpenAiChat,
        base_url: "http://127.0.0.1:1/v1".into(),
        credential: Credential::ApiKey { key: "k".into() },
        custom_models: vec!["m1".into()],
        fetched_models: Vec::new(),
        model_routes: BTreeMap::new(),
        extra_headers: BTreeMap::new(),
        updated_ms: 0,
    };
    let jobs_dir = root.join("jobs");
    std::fs::create_dir_all(&jobs_dir).unwrap();
    let host = Arc::new(jade_service::StoreHost::open(data.clone()).unwrap());
    let local_server = jade_service::AgentServer::new(jobs_dir, host);
    let cfg = TurnConfig {
        data_dir: data.clone(),
        conduit: None,
        local_socket: None,
        local_server: Some(local_server),
        target: "local".into(),
        cwd: work.clone(),
        provider,
        model: ModelRef {
            provider_id: "fake-plan-prov".into(),
            model_id: "m1".into(),
        },
        history: vec![ChatMessage::text(ChatRole::User, "make a plan then ship it")],
        asks,
        steer: Arc::new(Mutex::new(Vec::new())),
        permissions: perms.clone(),
        monitors: None,
        mcp_pool: None,
        tools: None,
        providers: Some(Arc::new(providers)),
        hooks: Some(HookEngine::new(data.clone(), work.clone())),
        hook_state: BTreeMap::new(),
        hook_state_sink: None,
        turn_seq: 1,
        max_steps: Some(8),
        usage: None,
    };
    let opts = LoopOptions {
        system: "sys".into(),
        max_steps: 8,
        depth: 0,
        allowed_tools: None,
        connect_mcp: false,
        subagent_host: None,
        subagent_name: None,
    };
    let outcome = execute_loop(cfg, opts, &tx).await.unwrap();
    drop(tx);
    let (saw_say, modes) = watcher.await.unwrap();

    assert!(saw_say, "downgrade to plan should emit a say event");
    assert_eq!(modes, vec![PermissionMode::Plan, PermissionMode::AcceptEdits]);
    assert!(*fake.saw_plan_env.lock().unwrap());
    assert!(*fake.saw_blocked_write.lock().unwrap());
    assert_eq!(perms.mode(), PermissionMode::AcceptEdits);
    assert_eq!(std::fs::read_to_string(work.join("src.txt")).unwrap(), "shipped");
    assert_eq!(outcome.messages.last().unwrap().content, "done");

    // PreToolUse for the same-batch write after enter_plan must observe "plan",
    // and the final write after exit_plan must observe "accept-edits".
    let logged_modes: Vec<String> = std::fs::read_to_string(&hook_log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(logged_modes, vec!["plan", "plan", "accept-edits"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn headless_loop_hides_and_blocks_mode_tool() {
    use super::providers::{ModelProvider, ProviderRegistry, StreamReply, StreamRequest};
    use super::r#loop::{execute_loop, LoopOptions};
    use crate::auth::{Credential, ModelRef, ProviderProtocol, StoredProvider};
    use crate::permissions::PermissionMode;
    use crate::session::ToolCall;
    use futures::future::BoxFuture;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct HeadlessModeProvider {
        step: AtomicUsize,
        mode_exposed: Mutex<bool>,
    }
    impl ModelProvider for HeadlessModeProvider {
        fn key(&self) -> &str {
            "fake-hl-mode"
        }
        fn stream<'a>(&'a self, req: StreamRequest<'a>) -> BoxFuture<'a, anyhow::Result<StreamReply>> {
            Box::pin(async move {
                if crate::agent::tools::openai_tools_json_filtered(
                    req.permissions,
                    req.tool_filter,
                    req.extra_tools,
                )
                .iter()
                .any(|t| t["function"]["name"] == "mode")
                {
                    *self.mode_exposed.lock().unwrap() = true;
                }
                let n = self.step.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Ok(StreamReply {
                        tool_calls: vec![ToolCall {
                            id: "c-hl".into(),
                            name: "mode".into(),
                            arguments: r#"{"action":"enter_plan"}"#.into(),
                        }],
                        input_tokens: 50,
                        output_tokens: 5,
                        calibrate: true,
                        ..StreamReply::default()
                    })
                } else {
                    let tool_out = req
                        .messages
                        .iter()
                        .rev()
                        .find(|m| m.role == ChatRole::Tool)
                        .map(|m| m.content.clone())
                        .unwrap_or_default();
                    Ok(StreamReply {
                        text: tool_out,
                        input_tokens: 60,
                        output_tokens: 5,
                        calibrate: true,
                        ..StreamReply::default()
                    })
                }
            })
        }
    }

    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let dir = std::env::temp_dir().join(format!(
        "jade-hl-mode-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let fake = Arc::new(HeadlessModeProvider {
        step: AtomicUsize::new(0),
        mode_exposed: Mutex::new(false),
    });
    let mut providers = ProviderRegistry::with_builtins();
    providers.register(fake.clone());

    let perms = PermissionEngine::new(
        dir.clone(),
        dir.clone(),
        "s-hl-mode".into(),
        "local".into(),
        PermissionMode::Auto,
    );
    let provider = StoredProvider {
        id: "fake-hl-mode".into(),
        name: "Fake".into(),
        protocol: ProviderProtocol::OpenAiChat,
        base_url: "http://127.0.0.1:1/v1".into(),
        credential: Credential::ApiKey { key: "k".into() },
        custom_models: vec!["m1".into()],
        fetched_models: Vec::new(),
        model_routes: BTreeMap::new(),
        extra_headers: BTreeMap::new(),
        updated_ms: 0,
    };
    let cfg = TurnConfig {
        data_dir: dir.clone(),
        conduit: None,
        local_socket: Some(dir.join("unused.sock")),
        local_server: None,
        target: "local".into(),
        cwd: dir.clone(),
        provider,
        model: ModelRef {
            provider_id: "fake-hl-mode".into(),
            model_id: "m1".into(),
        },
        history: vec![ChatMessage::text(ChatRole::User, "make a plan")],
        asks: AskBroker::new_headless(),
        steer: Arc::new(Mutex::new(Vec::new())),
        permissions: perms.clone(),
        monitors: None,
        mcp_pool: None,
        tools: None,
        providers: Some(Arc::new(providers)),
        hooks: None,
        hook_state: BTreeMap::new(),
        hook_state_sink: None,
        turn_seq: 1,
        max_steps: Some(4),
        usage: None,
    };
    let opts = LoopOptions {
        system: "sys".into(),
        max_steps: 4,
        depth: 0,
        allowed_tools: None,
        connect_mcp: false,
        subagent_host: None,
        subagent_name: None,
    };
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = execute_loop(cfg, opts, &tx).await.unwrap();
    assert!(!*fake.mode_exposed.lock().unwrap());
    assert_eq!(perms.mode(), PermissionMode::Auto);
    assert!(
        outcome.messages.last().unwrap().content.contains("not enabled"),
        "{}",
        outcome.messages.last().unwrap().content
    );
    let _ = std::fs::remove_dir_all(&dir);
}
