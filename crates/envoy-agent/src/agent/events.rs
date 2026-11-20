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
        /// Provider prompt-cache read tokens for this turn.
        #[serde(default)]
        cache_read_tokens: u64,
        /// Provider prompt-cache creation tokens for this turn.
        #[serde(default)]
        cache_create_tokens: u64,
        /// Calibration factor learned from uncompressed requests this turn.
        #[serde(default = "default_turn_calibration")]
        calibration_factor: f64,
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

fn default_turn_calibration() -> f64 {
    1.0
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

                    reasoning_blocks: Vec::new(),
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

    fn assert_postcard<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    {
        let bytes = postcard::to_allocvec(value).unwrap();
        let (decoded, rest): (T, &[u8]) = postcard::take_from_bytes(&bytes).unwrap();
        assert!(rest.is_empty(), "trailing {}", rest.len());
        assert_eq!(format!("{value:?}"), format!("{decoded:?}"));
        assert_eq!(bytes, postcard::to_allocvec(&decoded).unwrap());
    }

    fn rich_message(content: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Tool,
            content: content.into(),
            reasoning: Some("trace".into()),
            tool_calls: vec![crate::session::ToolCall {
                id: "call-9".into(),
                name: "bash".into(),
                arguments: "{\"cmd\":\"true\"}".into(),
            }],
            tool_call_id: Some("call-9".into()),
            tool_name: Some("bash".into()),
            reasoning_blocks: Vec::new(),
        }
    }

    fn sample_events_early() -> Vec<AgentEvent> {
        vec![
            AgentEvent::SessionReset,
            AgentEvent::UserMessage {
                text: "ship the fix".into(),
                mode: "Agent".into(),
            },
            AgentEvent::AssistantStart {
                model: "grok-4".into(),
            },
            AgentEvent::ReasoningDelta("thinking".into()),
            AgentEvent::TextDelta("hello".into()),
            AgentEvent::ToolStart {
                call_id: "c1".into(),
                name: "read".into(),
                title: "Read".into(),
                subtitle: "src/lib.rs".into(),
            },
            AgentEvent::ToolJob {
                call_id: "c1".into(),
                job_id: 15,
            },
            AgentEvent::ToolOutput {
                call_id: "c1".into(),
                text: "file body".into(),
                err: true,
            },
            AgentEvent::ToolDone {
                call_id: "c1".into(),
                ok: false,
                duration_ms: 40,
                exit_code: Some(2),
            },
            AgentEvent::Say {
                text: "note".into(),
                kind: "info".into(),
            },
            AgentEvent::Ask {
                id: 8,
                question: "apply?".into(),
                options: vec![AskOption {
                    label: "yes".into(),
                    description: Some("write the file".into()),
                    recommended: true,
                }],
                recommended: 1,
                timeout_ms: 30_000,
            },
            AgentEvent::AskClosed { id: 8 },
            AgentEvent::TurnDone {
                messages: vec![rich_message("done")],
                input_tokens: 100,
                output_tokens: 20,
                context_tokens: 42,
                cache_read_tokens: 7,
                cache_create_tokens: 3,
                calibration_factor: 1.0,
            },
        ]
    }

    fn sample_events_late() -> Vec<AgentEvent> {
        vec![
            AgentEvent::TurnError("no model configured".into()),
            AgentEvent::Compacting {
                auto: true,
                handoff: true,
            },
            AgentEvent::Compacted {
                messages: vec![rich_message("summary body")],
                before_tokens: 180_000,
                after_tokens: 24_000,
                summary: "summary".into(),
                auto: false,
            },
            AgentEvent::HandoffReady {
                summary: "handoff body".into(),
                before_tokens: 220_000,
                after_tokens: 4_500,
                title: "handoff: phase 4".into(),
            },
            AgentEvent::PermissionModeChanged {
                mode: crate::permissions::PermissionMode::Plan,
            },
            AgentEvent::FileChanged {
                path: "src/lib.rs".into(),
                before: "old".into(),
                after: "fn main() {}".into(),
                added: 1,
                removed: 2,
                existed_before: true,
                restorable_before: false,
            },
            AgentEvent::MonitorFired {
                id: 3,
                label: "cargo watch".into(),
                job_id: 42,
                matched_lines: vec!["error[E0425]".into()],
                trigger_count: 1,
                max_triggers: Some(5),
                stopped: true,
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
                    active: false,
                }],
            },
            AgentEvent::CheckpointRewound {
                checkpoint_id: 2,
                label: "refactor auth".into(),
                messages: vec![rich_message("rewound")],
                remaining_user_turns: 1,
                restored_files: vec!["src/auth.rs".into()],
                deleted_files: vec!["src/new_helper.rs".into()],
                skipped_files: vec!["src/skip.rs".into()],
            },
            AgentEvent::HookRan {
                event: "PreToolUse".into(),
                count: 2,
                blocked: true,
                reason: "policy block".into(),
                duration_ms: 14,
            },
        ]
    }

    fn event_index(event: &AgentEvent) -> usize {
        match event {
            AgentEvent::SessionReset => 0,
            AgentEvent::UserMessage { .. } => 1,
            AgentEvent::AssistantStart { .. } => 2,
            AgentEvent::ReasoningDelta(_) => 3,
            AgentEvent::TextDelta(_) => 4,
            AgentEvent::ToolStart { .. } => 5,
            AgentEvent::ToolJob { .. } => 6,
            AgentEvent::ToolOutput { .. } => 7,
            AgentEvent::ToolDone { .. } => 8,
            AgentEvent::Say { .. } => 9,
            AgentEvent::Ask { .. } => 10,
            AgentEvent::AskClosed { .. } => 11,
            AgentEvent::TurnDone { .. } => 12,
            AgentEvent::TurnError(_) => 13,
            AgentEvent::Compacting { .. } => 14,
            AgentEvent::Compacted { .. } => 15,
            AgentEvent::HandoffReady { .. } => 16,
            AgentEvent::PermissionModeChanged { .. } => 17,
            AgentEvent::FileChanged { .. } => 18,
            AgentEvent::MonitorFired { .. } => 19,
            AgentEvent::MonitorsSync { .. } => 20,
            AgentEvent::CheckpointRewound { .. } => 21,
            AgentEvent::HookRan { .. } => 22,
        }
    }

    #[test]
    fn every_agent_event_round_trips_with_no_trailing_bytes() {
        let mut samples = sample_events_early();
        samples.extend(sample_events_late());
        let mut seen = [false; 23];
        for event in &samples {
            let slot = event_index(event);
            assert!(!seen[slot], "duplicate sample {slot}");
            seen[slot] = true;
            assert_postcard(event);
        }
        assert!(seen.iter().all(|hit| *hit), "missing agent event");
        match samples
            .iter()
            .find(|event| matches!(event, AgentEvent::TurnDone { .. }))
        {
            Some(AgentEvent::TurnDone { context_tokens, .. }) => assert_eq!(*context_tokens, 42),
            _ => panic!("TurnDone sample missing"),
        }
    }
}
