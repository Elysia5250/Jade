//! Context budget management, two-stage compaction, overflow detection, and `/handoff`.

pub mod budget;
pub mod catalog;
mod extract;
mod families;
pub mod fetch;
mod mask;
pub mod prune;
pub(crate) mod reasoning;
pub mod summary;
pub mod tokens;

pub use budget::{resolve_budget, set_budget_tier, BudgetPlan, CompactionTier};
pub use catalog::{
    record_model_cap, refresh_models_dev_if_stale, store_listed_models, ListedModel,
};
pub use fetch::fetch_provider_models;
pub use prune::bound_tool_output;
pub use summary::{assemble_compacted_messages, split_head_tail, summarize_messages};
pub use tokens::{
    context_used, estimate_turn_raw_tokens, estimate_turn_tokens, tools_overhead, TokenCalibration,
    BUILTIN_TOOLS_TOKENS_ESTIMATE,
};

use crate::agent::events::AgentEvent;
use crate::auth::StoredProvider;
use crate::hooks::{emit_hook_event, HookContext, HookEngine, HookEvent};
use crate::session::ChatMessage;
use anyhow::Result;
use std::path::Path;
use tokio::sync::mpsc;

pub struct PreCompactHook<'a> {
    pub engine: &'a HookEngine,
    pub ctx: &'a mut HookContext,
}

#[derive(Debug, Clone)]
pub struct CompactionOutcome {
    pub messages: Vec<ChatMessage>,
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub summary: String,
    pub pruned_only: bool,
}

#[derive(Debug, Clone)]
pub struct HandoffOutcome {
    pub summary: String,
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub title: String,
}

/// Detect context-length / prompt-too-long provider errors across OpenAI,
/// Anthropic, Gemini, and OpenAI-compatible relays.
pub fn is_context_overflow_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    let has_status = lower.contains("400") || lower.contains("413");
    let markers = [
        "context_length_exceeded",
        "maximum context length",
        "prompt is too long",
        "exceeds the context window",
        "exceeds maximum context",
        "too many tokens",
        "input length and max_tokens exceed",
        "reduce the length of the messages",
        "request entity too large",
        "token limit exceeded",
        "max_tokens limit",
        "exceeds the maximum number of tokens",
    ];
    if markers.iter().any(|m| lower.contains(m)) {
        return true;
    }
    has_status && (lower.contains("context window") || lower.contains("context length"))
}

/// Extract an advertised numeric token cap from an overflow error string (e.g.
/// `"maximum context length is 65536 tokens"`), returning `None` if ambiguous.
pub fn parse_overflow_limit(msg: &str) -> Option<u64> {
    let lower = msg.to_ascii_lowercase();
    for anchor in [
        "maximum context length is ",
        "maximum context length of ",
        "context length is ",
        "limit of ",
        "max_input_tokens: ",
        "maximum is ",
    ] {
        if let Some(pos) = lower.find(anchor) {
            let rest = &lower[pos + anchor.len()..];
            let digits: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .filter(|c| c.is_ascii_digit())
                .collect();
            if let Ok(val) = digits.parse::<u64>() {
                if (4_096..=4_000_000).contains(&val) {
                    return Some(val);
                }
            }
        }
    }
    None
}

/// Prepare `messages` before an LLM step:
/// - Below `prune_at`: returns `Ok(None)`.
/// - Above `prune_at`: masks old tool observations outside the protected tail.
/// - Still above `compact_at` (or `force = true`): summarizes the head from the
///   previous summary plus newly evicted notes.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_context(
    http: &reqwest::Client,
    data_dir: &Path,
    provider: &StoredProvider,
    model_id: &str,
    target: &str,
    cwd: &Path,
    system: &str,
    messages: &mut Vec<ChatMessage>,
    calibration: TokenCalibration,
    plan: &BudgetPlan,
    tools_overhead: u64,
    force: bool,
    focus: Option<&str>,
    tx: Option<&mpsc::UnboundedSender<AgentEvent>>,
    pre_compact_hook: Option<PreCompactHook<'_>>,
    session_id: &str,
) -> Result<Option<CompactionOutcome>> {
    let pinned = crate::agent::prompt::take_environment(messages);
    let outcome = prepare_context_body(
        http,
        data_dir,
        provider,
        model_id,
        target,
        cwd,
        system,
        messages,
        calibration,
        plan,
        tools_overhead,
        force,
        focus,
        tx,
        pre_compact_hook,
        session_id,
    )
    .await;
    crate::agent::prompt::restore_environment(messages, pinned);
    match outcome {
        Ok(Some(mut out)) => {
            out.messages = messages.clone();
            Ok(Some(out))
        }
        other => other,
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_context_body(
    http: &reqwest::Client,
    data_dir: &Path,
    provider: &StoredProvider,
    model_id: &str,
    target: &str,
    cwd: &Path,
    system: &str,
    messages: &mut Vec<ChatMessage>,
    calibration: TokenCalibration,
    plan: &BudgetPlan,
    tools_overhead: u64,
    force: bool,
    focus: Option<&str>,
    tx: Option<&mpsc::UnboundedSender<AgentEvent>>,
    pre_compact_hook: Option<PreCompactHook<'_>>,
    session_id: &str,
) -> Result<Option<CompactionOutcome>> {
    let before_tokens = estimate_turn_tokens(system, messages, tools_overhead, calibration);

    if !force && before_tokens <= plan.prune_at {
        return Ok(None);
    }

    let mask_report = mask::mask_old_observations(messages, plan.tail_keep_tokens, calibration);
    let after_mask = estimate_turn_tokens(system, messages, tools_overhead, calibration);

    if !force && after_mask <= plan.compact_at {
        if mask_report.messages_pruned > 0 {
            return Ok(Some(CompactionOutcome {
                messages: messages.clone(),
                before_tokens,
                after_tokens: after_mask,
                summary: format!(
                    "Masked {} older tool outputs (~{} -> ~{} tokens).",
                    mask_report.messages_pruned, before_tokens, after_mask
                ),
                pruned_only: true,
            }));
        }
        return Ok(None);
    }

    let (head, tail) = split_head_tail(messages, plan.tail_keep_tokens, force, calibration);
    if head.is_empty() {
        return Ok(None);
    }

    let mut effective_focus = focus.map(|s| s.to_string());
    if let Some(hc) = pre_compact_hook {
        let mut hctx = hc.ctx.clone();
        hctx.event = HookEvent::PreCompact;
        hctx.can_block = !force;
        if hctx.compact_auto.is_none() {
            hctx.compact_auto = Some(!force);
        }
        hctx.model_id = Some(model_id.to_string());
        hctx.estimated_tokens = Some(after_mask);
        hctx.decision = Default::default();
        hctx.additional_context.clear();
        hctx.trace.clear();

        let next = hc.engine.dispatch(hctx).await;
        if let Some(tx) = tx {
            emit_hook_event(tx, &next);
        }
        let blocked = next.decision.is_blocking() && !force;
        let injected = next.additional_context.clone();
        *hc.ctx = next;
        if blocked {
            return Ok(None);
        }
        if !injected.is_empty() {
            let extra = injected.join("\n");
            effective_focus = Some(match effective_focus {
                Some(existing) if !existing.trim().is_empty() => format!("{existing}\n{extra}"),
                _ => extra,
            });
        }
    }

    if let Some(tx) = tx {
        let _ = tx.send(AgentEvent::Compacting {
            auto: !force || focus.is_none(),
            handoff: false,
        });
    }

    let summary_text = summarize_messages(
        http,
        data_dir,
        provider,
        model_id,
        system,
        &head,
        effective_focus.as_deref(),
        false,
        session_id,
    )
    .await?;

    let compacted = assemble_compacted_messages(&summary_text, tail, target, cwd);
    let after_tokens = estimate_turn_tokens(system, &compacted, tools_overhead, calibration);
    *messages = compacted.clone();

    Ok(Some(CompactionOutcome {
        messages: compacted,
        before_tokens,
        after_tokens,
        summary: summary_text,
        pruned_only: false,
    }))
}

/// Generate a full-session handoff summary for `/handoff` (Context Reset).
#[allow(clippy::too_many_arguments)]
pub async fn generate_handoff(
    http: &reqwest::Client,
    data_dir: &Path,
    provider: &StoredProvider,
    model_id: &str,
    system: &str,
    messages: &[ChatMessage],
    focus: Option<&str>,
    session_id: &str,
) -> Result<HandoffOutcome> {
    let before_tokens = estimate_turn_tokens(
        system,
        messages,
        BUILTIN_TOOLS_TOKENS_ESTIMATE,
        TokenCalibration::default(),
    );
    let summary = summarize_messages(
        http, data_dir, provider, model_id, system, messages, focus, true, session_id,
    )
    .await?;
    let after_tokens = estimate_turn_tokens(
        system,
        &[ChatMessage {
            role: crate::session::ChatRole::User,
            content: summary.clone(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        }],
        BUILTIN_TOOLS_TOKENS_ESTIMATE,
        TokenCalibration::default(),
    );
    let title = derive_handoff_title(messages, focus);
    Ok(HandoffOutcome {
        summary,
        before_tokens,
        after_tokens,
        title,
    })
}

fn derive_handoff_title(messages: &[ChatMessage], focus: Option<&str>) -> String {
    let raw = focus
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            messages
                .iter()
                .find(|m| m.role == crate::session::ChatRole::User)
                .map(|m| m.content.trim())
        })
        .unwrap_or("handoff");
    let line = raw.lines().next().unwrap_or("handoff").trim();
    let short: String = line.chars().take(40).collect();
    format!("handoff: {short}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_provider_overflow_errors_and_extracts_limits() {
        assert!(is_context_overflow_error(
            "Anthropic API error (400 Bad Request): prompt is too long: 215400 tokens > 200000 maximum"
        ));
        assert!(is_context_overflow_error(
            "LLM API error (400): This model's maximum context length is 65536 tokens. However, your messages resulted in 70100 tokens."
        ));
        assert!(is_context_overflow_error(
            "Codex API error (400): context_length_exceeded"
        ));
        assert!(!is_context_overflow_error(
            "LLM API error (429): rate limit reached"
        ));

        assert_eq!(
            parse_overflow_limit("This model's maximum context length is 65536 tokens."),
            Some(65_536)
        );
        assert_eq!(
            parse_overflow_limit("exceeds the limit of 131,072 tokens"),
            Some(131_072)
        );
        assert_eq!(parse_overflow_limit("invalid tool call"), None);
    }
}
