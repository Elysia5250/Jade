//! The single agent loop with pre-flight context budgeting and 1x overflow retry.

use super::config::TurnConfig;
use super::context::{
    estimate_turn_raw_tokens, is_context_overflow_error, parse_overflow_limit, prepare_context,
    record_model_cap, refresh_models_dev_if_stale, resolve_budget, TokenCalibration,
    BUILTIN_TOOLS_TOKENS_ESTIMATE,
};
use super::events::AgentEvent;
use super::prompt::build_system_prompt_for_session;
use super::providers::{stream_anthropic, stream_openai_chat, stream_openai_codex, StreamReply};
use super::tools::run_tool_call;
use crate::auth::{ensure_fresh_provider, ProviderProtocol, StoredProvider};
use crate::daemon;
use crate::mcp::client::McpManager;
use crate::session::{ChatMessage, ChatRole};
use anyhow::{bail, Result};
use envoy_service::{dialer_for, AgentDialer, Remote};
use std::time::Duration;
use tokio::sync::mpsc;

const MAX_TOOL_STEPS: usize = 25;

pub async fn run_agent_turn(cfg: TurnConfig, tx: mpsc::UnboundedSender<AgentEvent>) {
    let model_label = cfg.model.display();
    let _ = tx.send(AgentEvent::AssistantStart {
        model: model_label.clone(),
    });
    match execute_turn(cfg, &tx).await {
        Ok((messages, input_tokens, output_tokens, context_tokens)) => {
            let _ = tx.send(AgentEvent::TurnDone {
                messages,
                input_tokens,
                output_tokens,
                context_tokens,
            });
        }
        Err(e) => {
            let _ = tx.send(AgentEvent::TurnError(format!("{e:#}")));
        }
    }
}

async fn execute_turn(
    cfg: TurnConfig,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<(Vec<ChatMessage>, u64, u64, u64)> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()?;

    let bg_http = http.clone();
    let bg_dir = cfg.data_dir.clone();
    tokio::spawn(async move {
        refresh_models_dev_if_stale(&bg_http, &bg_dir).await;
    });

    let mut provider = ensure_fresh_provider(&http, &cfg.data_dir, cfg.provider.clone()).await?;
    let mut messages = cfg.history.clone();
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    #[allow(unused_assignments)]
    let mut context_tokens = 0u64;
    let mut calibration = TokenCalibration::default();

    if cfg.target == "local" || cfg.target.is_empty() {
        daemon::ensure(&cfg.data_dir).await?;
    }
    let remote: Remote<Box<dyn AgentDialer>> = Remote::new(dialer_for(
        &cfg.data_dir,
        &cfg.target,
        cfg.conduit.clone(),
    ));

    let mcp = McpManager::connect_configured(&cfg.data_dir, &cfg.cwd).await;
    let extra_tools = mcp.extra_tool_schemas();

    let system = build_system_prompt_for_session(
        &cfg.target,
        &cfg.cwd,
        &cfg.data_dir,
        cfg.permissions.session_id(),
    );
    let mut plan = resolve_budget(&cfg.data_dir, &cfg.provider.id, &cfg.model.model_id);

    for step in 0..MAX_TOOL_STEPS {
        if step > 0 {
            let _ = tx.send(AgentEvent::AssistantStart {
                model: cfg.model.display(),
            });
        }
        provider = ensure_fresh_provider(&http, &cfg.data_dir, provider).await?;

        if let Some(outcome) = prepare_context(
            &http,
            &cfg.data_dir,
            &provider,
            &cfg.model.model_id,
            &cfg.target,
            &cfg.cwd,
            &system,
            &mut messages,
            calibration,
            &plan,
            false,
            None,
            Some(tx),
        )
        .await?
        {
            if !outcome.pruned_only {
                let _ = tx.send(AgentEvent::Compacted {
                    messages: outcome.messages,
                    before_tokens: outcome.before_tokens,
                    after_tokens: outcome.after_tokens,
                    summary: outcome.summary,
                    auto: true,
                });
            }
        }

        let raw_est = estimate_turn_raw_tokens(&system, &messages, BUILTIN_TOOLS_TOKENS_ESTIMATE);
        let reply = call_provider_with_overflow_retry(
            &http,
            &cfg,
            &mut provider,
            &system,
            &mut messages,
            &extra_tools,
            calibration,
            &mut plan,
            tx,
        )
        .await?;

        calibration.observe(raw_est, reply.input_tokens);
        total_in += reply.input_tokens;
        total_out += reply.output_tokens;
        context_tokens = reply.input_tokens;

        let tool_calls = reply.tool_calls.clone();
        messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: reply.text,
            reasoning: if reply.reasoning.is_empty() {
                None
            } else {
                Some(reply.reasoning)
            },
            tool_calls: tool_calls.clone(),
            tool_call_id: None,
            tool_name: None,
        });

        if tool_calls.is_empty() {
            return Ok((messages, total_in, total_out, context_tokens));
        }

        for tc in tool_calls {
            let result = run_tool_call(
                &remote,
                &cfg.target,
                &cfg.cwd,
                &tc,
                tx,
                &cfg.asks,
                &cfg.permissions,
                Some(&mcp),
            )
            .await;
            messages.push(ChatMessage {
                role: ChatRole::Tool,
                content: result,
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: Some(tc.id),
                tool_name: Some(tc.name),
            });
        }

        // Steering: deliver queued user text verbatim before the next model
        // call. No summarising, no reordering.
        let steered: Vec<String> = {
            let mut pending = cfg.steer.lock().unwrap();
            pending.drain(..).collect()
        };
        for text in steered {
            messages.push(ChatMessage {
                role: ChatRole::User,
                content: text,
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
                tool_name: None,
            });
        }
    }

    bail!("reached maximum tool steps ({MAX_TOOL_STEPS}) without a final response")
}

#[allow(clippy::too_many_arguments)]
async fn call_provider_with_overflow_retry(
    http: &reqwest::Client,
    cfg: &TurnConfig,
    provider: &mut StoredProvider,
    system: &str,
    messages: &mut Vec<ChatMessage>,
    extra_tools: &[serde_json::Value],
    calibration: TokenCalibration,
    plan: &mut super::context::BudgetPlan,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    let first = dispatch_stream(
        http,
        provider,
        &cfg.model.model_id,
        system,
        messages,
        Some(&cfg.permissions),
        extra_tools,
        tx,
    )
    .await;
    let err = match first {
        Ok(reply) => return Ok(reply),
        Err(err) => err,
    };

    let msg = format!("{err:#}");
    if !is_context_overflow_error(&msg) {
        return Err(err);
    }

    if let Some(real_limit) = parse_overflow_limit(&msg) {
        record_model_cap(
            &cfg.data_dir,
            &cfg.provider.id,
            &cfg.model.model_id,
            real_limit,
            "overflow-calibrated",
        );
        *plan = resolve_budget(&cfg.data_dir, &cfg.provider.id, &cfg.model.model_id);
    }

    let compacted = prepare_context(
        http,
        &cfg.data_dir,
        provider,
        &cfg.model.model_id,
        &cfg.target,
        &cfg.cwd,
        system,
        messages,
        calibration,
        plan,
        true,
        None,
        Some(tx),
    )
    .await?;

    let Some(outcome) = compacted else {
        return Err(err);
    };
    let _ = tx.send(AgentEvent::Compacted {
        messages: outcome.messages,
        before_tokens: outcome.before_tokens,
        after_tokens: outcome.after_tokens,
        summary: outcome.summary,
        auto: true,
    });

    dispatch_stream(
        http,
        provider,
        &cfg.model.model_id,
        system,
        messages,
        Some(&cfg.permissions),
        extra_tools,
        tx,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_stream(
    http: &reqwest::Client,
    provider: &StoredProvider,
    model_id: &str,
    system: &str,
    messages: &[ChatMessage],
    permissions: Option<&crate::permissions::PermissionEngine>,
    extra_tools: &[serde_json::Value],
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    match provider.protocol {
        ProviderProtocol::OpenAiChat => {
            stream_openai_chat(
                http,
                provider,
                model_id,
                system,
                messages,
                permissions,
                extra_tools,
                tx,
            )
            .await
        }
        ProviderProtocol::OpenAiCodex => {
            stream_openai_codex(
                http,
                provider,
                model_id,
                system,
                messages,
                permissions,
                extra_tools,
                tx,
            )
            .await
        }
        ProviderProtocol::Anthropic => {
            stream_anthropic(
                http,
                provider,
                model_id,
                system,
                messages,
                permissions,
                extra_tools,
                tx,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Credential, ModelRef};
    use crate::session::ToolCall;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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
                    let body = "{\"error\":{\"message\":\"maximum context length is 65536 tokens; too many\"}}";
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
            credential: Credential::ApiKey { key: "sk-test".into() },
            custom_models: vec![],
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
            target: "local".into(),
            cwd: dir.clone(),
            provider: provider.clone(),
            model: ModelRef {
                provider_id: "test".into(),
                model_id: "test-model".into(),
            },
            history: history.clone(),
            asks: super::super::asks::AskBroker::new(),
            steer: Arc::new(Mutex::new(Vec::new())),
            permissions: crate::permissions::PermissionEngine::new(
                dir.clone(),
                dir.clone(),
                "test-s".into(),
                "local".into(),
                crate::permissions::PermissionMode::Default,
            ),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let system = "sys";
        let mut prov = provider;
        let mut plan = resolve_budget(&dir, "test", "test-model");
        let res = call_provider_with_overflow_retry(
            &reqwest::Client::new(),
            &cfg,
            &mut prov,
            system,
            &mut history,
            &[],
            TokenCalibration::default(),
            &mut plan,
            &tx,
        )
        .await;

        assert!(res.is_err());
        assert_eq!(*hits.lock().unwrap(), 3); // 1 overflow + 1 summary + 1 retry (no infinite loop!)
        let mut saw_compacted = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, AgentEvent::Compacted { .. }) {
                saw_compacted = true;
            }
        }
        assert!(saw_compacted);
        // Verified overflow calibration recorded 65536 into model_caps.json
        assert_eq!(plan.hard_window, 65_536);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
