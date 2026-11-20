//! OpenAI Chat Completions streaming.
use super::StreamReply;
use crate::agent::effort;
use crate::agent::events::AgentEvent;
use crate::agent::tools::ToolFilter;
use crate::auth::{copilot_session_token, Credential, StoredProvider};
use crate::permissions::PermissionEngine;
use crate::session::{ChatMessage, ToolCall};
use anyhow::{bail, Result};
use serde_json::Value;
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
    let mut body = super::openai_chat_body::chat_body(
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

    let resp = post_chat_retrying_stream_options(req, &mut body).await?;

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut partial_calls: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
    let mut next_auto = 0usize;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_read_tokens = 0u64;

    super::for_each_sse(resp.bytes_stream(), |ev| {
        let data = ev.data.trim();
        if data == "[DONE]" || data.is_empty() {
            return Ok(());
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return Ok(());
        };
        if let Some(message) = openai_stream_error(&v) {
            bail!("{message}");
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            input_tokens = u["prompt_tokens"].as_u64().unwrap_or(input_tokens);
            output_tokens = u["completion_tokens"].as_u64().unwrap_or(output_tokens);
            let cached = super::openai_chat_body::cached_tokens(u);
            if cached > 0 {
                cache_read_tokens = cached;
            }
        }
        let Some(choice) = v["choices"].as_array().and_then(|c| c.first()) else {
            return Ok(());
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
                remember_tool_delta(&mut partial_calls, &mut next_auto, call);
            }
        }
        Ok(())
    })
    .await?;

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
            arguments: if arguments.is_empty() {
                "{}".into()
            } else {
                arguments
            },
        })
        .filter(|tc| !tc.name.is_empty())
        .collect();

    Ok(StreamReply {
        text,
        reasoning,
        tool_calls,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_create_tokens: 0,
        reasoning_blocks: Vec::new(),
        calibrate: true,
    })
}

/// Gateways that reject `stream_options` get one retry without the field.
/// That retry happens before any token is forwarded.
async fn post_chat_retrying_stream_options(
    req: reqwest::RequestBuilder,
    body: &mut Value,
) -> Result<reqwest::Response> {
    let mut stripped = false;
    loop {
        let Some(attempt) = req.try_clone() else {
            bail!("could not clone OpenAI request");
        };
        let resp = super::send_with_retry(attempt, body, "calling LLM API").await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let err_body = resp.text().await.unwrap_or_default();
        let rejected = status.as_u16() == 400
            && !stripped
            && body.get("stream_options").is_some()
            && err_body.to_ascii_lowercase().contains("stream_options");
        if rejected {
            body.as_object_mut().map(|map| map.remove("stream_options"));
            stripped = true;
            continue;
        }
        bail!("LLM API error ({status}): {err_body}");
    }
}

fn openai_stream_error(v: &Value) -> Option<String> {
    match v.get("error")? {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        Value::Object(map) if map.is_empty() => None,
        other => Some(
            other
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("OpenAI stream error")
                .to_string(),
        ),
    }
}

/// Missing `index` allocates the next slot. An empty `id` never wipes one we
/// already stored. Later argument deltas without an id append to the last slot.
fn remember_tool_delta(
    calls: &mut BTreeMap<usize, (String, String, String)>,
    next_auto: &mut usize,
    call: &Value,
) {
    let idx = if let Some(i) = call.get("index").and_then(Value::as_u64) {
        let idx = i as usize;
        *next_auto = (*next_auto).max(idx.saturating_add(1));
        idx
    } else {
        let new_id = call
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty());
        if new_id || calls.is_empty() {
            let idx = *next_auto;
            *next_auto = idx.saturating_add(1);
            idx
        } else {
            calls.keys().next_back().copied().unwrap_or(0)
        }
    };
    let entry = calls.entry(idx).or_default();
    if let Some(id) = call.get("id").and_then(Value::as_str) {
        if !id.is_empty() && entry.0.is_empty() {
            entry.0 = id.to_string();
        }
    }
    if let Some(name) = call["function"]["name"].as_str() {
        entry.1.push_str(name);
    }
    if let Some(args) = call["function"]["arguments"].as_str() {
        entry.2.push_str(args);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Credential;
    use crate::session::ChatRole;
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
            fetched_models: vec![],
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
            fetched_models: vec![],
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

            reasoning_blocks: Vec::new(),
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
        assert!(!raw.contains("\"type\":\"enabled\""), "{raw}");
        assert!(raw.contains("\"reasoning_content\":\"earlier\""), "{raw}");
        assert!(raw.contains("\"prompt_cache_key\""), "{raw}");
        let _ = std::fs::remove_dir_all(&data);
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
            *seen_srv.lock().unwrap() = read_http_request(&mut sock).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        (format!("http://{addr}/v1"), seen)
    }

    fn chat_provider(base_url: String) -> StoredProvider {
        StoredProvider {
            id: "openai".into(),
            name: "OpenAI".into(),
            protocol: crate::auth::ProviderProtocol::OpenAiChat,
            base_url,
            credential: Credential::ApiKey { key: "k".into() },
            custom_models: vec![],
            fetched_models: vec![],
            model_routes: Default::default(),
            updated_ms: 0,
        }
    }

    #[tokio::test]
    async fn sse_error_object_fails_the_turn() {
        let body = "data: {\"error\":{\"message\":\"overloaded\",\"type\":\"server_error\"}}\n\n";
        let (base, _seen) = spawn_sse(body).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = stream_openai_chat(
            &reqwest::Client::new(),
            &chat_provider(base),
            "gpt",
            "sys",
            &[],
            None,
            &ToolFilter::default(),
            &[],
            "ses",
            std::path::Path::new(""),
            &tx,
        )
        .await;
        let Err(err) = err else {
            panic!("expected an SSE error");
        };
        assert!(format!("{err:#}").contains("overloaded"), "{err:#}");
    }

    #[tokio::test]
    async fn parallel_tools_keep_appearance_order_and_empty_args() {
        let body = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_z\",\"function\":{\"name\":\"zeta\",\"arguments\":\"\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_a\",\"function\":{\"name\":\"alpha\",\"arguments\":\"\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"\",\"function\":{\"arguments\":\"{\\\"x\\\":1}\"}}]}}]}\n\n\
data: [DONE]\n\n";
        let (base, _seen) = spawn_sse(body).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let reply = stream_openai_chat(
            &reqwest::Client::new(),
            &chat_provider(base),
            "gpt",
            "sys",
            &[],
            None,
            &ToolFilter::default(),
            &[],
            "ses",
            std::path::Path::new(""),
            &tx,
        )
        .await
        .unwrap();
        assert_eq!(reply.tool_calls.len(), 2);
        assert_eq!(reply.tool_calls[0].name, "zeta");
        assert_eq!(reply.tool_calls[0].arguments, "{}");
        assert_eq!(reply.tool_calls[1].id, "call_a");
        assert_eq!(reply.tool_calls[1].name, "alpha");
        assert_eq!(reply.tool_calls[1].arguments, "{\"x\":1}");
    }

    #[tokio::test]
    async fn finish_reads_a_tail_event_without_a_blank_line() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"尾\"}}]}\n";
        let (base, _seen) = spawn_sse(body).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let reply = stream_openai_chat(
            &reqwest::Client::new(),
            &chat_provider(base),
            "gpt",
            "sys",
            &[],
            None,
            &ToolFilter::default(),
            &[],
            "ses",
            std::path::Path::new(""),
            &tx,
        )
        .await
        .unwrap();
        assert_eq!(reply.text, "尾");
    }

    #[tokio::test]
    async fn stream_options_rejection_is_retried_without_the_field() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_srv = Arc::clone(&seen);
        tokio::spawn(async move {
            for turn in 0..2 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let raw = read_http_request(&mut sock).await;
                seen_srv.lock().unwrap().push(raw);
                if turn == 0 {
                    let msg = "{\"error\":\"unknown field stream_options\"}";
                    let resp = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{msg}",
                        msg.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                } else {
                    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            }
        });
        let (tx, _rx) = mpsc::unbounded_channel();
        let reply = stream_openai_chat(
            &reqwest::Client::new(),
            &chat_provider(format!("http://{addr}/v1")),
            "gpt",
            "sys",
            &[],
            None,
            &ToolFilter::default(),
            &[],
            "ses",
            std::path::Path::new(""),
            &tx,
        )
        .await
        .unwrap();
        assert_eq!(reply.text, "ok");
        let raw = seen.lock().unwrap().clone();
        assert!(raw[0].contains("stream_options"), "{}", raw[0]);
        assert!(!raw[1].contains("stream_options"), "{}", raw[1]);
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
