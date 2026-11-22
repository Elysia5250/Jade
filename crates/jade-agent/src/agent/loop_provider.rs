//! Provider streaming dispatch, single overflow-compaction retry, and
//! step-limit finalization for the agent loop.

use super::config::TurnConfig;
use super::context::{
    is_context_overflow_error, parse_overflow_limit, prepare_context, record_model_cap,
    resolve_budget, tools_overhead_for, BudgetPlan, TokenCalibration,
};
use super::events::AgentEvent;
use super::loop_hooks::TurnHookSession;
use super::providers::{anthropic_max_tokens, ProviderRegistry, StreamReply, StreamRequest};
use super::r#loop::LoopOutcome;
use super::tools::ToolFilter;
use crate::auth::StoredProvider;
use crate::session::{ChatMessage, ChatRole};
use anyhow::Result;
use tokio::sync::mpsc;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_provider_with_overflow_retry(
    http: &reqwest::Client,
    cfg: &TurnConfig,
    provider: &mut StoredProvider,
    system: &str,
    messages: &mut Vec<ChatMessage>,
    tool_filter: &ToolFilter,
    extra_tools: &[serde_json::Value],
    calibration: TokenCalibration,
    plan: &mut BudgetPlan,
    hook_session: Option<&mut TurnHookSession>,
    step: usize,
    total_in: &mut u64,
    total_out: &mut u64,
    cache_read_tokens: &mut u64,
    cache_create_tokens: &mut u64,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    let mut max_tokens = turn_max_tokens(cfg);
    let first = dispatch_stream(
        cfg.providers.as_deref(),
        http,
        provider,
        &cfg.model.model_id,
        system,
        messages,
        Some(&cfg.permissions),
        tool_filter,
        extra_tools,
        max_tokens,
        &cfg.data_dir,
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

    let overhead = tools_overhead_for(Some(&cfg.permissions), tool_filter, extra_tools);
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
                overhead,
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
                overhead,
                true,
                None,
                Some(tx),
                None,
                cfg.permissions.session_id(),
            )
            .await?
        }
    };

    let Some(outcome) = compacted else {
        return Err(err);
    };
    *total_in += outcome.input_tokens;
    *total_out += outcome.output_tokens;
    *cache_read_tokens += outcome.cache_read_tokens;
    *cache_create_tokens += outcome.cache_create_tokens;
    let _ = tx.send(AgentEvent::Compacted {
        messages: outcome.messages,
        before_tokens: outcome.before_tokens,
        after_tokens: outcome.after_tokens,
        summary: outcome.summary,
        auto: true,
    });

    max_tokens = turn_max_tokens(cfg);
    let mut reply = dispatch_stream(
        cfg.providers.as_deref(),
        http,
        provider,
        &cfg.model.model_id,
        system,
        messages,
        Some(&cfg.permissions),
        tool_filter,
        extra_tools,
        max_tokens,
        &cfg.data_dir,
        tx,
    )
    .await?;
    reply.calibrate = false;
    Ok(reply)
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
    providers: Option<&ProviderRegistry>,
    http: &reqwest::Client,
    provider: &StoredProvider,
    model_id: &str,
    system: &str,
    messages: &[ChatMessage],
    permissions: Option<&crate::permissions::PermissionEngine>,
    tool_filter: &ToolFilter,
    extra_tools: &[serde_json::Value],
    max_tokens: u32,
    data_dir: &std::path::Path,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    let provider = provider.routed_for(model_id);
    let session_id = permissions.map(|p| p.session_id()).unwrap_or("");
    let default_reg;
    let reg = match providers {
        Some(r) => r,
        None => {
            default_reg = ProviderRegistry::with_builtins();
            &default_reg
        }
    };
    reg.stream(StreamRequest {
        http,
        provider: &provider,
        model_id,
        system,
        messages,
        permissions,
        tool_filter,
        extra_tools,
        max_tokens,
        session_id,
        data_dir,
        tx,
    })
    .await
}

/// The step cap ended on a tool call. Ask once more with every tool removed so
/// the turn still ends on a reply. Tool calls in that reply are not run.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_step_limit(
    http: &reqwest::Client,
    provider: &mut StoredProvider,
    cfg: &TurnConfig,
    system: String,
    mut messages: Vec<ChatMessage>,
    mut total_in: u64,
    mut total_out: u64,
    mut context_tokens: u64,
    mut cache_read_tokens: u64,
    mut cache_create_tokens: u64,
    calibration_factor: f64,
    max_steps: usize,
    hook_session: &TurnHookSession,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<LoopOutcome> {
    let _ = tx.send(AgentEvent::AssistantStart {
        model: cfg.model.display(),
    });
    messages.push(ChatMessage {
        role: ChatRole::User,
        content: format!(
            "Tool step limit ({max_steps}) reached. Do not call tools. Answer the user now with the results you already have."
        ),
        reasoning: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        reasoning_blocks: Vec::new(),
    images: Vec::new(),
    });
    let no_tools = ToolFilter::none();
    let reply = dispatch_stream(
        cfg.providers.as_deref(),
        http,
        provider,
        &cfg.model.model_id,
        &system,
        &messages,
        Some(&cfg.permissions),
        &no_tools,
        &[],
        turn_max_tokens(cfg),
        &cfg.data_dir,
        tx,
    )
    .await;
    let fallback = format!(
        "Stopped after {max_steps} tool steps. This is the final reply; the commands above are what ran."
    );
    let text = match reply {
        Ok(reply) => {
            total_in += reply.input_tokens;
            total_out += reply.output_tokens;
            cache_read_tokens += reply.cache_read_tokens;
            cache_create_tokens += reply.cache_create_tokens;
            if reply.input_tokens > 0 {
                context_tokens = reply.input_tokens;
            }
            if reply.text.trim().is_empty() {
                let _ = tx.send(AgentEvent::TextDelta(fallback.clone()));
                fallback
            } else {
                reply.text
            }
        }
        Err(_) => {
            let _ = tx.send(AgentEvent::TextDelta(fallback.clone()));
            fallback
        }
    };
    messages.push(ChatMessage {
        role: ChatRole::Assistant,
        content: text,
        reasoning: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        reasoning_blocks: Vec::new(),
        images: Vec::new(),
    });
    Ok(LoopOutcome {
        messages,
        input_tokens: total_in,
        output_tokens: total_out,
        context_tokens,
        cache_read_tokens,
        cache_create_tokens,
        calibration_factor,
        hook_state: hook_session.state.clone(),
    })
}
