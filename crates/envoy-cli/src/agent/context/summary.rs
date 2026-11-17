//! Stage 2 structured compaction summary, safe boundary splitting, and `/handoff`.

use super::is_context_overflow_error;
use super::tokens::estimate_message_tokens;
use crate::agent::events::AgentEvent;
use crate::agent::providers::{stream_anthropic, stream_openai_chat, stream_openai_codex};
use crate::auth::{ensure_fresh_provider, ProviderProtocol, StoredProvider};
use crate::session::{ChatMessage, ChatRole};
use anyhow::Result;
use std::ops::Range;
use std::path::Path;
use tokio::sync::mpsc;

const MAX_FALLBACK_LINES: usize = 32;

/// Partition `messages` into atomic ranges so an `Assistant` with `tool_calls`
/// is never separated from its following `Tool` result messages.
pub fn atomic_group_ranges(messages: &[ChatMessage]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let start = i;
        let is_tool_caller =
            messages[i].role == ChatRole::Assistant && !messages[i].tool_calls.is_empty();
        i += 1;
        if is_tool_caller {
            while i < messages.len() && messages[i].role == ChatRole::Tool {
                i += 1;
            }
        }
        ranges.push(start..i);
    }
    ranges
}

/// Split `messages` into `(head, tail)` along atomic group boundaries, keeping
/// up to `tail_keep_tokens` at the end. When `prefer_nonempty_head` is true
/// (for manual `/compact` or overflow recovery), only the final group stays in
/// `tail` if the normal budget would swallow the whole conversation.
pub fn split_head_tail(
    messages: &[ChatMessage],
    tail_keep_tokens: u64,
    prefer_nonempty_head: bool,
) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
    let groups = atomic_group_ranges(messages);
    if groups.len() <= 1 {
        return (Vec::new(), messages.to_vec());
    }

    let mut acc = 0u64;
    let mut split_group_idx = groups.len();
    for (idx, range) in groups.iter().enumerate().rev() {
        let cost: u64 = messages[range.clone()]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        if idx < groups.len() - 1 && acc + cost > tail_keep_tokens {
            break;
        }
        acc += cost;
        split_group_idx = idx;
    }

    if split_group_idx == 0 && prefer_nonempty_head && groups.len() >= 2 {
        // Keep the latest user-initiated group(s) so at least the older prefix compacts.
        split_group_idx = groups.len() - 1;
        if groups.len() >= 3 && messages[groups[split_group_idx].start].role != ChatRole::User {
            for idx in (1..groups.len()).rev() {
                if messages[groups[idx].start].role == ChatRole::User {
                    split_group_idx = idx;
                    break;
                }
            }
        }
    }

    let split_msg_idx = groups[split_group_idx].start;
    (
        messages[..split_msg_idx].to_vec(),
        messages[split_msg_idx..].to_vec(),
    )
}

/// Build the handoff/compaction instruction appended after `head` so the
/// provider call shares the stable `system` + `head` prefix for prompt caching.
pub fn build_summary_prompt(focus: Option<&str>, for_handoff: bool) -> String {
    let kind = if for_handoff {
        "a clean-slate session handoff document so a fresh agent can immediately continue without reading old logs"
    } else {
        "a structured continuation summary to replace older history while preserving exact engineering facts"
    };
    let mut prompt = format!(
        "Create {kind}. Do NOT call any tools; reply with Markdown only.\n\n\
         Include these sections:\n\
         ## Goal & Constraints\n\
         - User's core objective, acceptance criteria, and explicit constraints or things NOT to do.\n\n\
         ## Completed Work & Verified Facts\n\
         - What was implemented, tested, or verified (cite exact results, not guesses).\n\n\
         ## Decisions & Rejected Alternatives\n\
         - Key architectural/implementation choices and WHY any rejected approaches were abandoned.\n\n\
         ## Files, Commands & Exit Codes\n\
         - Exact file paths read/modified, important commands run, and notable exit codes or errors.\n\n\
         ## Open Issues & Blockers\n\
         - Unresolved bugs, failing checks, or pending questions.\n\n\
         ## Active State & Next Steps\n\
         - Active detached job IDs, current working state, and exact immediate next steps.\n\n\
         Rules: Be factual, concise, and dense. Never invent file paths, symbols, or test outcomes."
    );
    if let Some(f) = focus.map(str::trim).filter(|s| !s.is_empty()) {
        prompt.push_str("\n\nPrioritize preserving details relevant to this focus:\n");
        prompt.push_str(f);
    }
    prompt
}

/// Assemble compacted `messages` by placing the structured handoff + active
/// environment metadata right before `tail`, keeping role alternation valid.
pub fn assemble_compacted_messages(
    summary: &str,
    tail: Vec<ChatMessage>,
    target: &str,
    cwd: &Path,
) -> Vec<ChatMessage> {
    let handoff = format!(
        "<compacted_context>\n{summary}\n</compacted_context>\n\n\
         [Resumed environment: target={target}, cwd={}]",
        cwd.display()
    );
    let mut out = vec![ChatMessage {
        role: ChatRole::User,
        content: handoff,
        reasoning: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_name: None,
    }];
    if tail
        .first()
        .is_some_and(|m| m.role == ChatRole::User || m.role == ChatRole::Tool)
    {
        out.push(ChatMessage {
            role: ChatRole::Assistant,
            content: "Understood. Continuing from the compacted context summary and recent messages.".into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        });
    }
    out.extend(tail);
    out
}

/// Summarize `slice` via the active provider (reusing `system` prefix for cache
/// hits), backing off by dropping oldest atomic groups on overflow and falling
/// back to deterministic extraction if the provider call fails.
pub async fn summarize_messages(
    http: &reqwest::Client,
    data_dir: &Path,
    provider: &StoredProvider,
    model_id: &str,
    system: &str,
    slice: &[ChatMessage],
    focus: Option<&str>,
    for_handoff: bool,
) -> Result<String> {
    let mut working = slice.to_vec();
    let instruction = build_summary_prompt(focus, for_handoff);

    for _attempt in 0..4 {
        if working.is_empty() {
            break;
        }
        let mut req_msgs = working.clone();
        req_msgs.push(ChatMessage {
            role: ChatRole::User,
            content: instruction.clone(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        });

        let fresh = ensure_fresh_provider(http, data_dir, provider.clone())
            .await
            .unwrap_or_else(|_| provider.clone());
        let (sink_tx, _sink_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let res = match fresh.protocol {
            ProviderProtocol::OpenAiChat => {
                stream_openai_chat(
                    http, &fresh, model_id, system, &req_msgs, None, &[], &sink_tx,
                )
                .await
            }
            ProviderProtocol::OpenAiCodex => {
                stream_openai_codex(
                    http, &fresh, model_id, system, &req_msgs, None, &[], &sink_tx,
                )
                .await
            }
            ProviderProtocol::Anthropic => {
                stream_anthropic(
                    http, &fresh, model_id, system, &req_msgs, None, &[], &sink_tx,
                )
                .await
            }
        };

        match res {
            Ok(reply) if !reply.text.trim().is_empty() => {
                return Ok(reply.text.trim().to_string());
            }
            Ok(_) => break,
            Err(err) if is_context_overflow_error(&format!("{err:#}")) => {
                // Drop oldest atomic group and retry (Codex backoff pattern).
                let groups = atomic_group_ranges(&working);
                if groups.len() <= 1 {
                    break;
                }
                let drop_end = groups[0].end;
                working.drain(..drop_end);
            }
            Err(_) => break,
        }
    }

    Ok(deterministic_fallback_summary(slice, focus))
}

/// Deterministic fallback summary used if LLM summarization is unavailable or fails.
pub fn deterministic_fallback_summary(messages: &[ChatMessage], focus: Option<&str>) -> String {
    let mut goals = Vec::new();
    let mut tools = Vec::new();
    let mut notes = Vec::new();

    for m in messages {
        match m.role {
            ChatRole::User => {
                let first = m.content.lines().next().unwrap_or("").trim();
                if !first.is_empty() && !first.starts_with("<compacted_context>") {
                    goals.push(truncate_chars(first, 140));
                }
            }
            ChatRole::Assistant => {
                for tc in &m.tool_calls {
                    tools.push(format!("`{}`({})", tc.name, truncate_chars(&tc.arguments, 80)));
                }
                let line = m.content.trim();
                if !line.is_empty() {
                    notes.push(truncate_chars(line.lines().last().unwrap_or(""), 140));
                }
            }
            ChatRole::Tool => {
                if let Some(last) = m.content.lines().last() {
                    if last.contains("exit=") || last.contains("error") {
                        tools.push(format!(
                            "  -> {} {}",
                            m.tool_name.as_deref().unwrap_or("tool"),
                            truncate_chars(last, 100)
                        ));
                    }
                }
            }
            ChatRole::System => {}
        }
    }

    let mut out = String::from("## Fallback Context Summary (rule-based)\n");
    if let Some(f) = focus.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!("- Focus: {f}\n"));
    }
    if !goals.is_empty() {
        out.push_str("\n### User Requests\n");
        for g in goals.into_iter().rev().take(6).rev() {
            out.push_str(&format!("- {g}\n"));
        }
    }
    if !tools.is_empty() {
        out.push_str("\n### Recent Tool Actions\n");
        for t in tools.into_iter().rev().take(MAX_FALLBACK_LINES).rev() {
            out.push_str(&format!("- {t}\n"));
        }
    }
    if !notes.is_empty() {
        out.push_str("\n### Assistant Notes\n");
        for n in notes.into_iter().rev().take(5).rev() {
            out.push_str(&format!("- {n}\n"));
        }
    }
    out
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ToolCall;

    fn user(s: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::User,
            content: s.into(),
            reasoning: None,
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
        }
    }

    fn assistant_tools(id: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Assistant,
            content: String::new(),
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "exec".into(),
                arguments: r#"{"command":"cargo test"}"#.into(),
            }],
            tool_call_id: None,
            tool_name: None,
        }
    }

    fn tool(id: &str, body: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Tool,
            content: body.into(),
            reasoning: None,
            tool_calls: vec![],
            tool_call_id: Some(id.into()),
            tool_name: Some("exec".into()),
        }
    }

    #[test]
    fn split_head_tail_never_orphans_tool_calls() {
        let msgs = vec![
            user(&"goal ".repeat(400)),
            assistant_tools("call_1"),
            tool("call_1", &"ok ".repeat(500)),
            user("second request"),
            assistant_tools("call_2"),
            tool("call_2", &"pass ".repeat(200)),
        ];
        // Tiny tail budget forces split around recent groups.
        let (head, tail) = split_head_tail(&msgs, 300, true);
        assert!(!head.is_empty());
        assert!(!tail.is_empty());
        assert_ne!(tail[0].role, ChatRole::Tool);
        if let Some(last_head) = head.last() {
            assert!(last_head.tool_calls.is_empty());
        }
    }

    #[test]
    fn assembled_messages_preserve_valid_alternation() {
        let out = assemble_compacted_messages(
            "## Goal & Constraints\n- Fix bug\n",
            vec![user("continue")],
            "local",
            Path::new("/tmp/proj"),
        );
        assert_eq!(out[0].role, ChatRole::User);
        assert!(out[0].content.contains("<compacted_context>"));
        assert!(out[0].content.contains("cwd=/tmp/proj"));
        assert_eq!(out[1].role, ChatRole::Assistant);
        assert_eq!(out[2].role, ChatRole::User);
    }

    #[test]
    fn summary_prompt_includes_focus_and_required_sections() {
        let p = build_summary_prompt(Some("keep failing test names"), false);
        assert!(p.contains("## Goal & Constraints"));
        assert!(p.contains("## Decisions & Rejected Alternatives"));
        assert!(p.contains("## Files, Commands & Exit Codes"));
        assert!(p.contains("keep failing test names"));
    }
}
