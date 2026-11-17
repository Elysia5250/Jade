//! Anthropic Messages streaming.
use super::sse::SseParser;
use super::StreamReply;
use crate::agent::events::AgentEvent;
use crate::agent::tools::anthropic_tools_json_for;
use crate::auth::{Credential, StoredProvider};
use crate::permissions::PermissionEngine;
use crate::session::{ChatMessage, ChatRole, ToolCall};
use anyhow::{bail, Context, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

pub(crate) fn anthropic_max_tokens(max_output: Option<u64>) -> u32 {
    match max_output {
        Some(n) if n >= 256 => n.min(128_000) as u32,
        _ => 8192,
    }
}

pub(crate) async fn stream_anthropic(
    http: &reqwest::Client,
    provider: &StoredProvider,
    model: &str,
    system: &str,
    history: &[ChatMessage],
    permissions: Option<&PermissionEngine>,
    extra_tools: &[Value],
    max_tokens: u32,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    let url = format!("{}/messages", provider.base_url.trim_end_matches('/'));
    let mut msgs: Vec<Value> = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();

    for m in history {
        if m.role != ChatRole::Tool && !pending_tool_results.is_empty() {
            msgs.push(json!({
                "role": "user",
                "content": std::mem::take(&mut pending_tool_results),
            }));
        }
        match m.role {
            ChatRole::System => {}
            ChatRole::User => {
                msgs.push(json!({ "role": "user", "content": m.content }));
            }
            ChatRole::Assistant => {
                let mut blocks = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": m.content }));
                }
                for tc in &m.tool_calls {
                    let input: Value = serde_json::from_str(&tc.arguments).unwrap_or_else(|_| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": input,
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
                msgs.push(json!({ "role": "assistant", "content": blocks }));
            }
            ChatRole::Tool => {
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.as_deref().unwrap_or(""),
                    "content": m.content,
                }));
            }
        }
    }
    if !pending_tool_results.is_empty() {
        msgs.push(json!({ "role": "user", "content": pending_tool_results }));
    }

    let body = json!({
        "model": model,
        "max_tokens": max_tokens.max(256),
        "system": system,
        "messages": msgs,
        "tools": anthropic_tools_json_for(permissions, extra_tools),
        "stream": true,
    });

    let mut req = http
        .post(&url)
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream");

    match &provider.credential {
        Credential::ApiKey { key } => {
            req = req.header("x-api-key", key);
        }
        Credential::OAuth { access_token, .. } => {
            req = req
                .bearer_auth(access_token)
                .header(
                    "anthropic-beta",
                    "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14",
                );
        }
    }

    let resp = req
        .json(&body)
        .send()
        .await
        .context("calling Anthropic API")?;
    let status = resp.status();
    if !status.is_success() {
        let err_body = resp.text().await.unwrap_or_default();
        bail!("Anthropic API error ({status}): {err_body}");
    }

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut blocks: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;

    let mut sse = SseParser::default();
    let mut bytes = resp.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk?;
        for ev in sse.feed(&chunk) {
            let data = ev.data.trim();
            if data.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            match v["type"].as_str().unwrap_or("") {
                "message_start" => {
                    input_tokens = v["message"]["usage"]["input_tokens"]
                        .as_u64()
                        .unwrap_or(input_tokens);
                }
                "content_block_start" => {
                    let idx = v["index"].as_u64().unwrap_or(0) as usize;
                    let cb = &v["content_block"];
                    if cb["type"].as_str() == Some("tool_use") {
                        let id = cb["id"].as_str().unwrap_or("").to_string();
                        let name = cb["name"].as_str().unwrap_or("").to_string();
                        blocks.insert(idx, (id, name, String::new()));
                    }
                }
                "content_block_delta" => {
                    let idx = v["index"].as_u64().unwrap_or(0) as usize;
                    let d = &v["delta"];
                    match d["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            if let Some(t) = d["text"].as_str() {
                                text.push_str(t);
                                let _ = tx.send(AgentEvent::TextDelta(t.to_string()));
                            }
                        }
                        "thinking_delta" => {
                            if let Some(t) = d["thinking"].as_str() {
                                reasoning.push_str(t);
                                let _ = tx.send(AgentEvent::ReasoningDelta(t.to_string()));
                            }
                        }
                        "input_json_delta" => {
                            if let Some(j) = d["partial_json"].as_str() {
                                if let Some(entry) = blocks.get_mut(&idx) {
                                    entry.2.push_str(j);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    output_tokens = v["usage"]["output_tokens"]
                        .as_u64()
                        .unwrap_or(output_tokens);
                }
                "error" => {
                    let msg = v["error"]["message"]
                        .as_str()
                        .unwrap_or("Anthropic stream error");
                    bail!("{msg}");
                }
                _ => {}
            }
        }
    }

    let tool_calls = blocks
        .into_values()
        .map(|(id, name, args)| ToolCall {
            id,
            name,
            arguments: if args.is_empty() { "{}".into() } else { args },
        })
        .filter(|tc| !tc.name.is_empty())
        .collect();

    Ok(StreamReply {
        text,
        reasoning,
        tool_calls,
        input_tokens,
        output_tokens,
    })
}
