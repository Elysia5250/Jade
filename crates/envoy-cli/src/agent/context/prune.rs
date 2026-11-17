//! Stage 1 context pruning: shrink old tool outputs without losing errors or recent tail.

use super::tokens::estimate_message_tokens;
use crate::session::{ChatMessage, ChatRole};

const PRUNE_MIN_BYTES: usize = 2_000;
const PRUNE_MARKER: &str = "[pruned older ";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub messages_pruned: usize,
    pub bytes_saved: usize,
}

/// Shrink oversized [`ChatRole::Tool`] payloads older than the protected
/// `tail_keep_tokens` window, keeping head/tail lines and exit codes.
pub fn prune_old_tool_outputs(
    messages: &mut [ChatMessage],
    tail_keep_tokens: u64,
) -> PruneReport {
    let tail_start = protected_tail_start(messages, tail_keep_tokens);
    let mut report = PruneReport::default();

    for msg in &mut messages[..tail_start] {
        if msg.role != ChatRole::Tool || msg.content.len() <= PRUNE_MIN_BYTES {
            continue;
        }
        if msg.content.contains(PRUNE_MARKER) {
            continue;
        }
        let tool_name = msg.tool_name.as_deref().unwrap_or("tool");
        let shrunk = shrink_tool_text(&msg.content, tool_name);
        if shrunk.len() < msg.content.len() {
            report.bytes_saved += msg.content.len() - shrunk.len();
            report.messages_pruned += 1;
            msg.content = shrunk;
        }
    }

    report
}

/// Find the index where the protected recent tail begins, keeping recent
/// messages up to `tail_keep_tokens` untouched by Stage 1 pruning.
fn protected_tail_start(messages: &[ChatMessage], tail_keep_tokens: u64) -> usize {
    if messages.is_empty() {
        return 0;
    }
    let mut acc = 0u64;
    let mut idx = messages.len();
    while idx > 0 {
        let cost = estimate_message_tokens(&messages[idx - 1]);
        if acc > 0 && acc + cost > tail_keep_tokens {
            break;
        }
        acc += cost;
        idx -= 1;
    }
    idx
}

fn shrink_tool_text(text: &str, tool_name: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 24 {
        return bound_tool_output(text, 900);
    }
    let head = lines[..8].join("\n");
    let tail = lines[lines.len().saturating_sub(10)..].join("\n");
    let omitted_lines = lines.len().saturating_sub(18);
    let omitted_chars = text.len().saturating_sub(head.len() + tail.len());
    format!(
        "{head}\n... {PRUNE_MARKER}{tool_name} output: {omitted_lines} lines / {omitted_chars} bytes; re-read or inspect job logs if needed] ...\n{tail}"
    )
}

/// Bound a single fresh tool result using a 38% head / 62% tail split so both
/// starting headers and trailing errors/exit codes survive.
pub fn bound_tool_output(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes || max_bytes < 256 {
        return s.to_string();
    }
    let reserve = 96usize;
    let budget = max_bytes.saturating_sub(reserve);
    let head_len = floor_char_boundary(s, budget * 38 / 100);
    let tail_target = s.len().saturating_sub(budget - head_len);
    let tail_start = ceil_char_boundary(s, tail_target);
    if tail_start <= head_len {
        return s.to_string();
    }
    let omitted = tail_start - head_len;
    format!(
        "{}\n... [truncated {omitted} bytes from middle] ...\n{}",
        &s[..head_len],
        &s[tail_start..]
    )
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prunes_old_tool_outputs_but_keeps_tail_and_exit_codes() {
        let big_log = (0..120)
            .map(|i| format!("line {i}: compiling module payload data"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n[job #7 exit=1]";
        let recent = "a".repeat(3_500);
        let mut msgs = vec![
            ChatMessage {
                role: ChatRole::User,
                content: "run tests".into(),
                reasoning: None,
                tool_calls: vec![],
                tool_call_id: None,
                tool_name: None,
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: big_log,
                reasoning: None,
                tool_calls: vec![],
                tool_call_id: Some("c1".into()),
                tool_name: Some("exec".into()),
            },
            ChatMessage {
                role: ChatRole::User,
                content: recent.clone(),
                reasoning: None,
                tool_calls: vec![],
                tool_call_id: None,
                tool_name: None,
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: recent,
                reasoning: None,
                tool_calls: vec![],
                tool_call_id: Some("c2".into()),
                tool_name: Some("read".into()),
            },
        ];
        // Tail budget covers the two recent 3.5k messages (~1750 tokens).
        let report = prune_old_tool_outputs(&mut msgs, 2_000);
        assert_eq!(report.messages_pruned, 1);
        assert!(msgs[1].content.contains("[job #7 exit=1]"));
        assert!(msgs[1].content.contains("[pruned older exec output"));
        assert!(!msgs[3].content.contains("[pruned older"));
    }

    #[test]
    fn bound_tool_output_preserves_head_and_tail() {
        let text = format!("HEADER\n{}\nFOOTER [job #3 exit=2]", "x".repeat(4_000));
        let bounded = bound_tool_output(&text, 600);
        assert!(bounded.starts_with("HEADER\n"));
        assert!(bounded.ends_with("FOOTER [job #3 exit=2]"));
        assert!(bounded.len() <= 600);
    }
}
