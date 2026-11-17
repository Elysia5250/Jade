//! OpenAI Codex Responses streaming.
use super::sse::SseParser;
use super::StreamReply;
use crate::agent::events::AgentEvent;
use crate::agent::tools::responses_tools_json_for;
use crate::auth::{Credential, StoredProvider};
use crate::permissions::PermissionEngine;
use crate::session::{ChatMessage, ChatRole, ToolCall};
use anyhow::{bail, Context, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

pub(crate) async fn stream_openai_codex(
    http: &reqwest::Client,
    provider: &StoredProvider,
    model: &str,
    system: &str,
    history: &[ChatMessage],
    permissions: Option<&PermissionEngine>,
    extra_tools: &[Value],
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    let (token, account_id) = match &provider.credential {
        Credential::OAuth {
            access_token,
            account_id,
            ..
        } => (access_token.clone(), account_id.clone()),
        Credential::ApiKey { key } => (key.clone(), None),
    };

    let mut input = Vec::new();
    for m in history {
        match m.role {
            ChatRole::System => {}
            ChatRole::User => {
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": m.content }],
                }));
            }
            ChatRole::Assistant => {
                if !m.content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": m.content }],
                    }));
                }
                for tc in &m.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": tc.name,
                        "arguments": tc.arguments,
                    }));
                }
            }
            ChatRole::Tool => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": m.tool_call_id.as_deref().unwrap_or(""),
                    "output": m.content,
                }));
            }
        }
    }

    let body = json!({
        "model": model,
        "instructions": system,
        "input": input,
        "tools": responses_tools_json_for(permissions, extra_tools),
        "stream": true,
        "store": false,
    });

    let mut req = http
        .post(&provider.base_url)
        .bearer_auth(&token)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .header("originator", "envoy");
    if let Some(acct) = account_id {
        req = req.header("ChatGPT-Account-ID", acct);
    }

    let resp = req
        .json(&body)
        .send()
        .await
        .context("calling OpenAI Codex endpoint")?;
    let status = resp.status();
    if !status.is_success() {
        let err_body = resp.text().await.unwrap_or_default();
        bail!("Codex API error ({status}): {err_body}");
    }

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: BTreeMap<String, (String, String, String)> = BTreeMap::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;

    let mut sse = SseParser::default();
    let mut bytes = resp.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk?;
        for ev in sse.feed(&chunk) {
            let data = ev.data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            let kind = v["type"].as_str().unwrap_or(ev.event.as_str());
            match kind {
                "response.output_text.delta" => {
                    if let Some(d) = v["delta"].as_str() {
                        text.push_str(d);
                        let _ = tx.send(AgentEvent::TextDelta(d.to_string()));
                    }
                }
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    if let Some(d) = v["delta"].as_str() {
                        reasoning.push_str(d);
                        let _ = tx.send(AgentEvent::ReasoningDelta(d.to_string()));
                    }
                }
                "response.output_item.added" => {
                    let item = &v["item"];
                    if item["type"].as_str() == Some("function_call") {
                        let item_id = item["id"].as_str().unwrap_or("").to_string();
                        let call_id = item["call_id"].as_str().unwrap_or(&item_id).to_string();
                        let name = item["name"].as_str().unwrap_or("").to_string();
                        let args = item["arguments"].as_str().unwrap_or("").to_string();
                        calls.insert(item_id, (call_id, name, args));
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(item_id) = v["item_id"].as_str() {
                        if let Some(d) = v["delta"].as_str() {
                            if let Some(entry) = calls.get_mut(item_id) {
                                entry.2.push_str(d);
                            }
                        }
                    }
                }
                "response.output_item.done" => {
                    let item = &v["item"];
                    if item["type"].as_str() == Some("function_call") {
                        let item_id = item["id"].as_str().unwrap_or("").to_string();
                        let call_id = item["call_id"].as_str().unwrap_or(&item_id).to_string();
                        let name = item["name"].as_str().unwrap_or("").to_string();
                        let args = item["arguments"].as_str().unwrap_or("").to_string();
                        calls.insert(item_id, (call_id, name, args));
                    }
                }
                "response.completed" => {
                    let usage = &v["response"]["usage"];
                    input_tokens = usage["input_tokens"].as_u64().unwrap_or(input_tokens);
                    output_tokens = usage["output_tokens"].as_u64().unwrap_or(output_tokens);
                }
                "response.failed" | "error" => {
                    let msg = v["response"]["error"]["message"]
                        .as_str()
                        .or_else(|| v["error"]["message"].as_str())
                        .or_else(|| v["message"].as_str())
                        .unwrap_or("Codex stream error");
                    bail!("{msg}");
                }
                _ => {}
            }
        }
    }

    let tool_calls = calls
        .into_values()
        .map(|(id, name, arguments)| ToolCall {
            id,
            name,
            arguments,
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
