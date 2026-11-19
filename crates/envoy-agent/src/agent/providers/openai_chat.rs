//! OpenAI Chat Completions streaming.
use super::sse::SseParser;
use super::StreamReply;
use crate::agent::effort;
use crate::agent::events::AgentEvent;
use crate::agent::tools::{openai_tools_json_filtered, ToolFilter};
use crate::auth::{copilot_session_token, Credential, StoredProvider};
use crate::permissions::PermissionEngine;
use crate::session::{ChatMessage, ChatRole, ToolCall};
use anyhow::{bail, Context, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use tokio::sync::mpsc;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_openai_chat(
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
                let mut msg = if m.tool_calls.is_empty() {
                    json!({ "role": "assistant", "content": m.content })
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
                    json!({
                        "role": "assistant",
                        "content": if m.content.is_empty() { Value::Null } else { json!(m.content) },
                        "tool_calls": calls,
                    })
                };
                if provider.id == "deepseek" {
                    if let Some(reasoning) = m.reasoning.as_deref().filter(|text| !text.is_empty())
                    {
                        msg["reasoning_content"] = json!(reasoning);
                    }
                }
                msgs.push(msg);
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

    let mut body = json!({
        "model": model,
        "messages": msgs,
        "tools": openai_tools_json_filtered(permissions, tool_filter, extra_tools),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    effort::apply(&mut body, provider.protocol, &provider.id, model, data_dir);

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
    req = super::apply_opencode_session(req, provider, session_id);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Credential;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn opencode_go_request_sends_the_session_header() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let seen_srv = Arc::clone(&seen);
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            *seen_srv.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let provider = StoredProvider {
            id: "opencode-go".into(),
            name: "OpenCode Go".into(),
            protocol: crate::auth::ProviderProtocol::OpenAiChat,
            base_url: format!("http://{addr}/v1"),
            credential: Credential::ApiKey { key: "k".into() },
            custom_models: vec![],
            model_routes: Default::default(),
            updated_ms: 0,
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let filter = ToolFilter::default();
        let reply = stream_openai_chat(
            &reqwest::Client::new(),
            &provider,
            "glm-5",
            "sys",
            &[],
            None,
            &filter,
            &[],
            "ses_abc",
            std::path::Path::new(""),
            &tx,
        )
        .await
        .unwrap();
        assert_eq!(reply.text, "ok");
        let raw = seen.lock().unwrap().clone();
        assert!(
            raw.to_ascii_lowercase()
                .contains("x-opencode-session: ses_abc"),
            "{raw}"
        );
    }

    #[tokio::test]
    async fn deepseek_request_uses_saved_effort_and_echoes_reasoning() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let seen_srv = Arc::clone(&seen);
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            *seen_srv.lock().unwrap() = read_http_request(&mut sock).await;
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let provider = StoredProvider {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            protocol: crate::auth::ProviderProtocol::OpenAiChat,
            base_url: format!("http://{addr}/v1"),
            credential: Credential::ApiKey { key: "k".into() },
            custom_models: vec![],
            model_routes: Default::default(),
            updated_ms: 0,
        };
        let data = std::env::temp_dir().join(format!(
            "envoy-effort-http-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&data).unwrap();
        effort::store_spec(
            &data,
            "deepseek",
            "deepseek-v3.2",
            crate::agent::context::reasoning::ReasoningSpec {
                efforts: vec!["low".into(), "high".into(), "max".into()],
                toggle: true,
            },
        );
        effort::set(&data, "max").unwrap();
        let history = vec![ChatMessage {
            role: ChatRole::Assistant,
            content: "noted".into(),
            reasoning: Some("earlier".into()),
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
        }];
        let (tx, _rx) = mpsc::unbounded_channel();
        let reply = stream_openai_chat(
            &reqwest::Client::new(),
            &provider,
            "deepseek-v3.2",
            "sys",
            &history,
            None,
            &ToolFilter::default(),
            &[],
            "ses",
            &data,
            &tx,
        )
        .await
        .unwrap();
        assert_eq!(reply.text, "ok");
        let raw = seen.lock().unwrap().clone();
        assert!(raw.contains("\"reasoning_effort\":\"max\""), "{raw}");
        assert!(raw.contains("\"type\":\"enabled\""), "{raw}");
        assert!(raw.contains("\"reasoning_content\":\"earlier\""), "{raw}");
        let _ = std::fs::remove_dir_all(&data);
    }

    async fn read_http_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            let n = sock.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let Some(split) = buf.windows(4).position(|mark| mark == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&buf[..split]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if buf.len() >= split + 4 + length {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }
}
