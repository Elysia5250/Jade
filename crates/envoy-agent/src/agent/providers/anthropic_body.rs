//! Anthropic message assembly: merged user turns, thinking replay, cache markers.
use crate::session::{ChatMessage, ChatRole, ReasoningBlock};
use serde_json::{json, Value};

pub(crate) const CACHE_EPHEMERAL: &str = "ephemeral";

pub(crate) fn anthropic_messages(history: &[ChatMessage]) -> Vec<Value> {
    let mut msgs = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();
    for message in history {
        match message.role {
            ChatRole::System => {}
            ChatRole::User => {
                let mut blocks = std::mem::take(&mut tool_results);
                push_text(&mut blocks, &message.content);
                push_user(&mut msgs, blocks);
            }
            ChatRole::Assistant => {
                flush_user(&mut msgs, &mut tool_results);
                let blocks = assistant_blocks(message);
                if !blocks.is_empty() {
                    msgs.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            ChatRole::Tool => {
                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.as_deref().unwrap_or(""),
                    "content": message.content,
                }));
            }
        }
    }
    flush_user(&mut msgs, &mut tool_results);
    mark_stable_prefix(&mut msgs);
    msgs
}

pub(crate) fn system_blocks(system: &str) -> Value {
    json!([{
        "type": "text",
        "text": system,
        "cache_control": { "type": CACHE_EPHEMERAL },
    }])
}

pub(crate) fn mark_last_tool(tools: &mut [Value]) {
    if let Some(last) = tools.last_mut() {
        if last.is_object() {
            last["cache_control"] = json!({ "type": CACHE_EPHEMERAL });
        }
    }
}

fn assistant_blocks(message: &ChatMessage) -> Vec<Value> {
    let mut blocks = Vec::new();
    for item in &message.reasoning_blocks {
        match item {
            ReasoningBlock::Thinking { text, signature } => {
                if text.is_empty() && signature.is_empty() {
                    continue;
                }
                let mut block = json!({ "type": "thinking", "thinking": text });
                if !signature.is_empty() {
                    block["signature"] = json!(signature);
                }
                blocks.push(block);
            }
            ReasoningBlock::Redacted { data } => {
                if !data.is_empty() {
                    blocks.push(json!({ "type": "redacted_thinking", "data": data }));
                }
            }
            ReasoningBlock::Encrypted { .. } => {}
        }
    }
    push_text(&mut blocks, &message.content);
    for call in &message.tool_calls {
        let input = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
        blocks.push(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": input,
        }));
    }
    blocks
}

fn push_text(blocks: &mut Vec<Value>, text: &str) {
    if text.is_empty() {
        return;
    }
    blocks.push(json!({ "type": "text", "text": text }));
}

fn flush_user(msgs: &mut Vec<Value>, tool_results: &mut Vec<Value>) {
    if tool_results.is_empty() {
        return;
    }
    push_user(msgs, std::mem::take(tool_results));
}

fn push_user(msgs: &mut Vec<Value>, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = msgs.last_mut() {
        if last["role"] == "user" {
            if let Some(existing) = last["content"].as_array_mut() {
                existing.extend(blocks);
                return;
            }
        }
    }
    msgs.push(json!({ "role": "user", "content": blocks }));
}

fn mark_stable_prefix(msgs: &mut [Value]) {
    for message in msgs.iter_mut().rev() {
        if stamp_stable_block(message) {
            return;
        }
    }
}

/// The breakpoint sits on the last block that is not a volatile tail.
fn stamp_stable_block(message: &mut Value) -> bool {
    let content = &mut message["content"];
    if let Some(text) = content.as_str() {
        if text.is_empty() || text_is_volatile(text) {
            return false;
        }
        *content = json!([{
            "type": "text",
            "text": text,
            "cache_control": { "type": CACHE_EPHEMERAL },
        }]);
        return true;
    }
    let Some(blocks) = content.as_array_mut() else {
        return false;
    };
    let Some(index) = blocks.iter().rposition(|block| !block_is_volatile(block)) else {
        return false;
    };
    if !blocks[index].is_object() {
        return false;
    }
    blocks[index]["cache_control"] = json!({ "type": CACHE_EPHEMERAL });
    true
}

fn block_is_volatile(block: &Value) -> bool {
    block["type"].as_str() == Some("text") && text_is_volatile(block["text"].as_str().unwrap_or(""))
}

fn text_is_volatile(text: &str) -> bool {
    text.starts_with("<environment_update>")
        || text.starts_with("<pre_model_context>")
        || text.starts_with("<hook_context>")
}
