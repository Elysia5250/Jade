//! Replace old tool observations with a short stub. Tool calls stay.

use super::extract::{collect_idents, fold_source, is_source_path};
use super::prune::PruneReport;
use super::summary::atomic_group_ranges;
use super::tokens::{message_cost, TokenCalibration};
use crate::session::{ChatMessage, ChatRole};
use serde_json::Value;
use std::collections::HashSet;

const MASK_MIN_BYTES: usize = 2_000;
pub const MASK_MARKER: &str = "[masked ";

/// Mask tool results that sit outside the protected tail. A tool-call group is
/// either entirely inside the tail or entirely eligible, so a call is never
/// left behind without its result, or the reverse.
pub fn mask_old_observations(
    messages: &mut [ChatMessage],
    tail_keep_tokens: u64,
    calibration: TokenCalibration,
) -> PruneReport {
    let end = maskable_end(messages, tail_keep_tokens, calibration);
    let mut report = PruneReport::default();
    for idx in 0..end {
        if messages[idx].role != ChatRole::Tool {
            continue;
        }
        if messages[idx].content.len() <= MASK_MIN_BYTES
            || messages[idx].content.contains(MASK_MARKER)
        {
            continue;
        }
        let replacement = masked_text(messages, idx);
        if replacement.len() < messages[idx].content.len() {
            report.bytes_saved += messages[idx].content.len() - replacement.len();
            report.messages_pruned += 1;
            messages[idx].content = replacement;
        }
    }
    report
}

fn maskable_end(
    messages: &[ChatMessage],
    tail_keep_tokens: u64,
    calibration: TokenCalibration,
) -> usize {
    let groups = atomic_group_ranges(messages);
    if groups.is_empty() {
        return 0;
    }
    let mut acc = 0u64;
    let mut idx = groups.len();
    while idx > 0 {
        let range = groups[idx - 1].clone();
        let cost: u64 = messages[range]
            .iter()
            .map(|msg| message_cost(msg, calibration))
            .sum();
        if acc > 0 && acc + cost > tail_keep_tokens {
            break;
        }
        acc += cost;
        idx -= 1;
    }
    groups.get(idx).map(|range| range.start).unwrap_or(0)
}

fn masked_text(messages: &[ChatMessage], idx: usize) -> String {
    let tool = messages[idx].tool_name.as_deref().unwrap_or("tool");
    let call = find_call(messages, idx);
    let args = call.map(|tc| tc.arguments.as_str()).unwrap_or("");
    let path = json_str(args, &["path", "file", "file_path"]).unwrap_or_default();
    let command = json_str(args, &["command", "cmd"]).unwrap_or_default();
    let bytes = messages[idx].content.len();
    let lines = messages[idx].content.lines().count();
    let body = &messages[idx].content;

    if tool == "read" || is_source_path(&path) {
        if let Some(outline) = fold_source(&path, body, &focus_names(messages, idx, &path)) {
            return format!(
                "{MASK_MARKER}{tool} path={} bytes={bytes} lines={lines}]\n{outline}\nRe-read with `read` if you need a folded body.",
                display_arg(&path)
            );
        }
    }

    if tool == "exec" || tool == "job" || !command.is_empty() {
        let kept = log_keep(body);
        return format!(
            "{MASK_MARKER}{tool} command={} bytes={bytes}]\n{kept}\nFollow the log with `job` if you need the rest.",
            display_arg(&command)
        );
    }

    let kept = if is_source_path(&path) {
        body.lines().take(8).collect::<Vec<_>>().join("\n")
    } else {
        log_keep(body)
    };
    format!(
        "{MASK_MARKER}{tool} path={} bytes={bytes} lines={lines}]\n{kept}\nRe-read with `read` if you need the rest.",
        display_arg(&path)
    )
}

fn find_call(messages: &[ChatMessage], tool_idx: usize) -> Option<&crate::session::ToolCall> {
    let id = messages[tool_idx].tool_call_id.as_deref()?;
    messages[..tool_idx]
        .iter()
        .rev()
        .find_map(|msg| msg.tool_calls.iter().find(|call| call.id == id))
}

fn json_str(args: &str, keys: &[&str]) -> Option<String> {
    let value: Value = serde_json::from_str(args).ok()?;
    for key in keys {
        if let Some(text) = value[*key].as_str() {
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

fn display_arg(raw: &str) -> String {
    let trimmed = raw.trim().replace('\n', " ");
    if trimmed.is_empty() {
        return "-".into();
    }
    trimmed.chars().take(120).collect()
}

fn focus_names(messages: &[ChatMessage], tool_idx: usize, path: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    for msg in &messages[tool_idx + 1..] {
        for call in &msg.tool_calls {
            let Ok(value) = serde_json::from_str::<Value>(&call.arguments) else {
                continue;
            };
            let same = value["path"]
                .as_str()
                .is_some_and(|other| paths_match(other, path));
            if same && (call.name == "edit" || call.name == "write") {
                for key in ["old_string", "new_string", "content"] {
                    if let Some(text) = value[key].as_str() {
                        collect_idents(text, &mut names);
                    }
                }
            }
        }
        for line in msg.content.lines() {
            if is_error_line(line) {
                collect_idents(line, &mut names);
            }
        }
    }
    names
}

fn paths_match(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let a = a.trim_end_matches('/');
    let b = b.trim_end_matches('/');
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

fn is_error_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error") || lower.contains("panic") || lower.contains("failed")
}

fn log_keep(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    for line in &lines {
        if is_error_line(line) || line.contains("exit=") {
            push_unique(&mut kept, line);
        }
    }
    let tail_from = lines.len().saturating_sub(6);
    for line in &lines[tail_from..] {
        push_unique(&mut kept, line);
    }
    if kept.len() > 24 {
        kept.truncate(24);
    }
    kept.join("\n")
}

fn push_unique<'a>(kept: &mut Vec<&'a str>, line: &'a str) {
    if !kept.contains(&line) {
        kept.push(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ToolCall;

    fn user(text: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::User,
            content: text.into(),
            reasoning: None,
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
            reasoning_blocks: Vec::new(),
            images: Vec::new(),
        }
    }

    fn assistant_read(id: &str, path: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Assistant,
            content: String::new(),
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments: format!(r#"{{"path":"{path}"}}"#),
            }],
            tool_call_id: None,
            tool_name: None,
            reasoning_blocks: Vec::new(),
            images: Vec::new(),
        }
    }

    fn tool(id: &str, name: &str, body: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Tool,
            content: body.into(),
            reasoning: None,
            tool_calls: vec![],
            tool_call_id: Some(id.into()),
            tool_name: Some(name.into()),
            reasoning_blocks: Vec::new(),
            images: Vec::new(),
        }
    }

    #[test]
    fn mask_keeps_the_call_and_protects_the_tail_group() {
        let body = format!("fn drop_me() {{\n{}\n}}\n", "x".repeat(2_500));
        let mut msgs = vec![
            user("start"),
            assistant_read("c1", "src/lib.rs"),
            tool("c1", "read", &body),
            user(&"recent ".repeat(800)),
        ];
        let report = mask_old_observations(&mut msgs, 400, TokenCalibration::default());
        assert_eq!(report.messages_pruned, 1);
        assert_eq!(msgs[1].tool_calls[0].name, "read");
        assert!(msgs[1].tool_calls[0].arguments.contains("src/lib.rs"));
        assert!(msgs[2].content.contains("[masked read"));
        assert!(msgs[2].content.contains("src/lib.rs"));
        assert!(!msgs[2].content.contains(&"x".repeat(200)));
        assert!(msgs[3].content.starts_with("recent"));
    }

    #[test]
    fn last_tool_group_stays_even_when_it_is_large() {
        let body = "y".repeat(4_000);
        let mut msgs = vec![
            user("start"),
            assistant_read("c1", "src/lib.rs"),
            tool("c1", "read", &body),
        ];
        let report = mask_old_observations(&mut msgs, 50, TokenCalibration::default());
        assert_eq!(report.messages_pruned, 0);
        assert_eq!(msgs[2].content, body);
    }
}
