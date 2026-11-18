//! The single agent loop with pre-flight context budgeting and 1x overflow retry.

use super::config::TurnConfig;
use super::context::{
    estimate_turn_raw_tokens, is_context_overflow_error, parse_overflow_limit, prepare_context,
    record_model_cap, refresh_models_dev_if_stale, resolve_budget, tools_overhead,
    TokenCalibration,
};
use super::events::AgentEvent;
use super::loop_hooks::{hook_context_message, TurnHookSession};
use super::prompt::{build_system_prompt, environment_note_for_depth, sync_environment};
use super::providers::{
    anthropic_max_tokens, stream_anthropic, stream_openai_chat, stream_openai_codex, StreamReply,
};
use super::subagent::SubagentHost;
use super::tools::ToolFilter;
use crate::auth::{ensure_fresh_provider, ProviderProtocol, StoredProvider};
use crate::daemon;
use crate::mcp::client::McpManager;
use crate::session::{ChatMessage, ChatRole};
use anyhow::{bail, Result};
use envoy_service::{dialer_for, AgentDialer, Remote, UnixDialer};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;

const DEFAULT_TOOL_STEPS: usize = 25;
const HARD_MAX_TOOL_STEPS: usize = 250;

pub(crate) struct LoopOptions {
    pub(crate) system: String,
    pub(crate) max_steps: usize,
    pub(crate) depth: u8,
    pub(crate) allowed_tools: Option<Vec<String>>,
    pub(crate) connect_mcp: bool,
    pub(crate) subagent_host: Option<SubagentHost>,
    pub(crate) subagent_name: Option<String>,
}

pub(crate) struct LoopOutcome {
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) context_tokens: u64,
    pub(crate) hook_state: BTreeMap<String, Value>,
}

pub async fn run_agent_turn(cfg: TurnConfig, tx: mpsc::UnboundedSender<AgentEvent>) {
    let model_label = cfg.model.display();
    let _ = tx.send(AgentEvent::AssistantStart {
        model: model_label.clone(),
    });
    let host = SubagentHost {
        data_dir: cfg.data_dir.clone(),
        conduit: cfg.conduit.clone(),
        local_socket: cfg.local_socket.clone(),
        target: cfg.target.clone(),
        cwd: cfg.cwd.clone(),
        provider: cfg.provider.clone(),
        model: cfg.model.clone(),
        asks: cfg.asks.clone(),
        permissions: cfg.permissions.clone(),
        hooks: cfg.hooks.clone(),
        turn_seq: cfg.turn_seq,
    };
    let opts = LoopOptions {
        system: build_system_prompt(&cfg.target, &cfg.cwd),
        max_steps: cfg.max_steps.unwrap_or(DEFAULT_TOOL_STEPS),
        depth: 0,
        allowed_tools: None,
        connect_mcp: true,
        subagent_host: Some(host),
        subagent_name: None,
    };
    let hook_sink = cfg.hook_state_sink.clone();
    match execute_loop(cfg, opts, &tx).await {
        Ok(outcome) => {
            if let Some(sink) = &hook_sink {
                *sink.lock().unwrap() = outcome.hook_state.clone();
            }
            let _ = tx.send(AgentEvent::TurnDone {
                messages: outcome.messages,
                input_tokens: outcome.input_tokens,
                output_tokens: outcome.output_tokens,
                context_tokens: outcome.context_tokens,
            });
        }
        Err(e) => {
            let _ = tx.send(AgentEvent::TurnError(format!("{e:#}")));
        }
    }
}

pub(crate) async fn execute_loop(
    cfg: TurnConfig,
    opts: LoopOptions,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<LoopOutcome> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()?;

    let is_local = cfg.target == "local" || cfg.target.is_empty();
    if opts.depth == 0 {
        let bg_http = http.clone();
        let bg_dir = cfg.data_dir.clone();
        tokio::spawn(async move {
            refresh_models_dev_if_stale(&bg_http, &bg_dir).await;
        });

        if is_local && cfg.local_socket.is_none() {
            daemon::ensure(&cfg.data_dir).await?;
        }
    }

    let mut provider = ensure_fresh_provider(&http, &cfg.data_dir, cfg.provider.clone()).await?;
    let mut messages = cfg.history.clone();
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    #[allow(unused_assignments)]
    let mut context_tokens = 0u64;
    let mut calibration = TokenCalibration::default();

    let dialer: Box<dyn AgentDialer> = match (&cfg.local_socket, is_local) {
        (Some(sock), true) => Box::new(UnixDialer::new(sock.clone())),
        _ => dialer_for(&cfg.data_dir, &cfg.target, cfg.conduit.clone()),
    };
    let remote: Remote<Box<dyn AgentDialer>> = Remote::new(dialer);

    let mcp = if opts.connect_mcp {
        Some(McpManager::connect_configured(&cfg.data_dir, &cfg.cwd).await)
    } else {
        None
    };
    let extra_tools = mcp
        .as_ref()
        .map(McpManager::extra_tool_schemas)
        .unwrap_or_default();

    let tool_filter = ToolFilter {
        depth: opts.depth,
        allowed: opts.allowed_tools,
    };
    let system = opts.system;
    let overhead = tools_overhead(extra_tools.len());
    let mut plan = resolve_budget(&cfg.data_dir, &cfg.provider.id, &cfg.model.model_id);
    let max_steps = opts.max_steps.clamp(1, HARD_MAX_TOOL_STEPS);
    let mut hook_session = TurnHookSession {
        engine: cfg.hooks.clone(),
        session_id: cfg.permissions.session_id().to_string(),
        cwd: cfg.cwd.clone(),
        target: cfg.target.clone(),
        permission_mode: cfg.permissions.mode().label().to_string(),
        turn_seq: cfg.turn_seq,
        depth: opts.depth,
        subagent_name: opts.subagent_name.clone(),
        state: cfg.hook_state.clone(),
        stop_hook_count: 0,
    };

    let res: Result<LoopOutcome> = async {
        for step in 0..max_steps {
            if step > 0 {
                let _ = tx.send(AgentEvent::AssistantStart {
                    model: cfg.model.display(),
                });
            }
            provider = ensure_fresh_provider(&http, &cfg.data_dir, provider).await?;
            let note = environment_note_for_depth(
                &cfg.data_dir,
                &cfg.cwd,
                cfg.permissions.session_id(),
                opts.depth,
            );
            sync_environment(&mut messages, note.as_deref());

            if let Some(outcome) = hook_session
                .prepare_context_step(
                    step,
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
                    overhead,
                    false,
                    tx,
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

            let raw_est = estimate_turn_raw_tokens(&system, &messages, overhead);
            let pre_model_extra = hook_session
                .run_pre_model_call(
                    step,
                    &cfg.model.model_id,
                    raw_est,
                    context_tokens,
                    &cfg.asks,
                    tx,
                )
                .await?;
            let effective_system = match pre_model_extra {
                Some(extra) => format!("{system}\n\n<hook_context>\n{extra}\n</hook_context>"),
                None => system.clone(),
            };

            let reply = call_provider_with_overflow_retry(
                &http,
                &cfg,
                &mut provider,
                &effective_system,
                &mut messages,
                &tool_filter,
                &extra_tools,
                calibration,
                &mut plan,
                Some(&mut hook_session),
                step,
                tx,
            )
            .await?;

            calibration.observe(raw_est, reply.input_tokens);
            total_in += reply.input_tokens;
            total_out += reply.output_tokens;
            context_tokens = reply.input_tokens;

            let planned_tools: Vec<String> =
                reply.tool_calls.iter().map(|t| t.name.clone()).collect();
            let post_model_extra = hook_session
                .run_post_model_call(
                    step,
                    &cfg.model.model_id,
                    reply.input_tokens,
                    reply.output_tokens,
                    &reply.text,
                    planned_tools,
                    &cfg.asks,
                    tx,
                )
                .await?;

            let tool_calls = reply.tool_calls.clone();
            let reply_text = reply.text.clone();
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
                if let Some(reenter) = hook_session
                    .check_stop_hook(step, &reply_text, step + 1 < max_steps, tx)
                    .await
                {
                    if !post_model_extra.is_empty() {
                        messages.push(hook_context_message(&post_model_extra));
                    }
                    messages.push(ChatMessage {
                        role: ChatRole::User,
                        content: reenter,
                        reasoning: None,
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                        tool_name: None,
                    });
                    continue;
                }
                return Ok(LoopOutcome {
                    messages,
                    input_tokens: total_in,
                    output_tokens: total_out,
                    context_tokens,
                    hook_state: hook_session.state.clone(),
                });
            }

            hook_session
                .execute_tool_batch(
                    step,
                    &remote,
                    &cfg.target,
                    &cfg.cwd,
                    &tool_calls,
                    &mut messages,
                    tx,
                    &cfg.asks,
                    &cfg.permissions,
                    mcp.as_ref(),
                    &tool_filter,
                    opts.subagent_host.as_ref(),
                    cfg.monitors.as_ref(),
                )
                .await;

            if !post_model_extra.is_empty() {
                messages.push(hook_context_message(&post_model_extra));
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

        bail!("reached maximum tool steps ({max_steps}) without a final response")
    }
    .await;

    if let Some(sink) = &cfg.hook_state_sink {
        *sink.lock().unwrap() = hook_session.state;
    }
    res
}

#[allow(clippy::too_many_arguments)]
async fn call_provider_with_overflow_retry(
    http: &reqwest::Client,
    cfg: &TurnConfig,
    provider: &mut StoredProvider,
    system: &str,
    messages: &mut Vec<ChatMessage>,
    tool_filter: &ToolFilter,
    extra_tools: &[serde_json::Value],
    calibration: TokenCalibration,
    plan: &mut super::context::BudgetPlan,
    hook_session: Option<&mut TurnHookSession>,
    step: usize,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    let max_tokens = turn_max_tokens(cfg);
    let first = dispatch_stream(
        http,
        provider,
        &cfg.model.model_id,
        system,
        messages,
        Some(&cfg.permissions),
        tool_filter,
        extra_tools,
        max_tokens,
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

    let compacted = match hook_session {
        Some(hs) => {
            hs.prepare_context_step(
                step,
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
                tools_overhead(extra_tools.len()),
                true,
                tx,
            )
            .await?
        }
        None => {
            prepare_context(
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
                tools_overhead(extra_tools.len()),
                true,
                None,
                Some(tx),
                None,
            )
            .await?
        }
    };

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
        tool_filter,
        extra_tools,
        max_tokens,
        tx,
    )
    .await
}

fn turn_max_tokens(cfg: &TurnConfig) -> u32 {
    let cap = super::context::catalog::resolve_model_cap(
        &cfg.data_dir,
        &cfg.provider.id,
        &cfg.model.model_id,
    );
    anthropic_max_tokens(cap.max_output)
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_stream(
    http: &reqwest::Client,
    provider: &StoredProvider,
    model_id: &str,
    system: &str,
    messages: &[ChatMessage],
    permissions: Option<&crate::permissions::PermissionEngine>,
    tool_filter: &ToolFilter,
    extra_tools: &[serde_json::Value],
    max_tokens: u32,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    let provider = provider.routed_for(model_id);
    match provider.protocol {
        ProviderProtocol::OpenAiChat => {
            stream_openai_chat(
                http,
                &provider,
                model_id,
                system,
                messages,
                permissions,
                tool_filter,
                extra_tools,
                tx,
            )
            .await
        }
        ProviderProtocol::OpenAiCodex => {
            stream_openai_codex(
                http,
                &provider,
                model_id,
                system,
                messages,
                permissions,
                tool_filter,
                extra_tools,
                tx,
            )
            .await
        }
        ProviderProtocol::Anthropic => {
            stream_anthropic(
                http,
                &provider,
                model_id,
                system,
                messages,
                permissions,
                tool_filter,
                extra_tools,
                max_tokens,
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
            credential: Credential::ApiKey {
                key: "sk-test".into(),
            },
            custom_models: vec![],
            model_routes: std::collections::BTreeMap::new(),
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
            asks: super::super::asks::AskBroker::new(),
            steer: Arc::new(Mutex::new(Vec::new())),
            permissions: crate::permissions::PermissionEngine::new(
                dir.clone(),
                dir.clone(),
                "test-s".into(),
                "local".into(),
                crate::permissions::PermissionMode::Default,
            ),
            monitors: None,
            hooks: None,
            hook_state: BTreeMap::new(),
            hook_state_sink: None,
            turn_seq: 0,
            max_steps: None,
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
