//! Agent event stream.
use super::monitors::MonitorInfo;
use crate::permissions::PermissionMode;
use crate::session::ChatMessage;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentEvent {
    /// The daemon started a fresh session (it was restarted): rebuild the view.
    SessionReset,
    /// A user message entered the session (prompt, steer, or queued follow-up).
    UserMessage {
        text: String,
        mode: String,
    },
    AssistantStart {
        model: String,
    },
    ReasoningDelta(String),
    TextDelta(String),
    ToolStart {
        call_id: String,
        name: String,
        title: String,
        subtitle: String,
    },
    ToolJob {
        call_id: String,
        job_id: u64,
    },
    ToolOutput {
        call_id: String,
        text: String,
        err: bool,
    },
    ToolDone {
        call_id: String,
        ok: bool,
        duration_ms: u64,
        exit_code: Option<i32>,
    },
    /// A user-facing progress note from the `say` tool.
    Say {
        text: String,
        kind: String,
    },
    /// The `ask` tool wants a decision; the UI answers through [`AskBroker`].
    Ask {
        id: u64,
        question: String,
        options: Vec<AskOption>,
        recommended: usize,
        timeout_ms: u64,
    },
    /// The ask timed out (or was aborted) and the UI should close the prompt.
    AskClosed {
        id: u64,
    },
    TurnDone {
        messages: Vec<ChatMessage>,
        input_tokens: u64,
        output_tokens: u64,
        /// Prompt size of the last model call. This is the context window fill,
        /// not the sum of every step in the turn.
        #[serde(default)]
        context_tokens: u64,
    },
    TurnError(String),
    /// Context compaction or handoff generation started.
    Compacting {
        auto: bool,
        handoff: bool,
    },
    /// Compaction finished; daemon and UI replace model-visible messages while
    /// preserving the visual transcript.
    Compacted {
        messages: Vec<ChatMessage>,
        before_tokens: u64,
        after_tokens: u64,
        summary: String,
        auto: bool,
    },
    /// A `/handoff` summary is ready to seed a clean session.
    HandoffReady {
        summary: String,
        before_tokens: u64,
        after_tokens: u64,
        title: String,
    },
    /// Active session permission mode changed.
    PermissionModeChanged {
        mode: PermissionMode,
    },
    /// A workspace file was modified by `write` or `edit`.
    FileChanged {
        path: String,
        before: String,
        after: String,
        added: usize,
        removed: usize,
        existed_before: bool,
        restorable_before: bool,
    },
    /// A background monitor matched its pattern and delivered an in-session event.
    MonitorFired {
        id: u64,
        label: String,
        job_id: u64,
        matched_lines: Vec<String>,
        trigger_count: u32,
        max_triggers: Option<u32>,
        stopped: bool,
    },
    /// Synchronizes the session's active/recent monitor list to the TUI.
    MonitorsSync {
        monitors: Vec<MonitorInfo>,
    },
    /// Session rewound to a prior checkpoint; workspace files and history rolled back.
    CheckpointRewound {
        checkpoint_id: u64,
        label: String,
        messages: Vec<ChatMessage>,
        /// Zero-based user turn count remaining after rewind (counts non-steer user turns).
        remaining_user_turns: usize,
        restored_files: Vec<String>,
        deleted_files: Vec<String>,
        skipped_files: Vec<String>,
    },
    /// Deterministic lifecycle hooks ran at a stage boundary.
    HookRan {
        event: String,
        count: usize,
        blocked: bool,
        reason: String,
        duration_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
    pub recommended: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ChatRole;

    #[test]
    fn agent_events_round_trip_through_postcard() {
        let samples = vec![
            AgentEvent::Compacting {
                auto: true,
                handoff: false,
            },
            AgentEvent::Compacted {
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: "<compacted_context>\nsummary\n</compacted_context>".into(),
                    reasoning: None,
                    tool_calls: vec![],
                    tool_call_id: None,
                    tool_name: None,
                }],
                before_tokens: 180_000,
                after_tokens: 24_000,
                summary: "summary".into(),
                auto: true,
            },
            AgentEvent::HandoffReady {
                summary: "handoff body".into(),
                before_tokens: 220_000,
                after_tokens: 4_500,
                title: "handoff: phase 2".into(),
            },
            AgentEvent::FileChanged {
                path: "src/lib.rs".into(),
                before: String::new(),
                after: "fn main() {}".into(),
                added: 1,
                removed: 0,
                existed_before: false,
                restorable_before: true,
            },
            AgentEvent::MonitorFired {
                id: 3,
                label: "cargo watch".into(),
                job_id: 42,
                matched_lines: vec!["error[E0425]: cannot find value".into()],
                trigger_count: 1,
                max_triggers: Some(5),
                stopped: false,
            },
            AgentEvent::MonitorsSync {
                monitors: vec![MonitorInfo {
                    id: 3,
                    job_id: 42,
                    label: "cargo watch".into(),
                    pattern: "error|panic".into(),
                    command: "cargo watch -x test".into(),
                    trigger_count: 1,
                    max_triggers: Some(5),
                    owns_job: true,
                    active: true,
                }],
            },
            AgentEvent::CheckpointRewound {
                checkpoint_id: 2,
                label: "refactor auth".into(),
                messages: vec![],
                remaining_user_turns: 1,
                restored_files: vec!["src/auth.rs".into()],
                deleted_files: vec!["src/new_helper.rs".into()],
                skipped_files: vec![],
            },
            AgentEvent::HookRan {
                event: "PreToolUse".into(),
                count: 2,
                blocked: true,
                reason: "policy block".into(),
                duration_ms: 14,
            },
        ];
        for ev in samples {
            let bytes = postcard::to_allocvec(&ev).unwrap();
            let decoded: AgentEvent = postcard::from_bytes(&bytes).unwrap();
            match (&ev, &decoded) {
                (
                    AgentEvent::Compacted {
                        before_tokens: a, ..
                    },
                    AgentEvent::Compacted {
                        before_tokens: b, ..
                    },
                ) => assert_eq!(a, b),
                (
                    AgentEvent::HandoffReady { title: a, .. },
                    AgentEvent::HandoffReady { title: b, .. },
                ) => assert_eq!(a, b),
                (
                    AgentEvent::MonitorFired {
                        id: a,
                        trigger_count: ta,
                        ..
                    },
                    AgentEvent::MonitorFired {
                        id: b,
                        trigger_count: tb,
                        ..
                    },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(ta, tb);
                }
                (
                    AgentEvent::MonitorsSync { monitors: a },
                    AgentEvent::MonitorsSync { monitors: b },
                ) => assert_eq!(a, b),
                (
                    AgentEvent::CheckpointRewound {
                        checkpoint_id: a,
                        deleted_files: da,
                        ..
                    },
                    AgentEvent::CheckpointRewound {
                        checkpoint_id: b,
                        deleted_files: db,
                        ..
                    },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(da, db);
                }
                (
                    AgentEvent::HookRan {
                        event: ea,
                        count: ca,
                        blocked: ba,
                        ..
                    },
                    AgentEvent::HookRan {
                        event: eb,
                        count: cb,
                        blocked: bb,
                        ..
                    },
                ) => {
                    assert_eq!(ea, eb);
                    assert_eq!(ca, cb);
                    assert_eq!(ba, bb);
                }
                _ => {}
            }
        }
    }
}
