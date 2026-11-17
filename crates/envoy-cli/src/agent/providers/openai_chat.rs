//! OpenAI Chat Completions streaming.
use super::sse::SseParser;
use super::StreamReply;
use crate::agent::events::AgentEvent;
use crate::agent::tools::openai_tools_json_for;
use crate::auth::{copilot_session_token, Credential, StoredProvider};
use crate::permissions::PermissionEngine;
use crate::session::{ChatMessage, ChatRole, ToolCall};
use anyhow::{bail, Context, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

pub(crate) async fn stream_openai_chat(
    http: &reqwest::Client,
    provider: &StoredProvider,
    model: &str,
    system: &str,
    history: &[ChatMessage],
    permissions: Option<&PermissionEngine>,
    extra_tools: &[Value],
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    let (bearer, base_url) = match &provider.credential {
        Credential::ApiKey { key } => (key.clone(), provider.base_url.clone()),
        Credential::OAuth {
            access_token,
            method_id,
            ..
        } => {
            if method_id == "github-copilot" {
                copilot_session_token(http, access_token).await?
            } else {
                (access_token.clone(), provider.base_url.clone())
            }
        }
    };

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut msgs = vec![json!({ "role": "system", "content": system })];
    for m in history {
        match m.role {
            ChatRole::System => {}
            ChatRole::User => msgs.push(json!({ "role": "user", "content": m.content })),
            ChatRole::Assistant => {
                if m.tool_calls.is_empty() {
                    msgs.push(json!({ "role": "assistant", "content": m.content }));
                } else {
                    let calls: Vec<Value> = m
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.arguments,
                                }
                            })
                        })
                        .collect();
                    msgs.push(json!({
                        "role": "assistant",
                        "content": if m.content.is_empty() { Value::Null } else { json!(m.content) },
                        "tool_calls": calls,
                    }));
                }
            }
            ChatRole::Tool => {
                msgs.push(json!({
                    "role": "tool",
                    "tool_call_id": m.tool_call_id.as_deref().unwrap_or(""),
                    "content": m.content,
                }));
            }
        }
    }

    let body = json!({
        "model": model,
        "messages": msgs,
        "tools": openai_tools_json_for(permissions, extra_tools),
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    let mut req = http
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream");
    if !bearer.is_empty() {
        req = req.bearer_auth(&bearer);
    }
    if provider.id == "github-copilot" {
        req = req
            .header("Editor-Version", "vscode/1.99.0")
            .header("Copilot-Integration-Id", "vscode-chat");
    }

    let resp = req.json(&body).send().await.context("calling LLM API")?;
    let status = resp.status();
    if !status.is_success() {
        let err_body = resp.text().await.unwrap_or_default();
        bail!("LLM API error ({status}): {err_body}");
    }

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut partial_calls: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;

    let mut sse = SseParser::default();
    let mut bytes = resp.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk?;
        for ev in sse.feed(&chunk) {
            let data = ev.data.trim();
            if data == "[DONE]" || data.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                input_tokens = u["prompt_tokens"].as_u64().unwrap_or(input_tokens);
                output_tokens = u["completion_tokens"].as_u64().unwrap_or(output_tokens);
            }
            let Some(choice) = v["choices"].as_array().and_then(|c| c.first()) else {
                continue;
            };
            let delta = &choice["delta"];
            if let Some(r) = delta["reasoning_content"]
                .as_str()
                .or_else(|| delta["reasoning"].as_str())
            {
                if !r.is_empty() {
                    reasoning.push_str(r);
                    let _ = tx.send(AgentEvent::ReasoningDelta(r.to_string()));
                }
            }
            if let Some(c) = delta["content"].as_str() {
                if !c.is_empty() {
                    text.push_str(c);
                    let _ = tx.send(AgentEvent::TextDelta(c.to_string()));
                }
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    let idx = call["index"].as_u64().unwrap_or(0) as usize;
                    let entry = partial_calls.entry(idx).or_default();
                    if let Some(id) = call["id"].as_str() {
                        entry.0 = id.to_string();
                    }
                    if let Some(name) = call["function"]["name"].as_str() {
                        entry.1.push_str(name);
                    }
                    if let Some(args) = call["function"]["arguments"].as_str() {
                        entry.2.push_str(args);
                    }
                }
            }
        }
    }

    let tool_calls = partial_calls
        .into_iter()
        .enumerate()
        .map(|(i, (_idx, (id, name, arguments)))| ToolCall {
            id: if id.is_empty() {
                format!("call_{i}")
            } else {
                id
            },
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
