//! Agent event stream.
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
    pub recommended: bool,
}
