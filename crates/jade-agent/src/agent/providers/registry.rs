//! Narrow `ModelProvider` trait and `ProviderRegistry` for streaming LLM backends.

use super::{stream_anthropic, stream_openai_chat, stream_openai_codex, StreamReply};
use crate::agent::events::AgentEvent;
use crate::agent::tools::ToolFilter;
use crate::auth::{ProviderProtocol, StoredProvider};
use crate::permissions::PermissionEngine;
use crate::session::ChatMessage;
use anyhow::{bail, Result};
use futures::future::BoxFuture;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Inputs required to execute one streaming model call.
pub struct StreamRequest<'a> {
    pub http: &'a reqwest::Client,
    pub provider: &'a StoredProvider,
    pub model_id: &'a str,
    pub system: &'a str,
    pub messages: &'a [ChatMessage],
    pub permissions: Option<&'a PermissionEngine>,
    pub tool_filter: &'a ToolFilter,
    pub extra_tools: &'a [Value],
    pub max_tokens: u32,
    pub session_id: &'a str,
    pub data_dir: &'a Path,
    pub tx: &'a mpsc::UnboundedSender<AgentEvent>,
}

/// Narrow extension trait for plugging a streaming LLM backend into `jade-agent`.
pub trait ModelProvider: Send + Sync {
    /// Unique adapter key (e.g. `"openai_chat"`, `"anthropic"`, or a custom provider id).
    fn key(&self) -> &str;

    fn stream<'a>(&'a self, req: StreamRequest<'a>) -> BoxFuture<'a, Result<StreamReply>>;
}

/// Registry resolving a [`StoredProvider`] to a [`ModelProvider`] implementation.
///
/// Lookup checks `provider.id` first (allowing per-provider custom adapters), then
/// falls back to `provider.protocol` (`openai_chat`, `openai_codex`, `anthropic`).
#[derive(Clone)]
pub struct ProviderRegistry {
    adapters: BTreeMap<String, Arc<dyn ModelProvider>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("adapters", &self.adapters.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl ProviderRegistry {
    pub fn with_builtins() -> Self {
        let mut reg = Self {
            adapters: BTreeMap::new(),
        };
        reg.register(Arc::new(OpenAiChatAdapter));
        reg.register(Arc::new(OpenAiCodexAdapter));
        reg.register(Arc::new(AnthropicAdapter));
        reg
    }

    pub fn register(&mut self, adapter: Arc<dyn ModelProvider>) -> &mut Self {
        self.adapters.insert(adapter.key().to_string(), adapter);
        self
    }

    pub fn resolve(&self, provider: &StoredProvider) -> Option<Arc<dyn ModelProvider>> {
        if let Some(by_id) = self.adapters.get(&provider.id) {
            return Some(Arc::clone(by_id));
        }
        self.adapters.get(protocol_key(provider.protocol)).cloned()
    }

    pub async fn stream(&self, req: StreamRequest<'_>) -> Result<StreamReply> {
        let Some(adapter) = self.resolve(req.provider) else {
            bail!(
                "no ModelProvider registered for provider `{}` (protocol `{}`)",
                req.provider.id,
                protocol_key(req.provider.protocol)
            );
        };
        adapter.stream(req).await
    }
}

pub fn protocol_key(protocol: ProviderProtocol) -> &'static str {
    match protocol {
        ProviderProtocol::OpenAiChat => "openai_chat",
        ProviderProtocol::OpenAiCodex => "openai_codex",
        ProviderProtocol::Anthropic => "anthropic",
    }
}

struct OpenAiChatAdapter;
impl ModelProvider for OpenAiChatAdapter {
    fn key(&self) -> &str {
        "openai_chat"
    }

    fn stream<'a>(&'a self, req: StreamRequest<'a>) -> BoxFuture<'a, Result<StreamReply>> {
        Box::pin(async move {
            stream_openai_chat(
                req.http,
                req.provider,
                req.model_id,
                req.system,
                req.messages,
                req.permissions,
                req.tool_filter,
                req.extra_tools,
                req.session_id,
                req.data_dir,
                req.tx,
            )
            .await
        })
    }
}

struct OpenAiCodexAdapter;
impl ModelProvider for OpenAiCodexAdapter {
    fn key(&self) -> &str {
        "openai_codex"
    }

    fn stream<'a>(&'a self, req: StreamRequest<'a>) -> BoxFuture<'a, Result<StreamReply>> {
        Box::pin(async move {
            stream_openai_codex(
                req.http,
                req.provider,
                req.model_id,
                req.system,
                req.messages,
                req.permissions,
                req.tool_filter,
                req.extra_tools,
                req.session_id,
                req.data_dir,
                req.tx,
            )
            .await
        })
    }
}

struct AnthropicAdapter;
impl ModelProvider for AnthropicAdapter {
    fn key(&self) -> &str {
        "anthropic"
    }

    fn stream<'a>(&'a self, req: StreamRequest<'a>) -> BoxFuture<'a, Result<StreamReply>> {
        Box::pin(async move {
            stream_anthropic(
                req.http,
                req.provider,
                req.model_id,
                req.system,
                req.messages,
                req.permissions,
                req.tool_filter,
                req.extra_tools,
                req.max_tokens,
                req.session_id,
                req.data_dir,
                req.tx,
            )
            .await
        })
    }
}
