//! Stage 2 structured compaction summary, safe boundary splitting, and `/handoff`.

use super::is_context_overflow_error;
use super::tokens::{message_cost, TokenCalibration};
use crate::agent::events::AgentEvent;
use crate::agent::providers::{stream_anthropic, stream_openai_chat, stream_openai_codex};
use crate::auth::{ensure_fresh_provider, ProviderProtocol, StoredProvider};
use crate::session::{ChatMessage, ChatRole};
use anyhow::Result;
use std::ops::Range;
use std::path::Path;
use tokio::sync::mpsc;



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
    calibration: TokenCalibration,
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
            .map(|msg| message_cost(msg, calibration))
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
         Rules: Be factual, concise, and dense. Never invent file paths, symbols, or test outcomes.\n\
         If a previous summary is included, merge the new notes into these same sections. Keep earlier facts unless the new notes contradict them."
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
            content:
                "Understood. Continuing from the compacted context summary and recent messages."
                    .into(),
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
    let (previous, dossier) = condense_for_summary(slice);
    let mut body = summary_input(&previous, &dossier);
    let instruction = build_summary_prompt(focus, for_handoff);

    for attempt in 0..2 {
        if body.trim().is_empty() {
            break;
        }
        let req_msgs = vec![
            user_note(&body),
            user_note(&instruction),
        ];

        let fresh = ensure_fresh_provider(http, data_dir, provider.clone())
            .await
            .unwrap_or_else(|_| provider.clone());
        let (sink_tx, _sink_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let routed = fresh.routed_for(model_id);
        let res = match routed.protocol {
            ProviderProtocol::OpenAiChat => {
                stream_openai_chat(
                    http,
                    &routed,
                    model_id,
                    system,
                    &req_msgs,
                    None,
                    &[],
                    &sink_tx,
                )
                .await
            }
            ProviderProtocol::OpenAiCodex => {
                stream_openai_codex(
                    http,
                    &routed,
                    model_id,
                    system,
                    &req_msgs,
                    None,
                    &[],
                    &sink_tx,
                )
                .await
            }
            ProviderProtocol::Anthropic => {
                let cap = super::catalog::resolve_model_cap(data_dir, &fresh.id, model_id);
                stream_anthropic(
                    http,
                    &routed,
                    model_id,
                    system,
                    &req_msgs,
                    None,
                    &[],
                    crate::agent::providers::anthropic_max_tokens(cap.max_output),
                    &sink_tx,
                )
                .await
            }
        };

        match res {
            Ok(reply) if !reply.text.trim().is_empty() => {
                return Ok(reply.text.trim().to_string());
            }
            Ok(_) => break,
            Err(err) if attempt == 0 && is_context_overflow_error(&format!("{err:#}")) => {
                body = truncate_chars(&body, body.chars().count() / 2);
            }
            Err(_) => break,
        }
    }

    Ok(merge_summary(previous.as_deref(), &dossier))
}

fn user_note(text: &str) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        content: text.to_string(),
        reasoning: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_name: None,
    }
}

/// Split an old `<compacted_context>` off the head and turn the rest into notes.
pub fn condense_for_summary(messages: &[ChatMessage]) -> (Option<String>, String) {
    let mut rest = messages;
    let mut previous = None;
    if let Some(first) = rest.first() {
        if let Some(summary) = previous_summary(&first.content) {
            previous = Some(summary);
            rest = &rest[1..];
            if rest.first().is_some_and(|msg| {
                msg.role == ChatRole::Assistant
                    && msg.content.starts_with("Understood. Continuing from the compacted context")
            }) {
                rest = &rest[1..];
            }
        }
    }
    (previous, dossier_from(rest))
}

pub fn summary_input(previous: &Option<String>, dossier: &str) -> String {
    match previous {
        Some(prev) => format!("## Previous summary\n{prev}\n\n## Newly evicted notes\n{dossier}"),
        None => dossier.to_string(),
    }
}

fn previous_summary(content: &str) -> Option<String> {
    let start = content.find("<compacted_context>")? + "<compacted_context>".len();
    let end = content[start..].find("</compacted_context>")? + start;
    let summary = content[start..end].trim();
    if summary.is_empty() {
        None
    } else {
        Some(summary.to_string())
    }
}

fn dossier_from(messages: &[ChatMessage]) -> String {
    let mut users = Vec::new();
    let mut decisions = Vec::new();
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for msg in messages {
        match msg.role {
            ChatRole::User => {
                let first = msg.content.lines().next().unwrap_or("").trim();
                if first.is_empty()
                    || first.starts_with("<compacted_context>")
                    || first.starts_with("<environment>")
                {
                    continue;
                }
                users.push(truncate_chars(first, 160));
            }
            ChatRole::Assistant => {
                if let Some(line) = msg.content.lines().rev().find(|line| !line.trim().is_empty()) {
                    decisions.push(truncate_chars(line.trim(), 160));
                }
                if let Some(reasoning) = msg.reasoning.as_deref().filter(|s| !s.trim().is_empty()) {
                    decisions.push(format!("reasoning: {}", truncate_chars(reasoning.trim(), 300)));
                }
                for call in &msg.tool_calls {
                    files.push(format!(
                        "`{}` {}",
                        call.name,
                        truncate_chars(&call.arguments, 120)
                    ));
                }
            }
            ChatRole::Tool => {
                let name = msg.tool_name.as_deref().unwrap_or("tool");
                let excerpt = tool_excerpt(&msg.content);
                files.push(format!("`{name}` result: {excerpt}"));
                if msg.content.to_ascii_lowercase().contains("error") || msg.content.contains("exit=")
                {
                    errors.push(format!("`{name}`: {excerpt}"));
                }
            }
            ChatRole::System => {}
        }
    }
    let mut out = String::new();
    push_notes(&mut out, "### User requests", &users);
    push_notes(&mut out, "### Decisions", &decisions);
    push_notes(&mut out, "### Files, commands, and results", &files);
    push_notes(&mut out, "### Errors", &errors);
    if out.is_empty() {
        out.push_str("(no newly evicted notes)");
    }
    out
}

fn tool_excerpt(content: &str) -> String {
    let lines: Vec<&str> = content.lines().take(12).collect();
    truncate_chars(&lines.join("\n"), 800)
}

fn push_notes(out: &mut String, title: &str, notes: &[String]) {
    if notes.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(title);
    out.push('\n');
    for note in notes {
        out.push_str("- ");
        out.push_str(note);
        out.push('\n');
    }
}

/// Keep the previous six sections and append new notes under the matching heading.
pub fn merge_summary(previous: Option<&str>, dossier: &str) -> String {
    let mut summary = previous
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            "\
## Goal & Constraints\n\
## Completed Work & Verified Facts\n\
## Decisions & Rejected Alternatives\n\
## Files, Commands & Exit Codes\n\
## Open Issues & Blockers\n\
## Active State & Next Steps"
                .to_string()
        });
    append_section(
        &mut summary,
        "## Goal & Constraints",
        section_body(dossier, "### User requests"),
    );
    append_section(
        &mut summary,
        "## Decisions & Rejected Alternatives",
        section_body(dossier, "### Decisions"),
    );
    append_section(
        &mut summary,
        "## Files, Commands & Exit Codes",
        section_body(dossier, "### Files, commands, and results"),
    );
    append_section(
        &mut summary,
        "## Open Issues & Blockers",
        section_body(dossier, "### Errors"),
    );
    summary
}

fn section_body<'a>(dossier: &'a str, header: &str) -> &'a str {
    let Some(start) = dossier.find(header) else {
        return "";
    };
    let rest = &dossier[start + header.len()..];
    let end = rest.find("\n### ").unwrap_or(rest.len());
    rest[..end].trim()
}

fn append_section(summary: &mut String, header: &str, body: &str) {
    if body.is_empty() {
        return;
    }
    if let Some(idx) = summary.find(header) {
        let after = idx + header.len();
        let next = summary[after..]
            .find("\n## ")
            .map(|pos| after + pos)
            .unwrap_or(summary.len());
        let insert = format!("\n{body}\n");
        summary.insert_str(next, &insert);
    } else {
        summary.push_str("\n\n");
        summary.push_str(header);
        summary.push('\n');
        summary.push_str(body);
        summary.push('\n');
    }
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
        let (head, tail) = split_head_tail(&msgs, 300, true, TokenCalibration::default());
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
    fn second_compaction_input_is_previous_summary_plus_new_notes() {
        let previous = "\
<compacted_context>
## Goal & Constraints
- ship it
</compacted_context>

[Resumed environment: target=local, cwd=/tmp]";
        let huge = "Z".repeat(3_000);
        let msgs = vec![
            user(previous),
            ChatMessage {
                role: ChatRole::Assistant,
                content: "Understood. Continuing from the compacted context summary and recent messages."
                    .into(),
                reasoning: None,
                tool_calls: vec![],
                tool_call_id: None,
                tool_name: None,
            },
            user("add a flag"),
            ChatMessage {
                role: ChatRole::Assistant,
                content: "using --flag".into(),
                reasoning: Some("rejected a config file".into()),
                tool_calls: vec![],
                tool_call_id: None,
                tool_name: None,
            },
            tool("c9", &huge),
        ];
        let (prev, dossier) = condense_for_summary(&msgs);
        let input = summary_input(&prev, &dossier);
        assert!(prev.unwrap().contains("ship it"));
        assert!(input.contains("## Previous summary"));
        assert!(input.contains("ship it"));
        assert!(input.contains("add a flag"));
        assert!(input.contains("rejected a config file"));
        assert!(input.matches('Z').count() < 900);
        let merged = merge_summary(Some("## Goal & Constraints\n- ship it\n"), &dossier);
        assert!(merged.contains("ship it"));
        assert!(merged.contains("add a flag"));
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
