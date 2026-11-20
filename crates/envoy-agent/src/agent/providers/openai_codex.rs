//! OpenAI Codex Responses streaming.
use super::StreamReply;
use crate::agent::effort;
use crate::agent::events::AgentEvent;
use crate::agent::tools::{responses_tools_json_filtered, ToolFilter};
use crate::auth::{Credential, StoredProvider};
use crate::permissions::PermissionEngine;
use crate::session::{ChatMessage, ChatRole, ReasoningBlock, ToolCall};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::mpsc;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_openai_codex(
    http: &reqwest::Client,
    provider: &StoredProvider,
    model: &str,
    system: &str,
    history: &[ChatMessage],
    permissions: Option<&PermissionEngine>,
    tool_filter: &ToolFilter,
    extra_tools: &[Value],
    session_id: &str,
    data_dir: &Path,
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

    let mut body = codex_request_body(
        model,
        system,
        history,
        permissions,
        tool_filter,
        extra_tools,
        session_id,
    );
    effort::apply(&mut body, provider.protocol, &provider.id, model, data_dir);

    let mut req = http
        .post(codex_responses_url(&provider.base_url))
        .bearer_auth(&token)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .header("originator", "envoy");
    if let Some(acct) = account_id {
        req = req.header("ChatGPT-Account-ID", acct);
    }
    req = super::apply_opencode_session(req, provider, session_id);

    let resp = super::send_with_retry(req, &body, "calling OpenAI Codex endpoint").await?;
    let status = resp.status();
    if !status.is_success() {
        let err_body = resp.text().await.unwrap_or_default();
        bail!("Codex API error ({status}): {err_body}");
    }

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut order: Vec<String> = Vec::new();
    let mut calls: HashMap<String, CodexCall> = HashMap::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_read_tokens = 0u64;
    let mut reasoning_blocks: Vec<ReasoningBlock> = Vec::new();

    super::for_each_sse(resp.bytes_stream(), |ev| {
        let data = ev.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return Ok(());
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
                    remember_codex_call(&mut order, &mut calls, item, false);
                }
                remember_encrypted(&mut reasoning_blocks, item);
            }
            "response.function_call_arguments.delta" => {
                if let Some(item_id) = v["item_id"].as_str() {
                    if let Some(d) = v["delta"].as_str() {
                        let entry = codex_slot(&mut order, &mut calls, item_id);
                        entry.arguments.push_str(d);
                    }
                }
            }
            "response.output_item.done" => {
                let item = &v["item"];
                if item["type"].as_str() == Some("function_call") {
                    remember_codex_call(&mut order, &mut calls, item, true);
                }
                remember_encrypted(&mut reasoning_blocks, item);
            }
            "response.completed" => {
                let usage = &v["response"]["usage"];
                input_tokens = usage["input_tokens"].as_u64().unwrap_or(input_tokens);
                output_tokens = usage["output_tokens"].as_u64().unwrap_or(output_tokens);
                let cached = usage["input_tokens_details"]["cached_tokens"].as_u64();
                if let Some(n) = cached {
                    cache_read_tokens = n;
                }
                if let Some(items) = v["response"]["output"].as_array() {
                    for item in items {
                        remember_encrypted(&mut reasoning_blocks, item);
                    }
                }
            }
            "response.incomplete" => {
                let reason = v["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("unknown");
                bail!("response incomplete: {reason}");
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
        Ok(())
    })
    .await?;

    let tool_calls = order
        .into_iter()
        .filter_map(|item_id| {
            let call = calls.remove(&item_id)?;
            if call.name.is_empty() {
                return None;
            }
            Some(ToolCall {
                id: if call.id.is_empty() { item_id } else { call.id },
                name: call.name,
                arguments: if call.arguments.is_empty() {
                    "{}".into()
                } else {
                    call.arguments
                },
            })
        })
        .collect();

    Ok(StreamReply {
        text,
        reasoning,
        tool_calls,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_create_tokens: 0,
        reasoning_blocks,
        calibrate: true,
    })
}

pub(crate) fn codex_request_body(
    model: &str,
    system: &str,
    history: &[ChatMessage],
    permissions: Option<&PermissionEngine>,
    tool_filter: &ToolFilter,
    extra_tools: &[Value],
    session_id: &str,
) -> Value {
    json!({
        "model": model,
        "instructions": system,
        "input": codex_input(history),
        "tools": responses_tools_json_filtered(permissions, tool_filter, extra_tools),
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": super::prompt_cache_key(session_id),
    })
}

fn codex_input(history: &[ChatMessage]) -> Vec<Value> {
    let mut input = Vec::new();
    for message in history {
        match message.role {
            ChatRole::System => {}
            ChatRole::User => {
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": message.content }],
                }));
            }
            ChatRole::Assistant => {
                push_encrypted_reasoning(&mut input, message);
                if !message.content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": message.content }],
                    }));
                }
                for call in &message.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    }));
                }
            }
            ChatRole::Tool => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id.as_deref().unwrap_or(""),
                    "output": message.content,
                }));
            }
        }
    }
    input
}

fn push_encrypted_reasoning(input: &mut Vec<Value>, message: &ChatMessage) {
    for block in &message.reasoning_blocks {
        let ReasoningBlock::Encrypted { id, content } = block else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        input.push(json!({
            "type": "reasoning",
            "id": id,
            "encrypted_content": content,
            "summary": [],
        }));
    }
}

fn remember_encrypted(blocks: &mut Vec<ReasoningBlock>, item: &Value) {
    if item["type"].as_str() != Some("reasoning") {
        return;
    }
    let Some(content) = item["encrypted_content"].as_str() else {
        return;
    };
    if content.is_empty() {
        return;
    }
    let id = item["id"].as_str().unwrap_or("").to_string();
    if blocks.iter().any(|block| matches!(block, ReasoningBlock::Encrypted { content: have, .. } if have == content))
    {
        return;
    }
    blocks.push(ReasoningBlock::Encrypted {
        id,
        content: content.to_string(),
    });
}

struct CodexCall {
    id: String,
    name: String,
    arguments: String,
}

fn codex_responses_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/responses") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/responses")
    }
}

fn codex_slot<'a>(
    order: &mut Vec<String>,
    calls: &'a mut HashMap<String, CodexCall>,
    item_id: &str,
) -> &'a mut CodexCall {
    if !calls.contains_key(item_id) {
        order.push(item_id.to_string());
        calls.insert(
            item_id.to_string(),
            CodexCall {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            },
        );
    }
    calls.get_mut(item_id).expect("codex slot")
}

fn assign_if_empty(dst: &mut String, src: &str) {
    if dst.is_empty() && !src.is_empty() {
        *dst = src.to_string();
    }
}

/// `replace_args` is the `output_item.done` payload, which carries the full
/// arguments string rather than a delta.
fn remember_codex_call(
    order: &mut Vec<String>,
    calls: &mut HashMap<String, CodexCall>,
    item: &Value,
    replace_args: bool,
) {
    let item_id = item["id"].as_str().unwrap_or("").to_string();
    if item_id.is_empty() {
        return;
    }
    let call_id = item["call_id"].as_str().unwrap_or("");
    let name = item["name"].as_str().unwrap_or("");
    let args = item["arguments"].as_str().unwrap_or("");
    let entry = codex_slot(order, calls, &item_id);
    assign_if_empty(&mut entry.id, call_id);
    assign_if_empty(&mut entry.name, name);
    if replace_args {
        if !args.is_empty() {
            entry.arguments = args.to_string();
        }
    } else if entry.arguments.is_empty() && !args.is_empty() {
        entry.arguments = args.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Credential;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    #[test]
    fn responses_url_joins_once_without_a_double_slash() {
        assert_eq!(
            codex_responses_url("https://chatgpt.com/backend-api/codex/responses"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            codex_responses_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            codex_responses_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/responses"
        );
    }

    async fn spawn_sse(body: &str) -> (String, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let seen_srv = Arc::clone(&seen);
        let body = body.to_string();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            *seen_srv.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        (format!("http://{addr}/v1"), seen)
    }

    fn provider(base_url: String) -> crate::auth::StoredProvider {
        crate::auth::StoredProvider {
            id: "openai-codex".into(),
            name: "Codex".into(),
            protocol: crate::auth::ProviderProtocol::OpenAiCodex,
            base_url,
            credential: Credential::ApiKey { key: "k".into() },
            custom_models: vec![],
            fetched_models: vec![],
            model_routes: Default::default(),
            updated_ms: 0,
        }
    }

    #[tokio::test]
    async fn parallel_tools_keep_output_order_and_empty_args() {
        let body = "\
data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_z\",\"type\":\"function_call\",\"call_id\":\"call_z\",\"name\":\"zeta\",\"arguments\":\"\"}}\n\n\
data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_a\",\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"alpha\",\"arguments\":\"\"}}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n";
        let (base, seen) = spawn_sse(body).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let reply = stream_openai_codex(
            &reqwest::Client::new(),
            &provider(base),
            "gpt",
            "sys",
            &[],
            None,
            &crate::agent::tools::ToolFilter::default(),
            &[],
            "ses",
            std::path::Path::new(""),
            &tx,
        )
        .await
        .unwrap();
        let raw = seen.lock().unwrap().clone();
        assert!(raw.contains("POST /v1/responses "), "{raw}");
        assert!(!raw.contains("/responses/responses"), "{raw}");
        assert_eq!(reply.tool_calls.len(), 2);
        assert_eq!(reply.tool_calls[0].name, "zeta");
        assert_eq!(reply.tool_calls[0].id, "call_z");
        assert_eq!(reply.tool_calls[0].arguments, "{}");
        assert_eq!(reply.tool_calls[1].name, "alpha");
        assert_eq!(reply.input_tokens, 1);
    }

    #[tokio::test]
    async fn response_incomplete_is_an_error() {
        let body = "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n";
        let (base, _seen) = spawn_sse(body).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = stream_openai_codex(
            &reqwest::Client::new(),
            &provider(base),
            "gpt",
            "sys",
            &[],
            None,
            &crate::agent::tools::ToolFilter::default(),
            &[],
            "ses",
            std::path::Path::new(""),
            &tx,
        )
        .await;
        let Err(err) = err else {
            panic!("expected an incomplete response");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("incomplete"), "{msg}");
        assert!(msg.contains("max_output_tokens"), "{msg}");
    }
}
