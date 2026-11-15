//! Session & transcript persistence under `<data_dir>/sessions/`.
//!
//! Each session tracks both the visual opencode-style transcript (`items`: user
//! cards, assistant text/reasoning, collapsible tool/shell cards) and the LLM
//! conversation history (`messages`) so resuming a session restores everything.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
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
}

fn default_true() -> bool {
    true
}

impl TranscriptItem {
    pub fn id(&self) -> u64 {
        match self {
            TranscriptItem::User { id, .. }
            | TranscriptItem::Assistant { id, .. }
            | TranscriptItem::Tool { id, .. }
            | TranscriptItem::Notice { id, .. } => *id,
        }
    }

    pub fn push_output_text(&mut self, text: &str, err: bool) {
        if let TranscriptItem::Tool { output, .. } = self {
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
            output.push(OutputLine { text: line, err });
        }
    }
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
    #[serde(default)]
    pub next_item_id: u64,
    #[serde(default)]
    pub items: Vec<TranscriptItem>,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
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
            next_item_id: 1,
            items: Vec::new(),
            messages: Vec::new(),
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

pub fn save_session(data_dir: &Path, session: &Session) -> Result<()> {
    if session.items.is_empty() && session.messages.is_empty() {
        return Ok(());
    }
    let dir = sessions_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let tmp_dir = data_dir.join("tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let data = serde_json::to_string_pretty(session)?;
    let tmp = tmp_dir.join(format!("{}.json.tmp", session.id));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, dir.join(format!("{}.json", session.id)))
        .context("writing session file")?;
    Ok(())
}

pub fn load_session(data_dir: &Path, id: &str) -> Result<Session> {
    let path = sessions_dir(data_dir).join(format!("{id}.json"));
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading session {}", path.display()))?;
    let session: Session = serde_json::from_str(&raw)?;
    Ok(session)
}

pub fn delete_session(data_dir: &Path, id: &str) -> Result<()> {
    let path = sessions_dir(data_dir).join(format!("{id}.json"));
    if path.exists() {
        std::fs::remove_file(path)?;
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
    out.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
    out
}

pub fn latest_session_for_cwd(data_dir: &Path, cwd: &Path) -> Option<Session> {
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
    list.first().and_then(|s| load_session(data_dir, &s.id).ok())
}

fn new_session_id(now_ms: u64) -> String {
    let mut b = [0u8; 3];
    let _ = getrandom::fill(&mut b);
    format!("ses_{now_ms:x}_{:02x}{:02x}{:02x}", b[0], b[1], b[2])
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

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
