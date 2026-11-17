//! Context budget management, two-stage compaction, overflow detection, and `/handoff`.

pub mod budget;
pub mod catalog;
pub mod prune;
pub mod summary;
pub mod tokens;

pub use budget::{resolve_budget, set_budget_tier, BudgetPlan, CompactionTier};
pub use catalog::{
    fetch_openai_models_with_caps, record_model_cap, refresh_models_dev_if_stale,
};
pub use prune::{bound_tool_output, prune_old_tool_outputs};
pub use summary::{assemble_compacted_messages, split_head_tail, summarize_messages};
pub use tokens::{
    context_used, estimate_turn_raw_tokens, estimate_turn_tokens, TokenCalibration,
    BUILTIN_TOOLS_TOKENS_ESTIMATE,
};

use crate::agent::events::AgentEvent;
use crate::auth::StoredProvider;
use crate::session::ChatMessage;
use anyhow::Result;
use std::path::Path;
use tokio::sync::mpsc;

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
/// - Above `prune_at`: runs Stage 1 cheap tool-output pruning first.
/// - Still above `compact_at` (or `force = true`): runs Stage 2 summary compaction.
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
    force: bool,
    focus: Option<&str>,
    tx: Option<&mpsc::UnboundedSender<AgentEvent>>,
) -> Result<Option<CompactionOutcome>> {
    let before_tokens =
        estimate_turn_tokens(system, messages, BUILTIN_TOOLS_TOKENS_ESTIMATE, calibration);

    if !force && before_tokens <= plan.prune_at {
        return Ok(None);
    }

    // Stage 1: cheap, local tool-output pruning.
    let prune_report = prune_old_tool_outputs(messages, plan.tail_keep_tokens);
    let after_prune =
        estimate_turn_tokens(system, messages, BUILTIN_TOOLS_TOKENS_ESTIMATE, calibration);

    if !force && after_prune <= plan.compact_at {
        if prune_report.messages_pruned > 0 {
            return Ok(Some(CompactionOutcome {
                messages: messages.clone(),
                before_tokens,
                after_tokens: after_prune,
                summary: format!(
                    "Pruned {} older tool outputs (~{} -> ~{} tokens).",
                    prune_report.messages_pruned, before_tokens, after_prune
                ),
                pruned_only: true,
            }));
        }
        return Ok(None);
    }

    // Stage 2: split (head, tail) safely and generate structured handoff summary.
    let (head, tail) = split_head_tail(messages, plan.tail_keep_tokens, force);
    if head.is_empty() {
        return Ok(None);
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
        focus,
        false,
    )
    .await?;

    let compacted = assemble_compacted_messages(&summary_text, tail, target, cwd);
    let after_tokens =
        estimate_turn_tokens(system, &compacted, BUILTIN_TOOLS_TOKENS_ESTIMATE, calibration);
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
pub async fn generate_handoff(
    http: &reqwest::Client,
    data_dir: &Path,
    provider: &StoredProvider,
    model_id: &str,
    system: &str,
    messages: &[ChatMessage],
    focus: Option<&str>,
) -> Result<HandoffOutcome> {
    let before_tokens = estimate_turn_tokens(
        system,
        messages,
        BUILTIN_TOOLS_TOKENS_ESTIMATE,
        TokenCalibration::default(),
    );
    let summary = summarize_messages(
        http,
        data_dir,
        provider,
        model_id,
        system,
        messages,
        focus,
        true,
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
        assert!(is_context_overflow_error("Codex API error (400): context_length_exceeded"));
        assert!(!is_context_overflow_error("LLM API error (429): rate limit reached"));

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
