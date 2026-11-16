//! Model provider adapters.
use crate::session::ToolCall;

mod anthropic;
mod openai_chat;
mod openai_codex;
mod sse;

pub(crate) use anthropic::stream_anthropic;
pub(crate) use openai_chat::stream_openai_chat;
pub(crate) use openai_codex::stream_openai_codex;

pub(super) struct StreamReply {
    pub(super) text: String,
    pub(super) reasoning: String,
    pub(super) tool_calls: Vec<ToolCall>,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
}
