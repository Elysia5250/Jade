//! Session & transcript persistence under `<data_dir>/sessions/`.
//!
//! Each session tracks both the visual opencode-style transcript (`items`: user
//! cards, assistant text/reasoning, collapsible tool/shell cards) and the LLM
//! conversation history (`messages`) so resuming a session restores everything.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MAX_OUTPUT_LINES_PER_CARD: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Provider reasoning that must be replayed on a later request in the same
/// turn. The display string stays in [`ChatMessage::reasoning`]. Postcard is
/// not self-describing: this enum's variants are part of the agent protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningBlock {
    Thinking { text: String, signature: String },
    Redacted { data: String },
    Encrypted { id: String, content: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    // Keep every field on the wire. postcard is not self-describing, so a
    // `skip_serializing_if` here would shift the remaining fields when the
    // same message is sent over `agent.sock` (it broke TurnDone replay).
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Structured thinking, redacted thinking, or encrypted reasoning items.
    #[serde(default)]
    pub reasoning_blocks: Vec<ReasoningBlock>,
}

impl ChatMessage {
    pub fn text(role: ChatRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            reasoning_blocks: Vec::new(),
        }
    }
}

/// A new user turn is starting. Prior-turn reasoning must not be replayed.
pub fn strip_prior_turn_reasoning(messages: &mut [ChatMessage]) {
    for msg in messages {
        if msg.role == ChatRole::Assistant {
            msg.reasoning = None;
            msg.reasoning_blocks.clear();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardStatus {
    Queued,
    Running,
    Done,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputLine {
    pub text: String,
    pub err: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptItem {
    User {
        id: u64,
        text: String,
        mode: String,
        cwd: String,
        ts_ms: u64,
    },
    Assistant {
        id: u64,
        text: String,
        #[serde(default)]
        reasoning: String,
        #[serde(default = "default_true")]
        reasoning_collapsed: bool,
        model: String,
        done: bool,
        #[serde(default)]
        error: Option<String>,
    },
    Tool {
        id: u64,
        call_id: String,
        name: String,
        title: String,
        subtitle: String,
        #[serde(default)]
        job_id: Option<u64>,
        target: String,
        #[serde(default)]
        output: Vec<OutputLine>,
        status: CardStatus,
        #[serde(default)]
        collapsed: bool,
        #[serde(default)]
        duration_ms: Option<u64>,
        #[serde(default)]
        exit_code: Option<i32>,
        started_ms: u64,
    },
    Notice {
        id: u64,
        text: String,
        is_error: bool,
    },
    /// A user-facing progress note emitted by the `say` tool.
    Say {
        id: u64,
        text: String,
        level: String,
    },
}

fn default_true() -> bool {
    true
}

fn default_calibration_factor() -> f64 {
    1.0
}

impl TranscriptItem {
    pub fn id(&self) -> u64 {
        match self {
            TranscriptItem::User { id, .. }
            | TranscriptItem::Assistant { id, .. }
            | TranscriptItem::Tool { id, .. }
            | TranscriptItem::Notice { id, .. }
            | TranscriptItem::Say { id, .. } => *id,
        }
    }

    pub fn push_output_text(&mut self, text: &str, err: bool) {
        if let TranscriptItem::Tool { output, .. } = self {
            let text = strip_ansi(text);
            for line in text.split('\n') {
                // Keep non-empty lines or preserve blank lines inside multi-line chunks
                if output.len() >= MAX_OUTPUT_LINES_PER_CARD {
                    output.drain(..1);
                }
                output.push(OutputLine {
                    text: line.trim_end_matches('\r').to_string(),
                    err,
                });
            }
            // Trim trailing empty artifact from trailing `\n`
            if text.ends_with('\n') && output.last().is_some_and(|l| l.text.is_empty()) {
                output.pop();
            }
        }
    }

    pub fn push_output_line(&mut self, line: String, err: bool) {
        if let TranscriptItem::Tool { output, .. } = self {
            if output.len() >= MAX_OUTPUT_LINES_PER_CARD {
                output.drain(..1);
            }
            output.push(OutputLine {
                text: strip_ansi(&line),
                err,
            });
        }
    }
}

/// Strip ANSI escape sequences from command output.
///
/// Tool output is rendered as plain text, so a colour code would either show up
/// as garbage or leave a background colour set for the rest of the line.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameter/intermediate bytes, then a final byte @..~
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC: terminated by BEL or ST (ESC \)
                Some(']') => loop {
                    match chars.next() {
                        Some('\x07') | None => break,
                        Some('\x1b') => {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        Some(_) => {}
                    }
                },
                // Two-character escapes such as ESC ( B
                Some('(') | Some(')') | Some('*') | Some('+') => {
                    chars.next();
                }
                Some(_) | None => {}
            },
            // Carriage returns only confuse the transcript; callers trim them.
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod ansi_tests {
    use super::strip_ansi;

    #[test]
    fn strips_colour_and_osc() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m plain"), "red plain");
        assert_eq!(strip_ansi("a\x1b]0;title\x07b"), "ab");
        assert_eq!(strip_ansi("a\x1b]8;;http://x\x1b\\link"), "alink");
        assert_eq!(strip_ansi("no codes"), "no codes");
        assert_eq!(strip_ansi("x\r\ny"), "x\ny");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub before: String,
    pub after: String,
    pub added: usize,
    pub removed: usize,
    pub ts_ms: u64,
}

pub fn compute_diff_counts(before: &str, after: &str) -> (usize, usize) {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(before, after);
    let mut added = 0usize;
    let mut removed = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub target: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Last model call's prompt size. Drives the context-window meter.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Prompt-cache tokens reported by the provider across turns.
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_create_tokens: u64,
    /// Heuristic multiplier learned from uncompressed provider usage.
    #[serde(default = "default_calibration_factor")]
    pub calibration_factor: f64,
    #[serde(default)]
    pub next_item_id: u64,
    /// Last agent event applied to `items`; attach replays from here.
    #[serde(default)]
    pub event_seq: u64,
    #[serde(default)]
    pub items: Vec<TranscriptItem>,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub changed_files: Vec<FileChange>,
}

impl Session {
    pub fn new(cwd: PathBuf, target: String) -> Self {
        let now = now_ms();
        Self {
            id: new_session_id(now),
            title: "New session".to_string(),
            cwd,
            target,
            created_ms: now,
            updated_ms: now,
            input_tokens: 0,
            output_tokens: 0,
            prompt_tokens: 0,
            cache_read_tokens: 0,
            cache_create_tokens: 0,
            calibration_factor: default_calibration_factor(),
            next_item_id: 1,
            event_seq: 0,
            items: Vec::new(),
            messages: Vec::new(),
            changed_files: Vec::new(),
        }
    }

    pub fn record_file_change(
        &mut self,
        path: String,
        before: String,
        after: String,
    ) -> (usize, usize) {
        let now = now_ms();
        if let Some(existing) = self.changed_files.iter_mut().find(|f| f.path == path) {
            existing.after = after;
            let (added, removed) = compute_diff_counts(&existing.before, &existing.after);
            existing.added = added;
            existing.removed = removed;
            existing.ts_ms = now;
            (added, removed)
        } else {
            let (added, removed) = compute_diff_counts(&before, &after);
            self.changed_files.push(FileChange {
                path,
                before,
                after,
                added,
                removed,
                ts_ms: now,
            });
            (added, removed)
        }
    }

    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_item_id.max(1);
        self.next_item_id = id + 1;
        id
    }

    pub fn maybe_set_title_from(&mut self, prompt: &str) {
        if self.title == "New session" {
            let first = prompt
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("Session");
            let truncated: String = first.chars().take(48).collect();
            self.title = if first.chars().count() > 48 {
                format!("{truncated}…")
            } else {
                truncated
            };
        }
    }

    pub fn touch(&mut self) {
        self.updated_ms = now_ms();
    }
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub updated_ms: u64,
    pub item_count: usize,
}

pub fn sessions_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sessions")
}

pub fn checked_session_id(id: &str) -> Result<&str> {
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.contains('/')
        || id.contains('\\')
        || id.chars().any(|c| c.is_control())
    {
        bail!("invalid session id");
    }
    Ok(id)
}

pub fn save_session(data_dir: &Path, session: &Session) -> Result<()> {
    if session.items.is_empty() && session.messages.is_empty() {
        return Ok(());
    }
    checked_session_id(&session.id)?;
    let dir = sessions_dir(data_dir);
    envoy_core::fs::ensure_private_dir(&dir)?;
    let data = serde_json::to_string_pretty(session)?;
    envoy_core::fs::write_atomic_sync(
        &dir.join(format!("{}.json", session.id)),
        data.as_bytes(),
        0o600,
    )
    .context("writing session file")?;
    Ok(())
}

pub fn load_session(data_dir: &Path, id: &str) -> Result<Session> {
    checked_session_id(id)?;
    let path = sessions_dir(data_dir).join(format!("{id}.json"));
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading session {}", path.display()))?;
    let session: Session = serde_json::from_str(&raw)?;
    Ok(session)
}

pub fn delete_session(data_dir: &Path, id: &str) -> Result<()> {
    checked_session_id(id)?;
    let path = sessions_dir(data_dir).join(format!("{id}.json"));
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let checkpoint = data_dir.join("checkpoints").join(format!("{id}.json"));
    if std::fs::symlink_metadata(&checkpoint).is_ok_and(|meta| meta.is_file()) {
        std::fs::remove_file(checkpoint)?;
    }
    Ok(())
}

pub fn list_sessions(data_dir: &Path) -> Vec<SessionSummary> {
    let dir = sessions_dir(data_dir);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(s) = serde_json::from_str::<Session>(&raw) else {
            continue;
        };
        out.push(SessionSummary {
            id: s.id,
            title: s.title,
            cwd: s.cwd,
            updated_ms: s.updated_ms,
            item_count: s.items.len(),
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    out
}

/// One row per directory, keeping the newest session title.
pub struct WorkspaceHit {
    pub cwd: PathBuf,
    pub title: String,
    pub updated_ms: u64,
}

pub fn recent_workspaces(data_dir: &Path) -> Vec<WorkspaceHit> {
    let mut best: BTreeMap<PathBuf, WorkspaceHit> = BTreeMap::new();
    for summary in list_sessions(data_dir) {
        let key = std::fs::canonicalize(&summary.cwd).unwrap_or_else(|_| summary.cwd.clone());
        let hit = best.entry(key.clone()).or_insert(WorkspaceHit {
            cwd: key,
            title: String::new(),
            updated_ms: 0,
        });
        if summary.updated_ms >= hit.updated_ms {
            hit.updated_ms = summary.updated_ms;
            hit.title = summary.title;
            hit.cwd = std::fs::canonicalize(&summary.cwd).unwrap_or(summary.cwd);
        }
    }
    let mut rows: Vec<_> = best.into_values().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.updated_ms));
    rows.truncate(12);
    rows
}

pub fn exact_session_for_cwd(data_dir: &Path, cwd: &Path) -> Option<Session> {
    let list = list_sessions(data_dir);
    let canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    for s in &list {
        let sc = std::fs::canonicalize(&s.cwd).unwrap_or_else(|_| s.cwd.clone());
        if sc == canon {
            if let Ok(loaded) = load_session(data_dir, &s.id) {
                return Some(loaded);
            }
        }
    }
    None
}

pub fn latest_session_for_cwd(data_dir: &Path, cwd: &Path) -> Option<Session> {
    if let Some(exact) = exact_session_for_cwd(data_dir, cwd) {
        return Some(exact);
    }
    list_sessions(data_dir)
        .first()
        .and_then(|s| load_session(data_dir, &s.id).ok())
}

fn new_session_id(now_ms: u64) -> String {
    let mut b = [0u8; 3];
    let _ = getrandom::fill(&mut b);
    format!("ses_{now_ms:x}_{:02x}{:02x}{:02x}", b[0], b[1], b[2])
}

pub fn now_ms() -> u64 {
    envoy_core::now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_messages_round_trip_over_postcard() {
        // The agent socket uses postcard. Optional/empty fields must still be
        // encoded, otherwise the reader shifts and loses the event.
        let message = ChatMessage {
            role: ChatRole::Assistant,
            content: "done".into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        
            reasoning_blocks: Vec::new(),};
        let bytes = postcard::to_stdvec(&message).unwrap();
        let back: ChatMessage = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.role, message.role);
        assert_eq!(back.content, message.content);
        assert!(back.reasoning.is_none());
        assert!(back.tool_calls.is_empty());
    }

    #[test]
    fn turn_done_round_trips_over_postcard() {
        let event = crate::agent::AgentEvent::TurnDone {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hi".into(),
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
                tool_name: None,
            
                reasoning_blocks: Vec::new(),}],
            input_tokens: 3,
            output_tokens: 4,
            context_tokens: 3,
            cache_read_tokens: 0,
            cache_create_tokens: 0,
            calibration_factor: 1.0,
        };
        let bytes = postcard::to_stdvec(&event).unwrap();
        let back: crate::agent::AgentEvent = postcard::from_bytes(&bytes).unwrap();
        match back {
            crate::agent::AgentEvent::TurnDone {
                messages,
                input_tokens,
                output_tokens,
                context_tokens,
                ..
            } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].content, "hi");
                assert_eq!((input_tokens, output_tokens, context_tokens), (3, 4, 3));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    fn tmpdir() -> PathBuf {
        let mut b = [0u8; 6];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "envoy-ses-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn saves_lists_and_reloads_sessions() {
        let dir = tmpdir();
        let mut s = Session::new(PathBuf::from("/tmp/proj"), "local".into());
        let id = s.alloc_id();
        s.maybe_set_title_from("fix the build error\nplease");
        assert_eq!(s.title, "fix the build error");
        s.items.push(TranscriptItem::User {
            id,
            text: "fix the build error".into(),
            mode: "Agent".into(),
            cwd: "/tmp/proj".into(),
            ts_ms: now_ms(),
        });
        save_session(&dir, &s).unwrap();

        let listed = list_sessions(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, s.id);

        let loaded = latest_session_for_cwd(&dir, Path::new("/tmp/proj")).unwrap();
        assert_eq!(loaded.id, s.id);
        assert_eq!(loaded.items.len(), 1);

        delete_session(&dir, &s.id).unwrap();
        assert!(list_sessions(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
