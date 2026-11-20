//! OpenAI Chat Completions request body: cache key and reasoning replay.
use super::prompt_cache_key;
use crate::agent::tools::{openai_tools_json_filtered, ToolFilter};
use crate::permissions::PermissionEngine;
use crate::session::{ChatMessage, ChatRole, ReasoningBlock};
use serde_json::{json, Value};

/// DeepSeek-R1 style models echo `reasoning_content` on the same turn.
/// The provider id is not the signal: a gateway can host the model.
pub(crate) fn echoes_reasoning_content(model_id: &str) -> bool {
    crate::model_registry::echoes_reasoning_content(model_id)
}

pub(crate) fn chat_body(
    model: &str,
    system: &str,
    history: &[ChatMessage],
    permissions: Option<&PermissionEngine>,
    tool_filter: &ToolFilter,
    extra_tools: &[Value],
    session_id: &str,
) -> Value {
    let tools = openai_tools_json_filtered(permissions, tool_filter, extra_tools);
    let mut body = json!({
        "model": model,
        "messages": chat_messages(model, system, history),
        "stream": true,
        "stream_options": { "include_usage": true },
        "prompt_cache_key": prompt_cache_key(session_id),
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    body
}

pub(crate) fn chat_messages(model: &str, system: &str, history: &[ChatMessage]) -> Vec<Value> {
    let mut msgs = vec![json!({ "role": "system", "content": system })];
    for (index, message) in history.iter().enumerate() {
        match message.role {
            ChatRole::System => {}
            ChatRole::User => msgs.push(json!({ "role": "user", "content": message.content })),
            ChatRole::Assistant => {
                let mut msg = assistant_message(message);
                if let Some(text) = replay_reasoning(model, history, index) {
                    msg["reasoning_content"] = json!(text);
                }
                msgs.push(msg);
            }
            ChatRole::Tool => {
                msgs.push(json!({
                    "role": "tool",
                    "tool_call_id": message.tool_call_id.as_deref().unwrap_or(""),
                    "content": message.content,
                }));
            }
        }
    }
    msgs
}

fn assistant_message(message: &ChatMessage) -> Value {
    if message.tool_calls.is_empty() {
        return json!({ "role": "assistant", "content": message.content });
    }
    let calls: Vec<Value> = message
        .tool_calls
        .iter()
        .map(|tc| {
            json!({
                "id": tc.id,
                "type": "function",
                "function": { "name": tc.name, "arguments": tc.arguments }
            })
        })
        .collect();
    json!({
        "role": "assistant",
        "content": if message.content.is_empty() { Value::Null } else { json!(message.content) },
        "tool_calls": calls,
    })
}

/// Same-turn tool calls keep `reasoning_content`. A later user message strips it,
/// except synthetic `<environment_update>` / `<pre_model_context>` tail injections.
fn replay_reasoning(model: &str, history: &[ChatMessage], index: usize) -> Option<String> {
    if !echoes_reasoning_content(model) {
        return None;
    }
    let later_user = history[index + 1..].iter().any(|message| {
        message.role == ChatRole::User && !crate::agent::prompt::is_synthetic_tail_message(message)
    });
    if later_user {
        return None;
    }
    let message = &history[index];
    if let Some(text) = message.reasoning.as_deref().filter(|text| !text.is_empty()) {
        return Some(text.to_string());
    }
    message.reasoning_blocks.iter().find_map(|block| match block {
        ReasoningBlock::Thinking { text, .. } if !text.is_empty() => Some(text.clone()),
        _ => None,
    })
}

pub(crate) fn cached_tokens(usage: &Value) -> u64 {
    usage["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| usage["input_tokens_details"]["cached_tokens"].as_u64())
        .unwrap_or(0)
}
