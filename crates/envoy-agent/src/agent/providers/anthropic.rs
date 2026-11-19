//! Anthropic Messages streaming.
use super::StreamReply;
use crate::agent::effort;
use crate::agent::events::AgentEvent;
use crate::agent::tools::{anthropic_tools_json_filtered, ToolFilter};
use crate::auth::{Credential, StoredProvider};
use crate::permissions::PermissionEngine;
use crate::session::{ChatMessage, ReasoningBlock, ToolCall};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use tokio::sync::mpsc;

pub(crate) fn anthropic_max_tokens(max_output: Option<u64>) -> u32 {
    match max_output {
        Some(n) if n >= 256 => n.min(128_000) as u32,
        _ => 8192,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_anthropic(
    http: &reqwest::Client,
    provider: &StoredProvider,
    model: &str,
    system: &str,
    history: &[ChatMessage],
    permissions: Option<&PermissionEngine>,
    tool_filter: &ToolFilter,
    extra_tools: &[Value],
    max_tokens: u32,
    session_id: &str,
    data_dir: &Path,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    let url = format!("{}/messages", provider.base_url.trim_end_matches('/'));
    let msgs = super::anthropic_body::anthropic_messages(history);
    let mut tools = anthropic_tools_json_filtered(permissions, tool_filter, extra_tools);
    super::anthropic_body::mark_last_tool(&mut tools);

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens.max(256),
        "system": super::anthropic_body::system_blocks(system),
        "messages": msgs,
        "tools": tools,
        "stream": true,
    });
    effort::apply(&mut body, provider.protocol, &provider.id, model, data_dir);

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
            req = req.bearer_auth(access_token).header(
                "anthropic-beta",
                "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14",
            );
        }
    }
    req = super::apply_opencode_session(req, provider, session_id);

    let resp = super::send_with_retry(req, &body, "calling Anthropic API").await?;
    let status = resp.status();
    if !status.is_success() {
        let err_body = resp.text().await.unwrap_or_default();
        bail!("Anthropic API error ({status}): {err_body}");
    }

    let mut acc = AnthropicAcc::default();
    super::for_each_sse(resp.bytes_stream(), |ev| {
        let data = ev.data.trim();
        if data.is_empty() {
            return Ok(());
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return Ok(());
        };
        acc.on_event(&v, tx)?;
        Ok(())
    })
    .await?;
    Ok(acc.into_reply())
}

#[derive(Default)]
struct AnthropicAcc {
    text: String,
    reasoning: String,
    tools: BTreeMap<usize, (String, String, String)>,
    thinking: BTreeMap<usize, (String, String)>,
    redacted: Vec<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_create_tokens: u64,
}

impl AnthropicAcc {
    fn on_event(&mut self, v: &Value, tx: &mpsc::UnboundedSender<AgentEvent>) -> Result<()> {
        match v["type"].as_str().unwrap_or("") {
            "message_start" => self.read_usage(&v["message"]["usage"], true),
            "message_delta" => self.read_usage(&v["usage"], false),
            "content_block_start" => self.on_block_start(v),
            "content_block_delta" => self.on_delta(v, tx),
            "error" => {
                let msg = v["error"]["message"]
                    .as_str()
                    .unwrap_or("Anthropic stream error");
                bail!("{msg}");
            }
            _ => {}
        }
        Ok(())
    }

    fn read_usage(&mut self, usage: &Value, start: bool) {
        if start {
            self.input_tokens = usage["input_tokens"].as_u64().unwrap_or(self.input_tokens);
        } else {
            self.output_tokens = usage["output_tokens"].as_u64().unwrap_or(self.output_tokens);
        }
        if let Some(n) = usage["cache_read_input_tokens"].as_u64() {
            self.cache_read_tokens = n;
        }
        if let Some(n) = usage["cache_creation_input_tokens"].as_u64() {
            self.cache_create_tokens = n;
        }
    }

    fn on_block_start(&mut self, v: &Value) {
        let idx = v["index"].as_u64().unwrap_or(0) as usize;
        let block = &v["content_block"];
        match block["type"].as_str().unwrap_or("") {
            "tool_use" => {
                let id = block["id"].as_str().unwrap_or("").to_string();
                let name = block["name"].as_str().unwrap_or("").to_string();
                self.tools.insert(idx, (id, name, String::new()));
            }
            "thinking" => {
                let signature = block["signature"].as_str().unwrap_or("").to_string();
                self.thinking.insert(idx, (String::new(), signature));
            }
            "redacted_thinking" => {
                if let Some(data) = block["data"].as_str() {
                    if !data.is_empty() {
                        self.redacted.push(data.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    fn on_delta(&mut self, v: &Value, tx: &mpsc::UnboundedSender<AgentEvent>) {
        let idx = v["index"].as_u64().unwrap_or(0) as usize;
        let delta = &v["delta"];
        match delta["type"].as_str().unwrap_or("") {
            "text_delta" => {
                if let Some(t) = delta["text"].as_str() {
                    self.text.push_str(t);
                    let _ = tx.send(AgentEvent::TextDelta(t.to_string()));
                }
            }
            "thinking_delta" => {
                if let Some(t) = delta["thinking"].as_str() {
                    self.reasoning.push_str(t);
                    self.thinking.entry(idx).or_default().0.push_str(t);
                    let _ = tx.send(AgentEvent::ReasoningDelta(t.to_string()));
                }
            }
            "signature_delta" => {
                if let Some(sig) = delta["signature"].as_str() {
                    self.thinking.entry(idx).or_default().1.push_str(sig);
                }
            }
            "input_json_delta" => {
                if let Some(j) = delta["partial_json"].as_str() {
                    if let Some(entry) = self.tools.get_mut(&idx) {
                        entry.2.push_str(j);
                    }
                }
            }
            _ => {}
        }
    }

    fn into_reply(self) -> StreamReply {
        let tool_calls = self
            .tools
            .into_values()
            .map(|(id, name, args)| ToolCall {
                id,
                name,
                arguments: if args.is_empty() { "{}".into() } else { args },
            })
            .filter(|tc| !tc.name.is_empty())
            .collect();
        let mut reasoning_blocks = Vec::new();
        for (_idx, (text, signature)) in self.thinking {
            if text.is_empty() && signature.is_empty() {
                continue;
            }
            reasoning_blocks.push(ReasoningBlock::Thinking { text, signature });
        }
        for data in self.redacted {
            reasoning_blocks.push(ReasoningBlock::Redacted { data });
        }
        StreamReply {
            text: self.text,
            reasoning: self.reasoning,
            tool_calls,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_create_tokens: self.cache_create_tokens,
            reasoning_blocks,
            calibrate: true,
        }
    }
}
