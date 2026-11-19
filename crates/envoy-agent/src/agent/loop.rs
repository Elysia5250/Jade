//! The single agent loop with pre-flight context budgeting and 1x overflow retry.

use super::config::{TurnConfig, TurnUsage};
use super::context::{
    estimate_turn_raw_tokens, estimate_turn_tokens, refresh_models_dev_if_stale, resolve_budget,
    skip_inplace_mask, tools_overhead, TokenCalibration,
};
use super::events::AgentEvent;
use super::loop_hooks::{hook_context_message, TurnHookSession};
pub(crate) use super::loop_provider::call_provider_with_overflow_retry;
use super::loop_provider::finalize_step_limit;
use super::prompt::{
    build_system_prompt, sync_environment, sync_pre_model_tail, EnvDiskSnapshot,
};
use super::subagent::SubagentHost;
use super::tools::ToolFilter;
use crate::auth::ensure_fresh_provider;
use crate::daemon;
use crate::mcp::client::McpManager;
use crate::session::{ChatMessage, ChatRole};
use anyhow::Result;
use envoy_service::{dialer_for, AgentDialer, Remote, UnixDialer};
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::sync::mpsc;

pub(super) const DEFAULT_TOOL_STEPS: usize = 100;
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
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_create_tokens: u64,
    pub(crate) calibration_factor: f64,
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
                cache_read_tokens: outcome.cache_read_tokens,
                cache_create_tokens: outcome.cache_create_tokens,
                calibration_factor: outcome.calibration_factor,
            });
        }
        Err(e) => {
            let _ = tx.send(AgentEvent::TurnError(format!("{e:#}")));
        }
    }
}

struct UsageNote {
    input_tokens: u64,
    output_tokens: u64,
    context_tokens: u64,
    cache_read_tokens: u64,
    cache_create_tokens: u64,
    calibration_factor: f64,
}

fn note_usage(cfg: &TurnConfig, note: UsageNote) {
    let Some(slot) = &cfg.usage else {
        return;
    };
    *slot.lock().unwrap() = TurnUsage {
        input_tokens: note.input_tokens,
        output_tokens: note.output_tokens,
        context_tokens: note.context_tokens,
        cache_read_tokens: note.cache_read_tokens,
        cache_create_tokens: note.cache_create_tokens,
        calibration_factor: note.calibration_factor,
    };
}

pub(crate) async fn execute_loop(
    cfg: TurnConfig,
    opts: LoopOptions,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<LoopOutcome> {
    let http = super::providers::shared_http_client()?;

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
    let mut calibration = match &cfg.usage {
        Some(slot) => TokenCalibration::from_stored(slot.lock().unwrap().calibration_factor),
        None => TokenCalibration::default(),
    };
    let mut cache_read_tokens = 0u64;
    let mut cache_create_tokens = 0u64;
    let mut mask_latched = false;
    let env_disk = EnvDiskSnapshot::capture(&cfg.data_dir, &cfg.cwd, opts.depth);

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
    let overhead = tools_overhead(&extra_tools);
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
            let note = env_disk.note(&cfg.data_dir, cfg.permissions.session_id(), opts.depth);
            sync_environment(&mut messages, note.as_deref());

            let before_tokens = estimate_turn_tokens(&system, &messages, overhead, calibration);
            let mut compressed = false;
            if !skip_inplace_mask(mask_latched, before_tokens, &plan) {
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
                    compressed = true;
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
                if before_tokens > plan.prune_at {
                    mask_latched = true;
                }
            }

            let pre_model_extra = hook_session
                .run_pre_model_call(
                    step,
                    &cfg.model.model_id,
                    before_tokens,
                    context_tokens,
                    &cfg.asks,
                    tx,
                )
                .await?;
            sync_pre_model_tail(&mut messages, pre_model_extra.as_deref());
            let raw_est = estimate_turn_raw_tokens(&system, &messages, overhead);

            let reply = call_provider_with_overflow_retry(
                &http,
                &cfg,
                &mut provider,
                &system,
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

            calibration.observe_uncompressed(raw_est, reply.input_tokens, reply.calibrate && !compressed);
            total_in += reply.input_tokens;
            total_out += reply.output_tokens;
            cache_read_tokens += reply.cache_read_tokens;
            cache_create_tokens += reply.cache_create_tokens;
            context_tokens = reply.input_tokens;
            note_usage(
                &cfg,
                UsageNote {
                    input_tokens: total_in,
                    output_tokens: total_out,
                    context_tokens,
                    cache_read_tokens,
                    cache_create_tokens,
                    calibration_factor: calibration.factor,
                },
            );

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
                reasoning_blocks: reply.reasoning_blocks.clone(),
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
                    
                        reasoning_blocks: Vec::new(),});
                    continue;
                }
                return Ok(LoopOutcome {
                    messages,
                    input_tokens: total_in,
                    output_tokens: total_out,
                    context_tokens,
                    cache_read_tokens,
                    cache_create_tokens,
                    calibration_factor: calibration.factor,
                    hook_state: hook_session.state.clone(),
                });
            }

            let (child_in, child_out) = hook_session
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
            total_in += child_in;
            total_out += child_out;
            note_usage(
                &cfg,
                UsageNote {
                    input_tokens: total_in,
                    output_tokens: total_out,
                    context_tokens,
                    cache_read_tokens,
                    cache_create_tokens,
                    calibration_factor: calibration.factor,
                },
            );

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
                
                    reasoning_blocks: Vec::new(),});
            }
        }

        return finalize_step_limit(
            &http,
            &mut provider,
            &cfg,
            system,
            messages,
            total_in,
            total_out,
            context_tokens,
            max_steps,
            &hook_session,
            tx,
        )
        .await;
    }
    .await;

    if let Some(sink) = &cfg.hook_state_sink {
        *sink.lock().unwrap() = hook_session.state;
    }
    res
}
