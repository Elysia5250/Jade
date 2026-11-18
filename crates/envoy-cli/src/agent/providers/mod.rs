//! Model provider adapters.
use crate::auth::StoredProvider;
use crate::session::ToolCall;

mod anthropic;
mod openai_chat;
mod openai_codex;
mod sse;

pub(crate) use anthropic::{anthropic_max_tokens, stream_anthropic};
pub(crate) use openai_chat::stream_openai_chat;
pub(crate) use openai_codex::stream_openai_codex;

/// OpenCode Go rejects inference calls that omit this header.
const OPENCODE_SESSION: &str = "x-opencode-session";

pub(crate) fn requires_opencode_session(provider_id: &str, base_url: &str) -> bool {
    matches!(provider_id, "opencode-zen" | "opencode-go") || opencode_host(base_url)
}

fn opencode_host(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    host == "opencode.ai" || host.ends_with(".opencode.ai")
}

/// Stable per-conversation value. Empty ids still send a provider-scoped
/// fallback so the gateway does not answer 400 MissingSessionID.
pub(crate) fn opencode_session_value(provider_id: &str, session_id: &str) -> String {
    let clean: String = session_id
        .chars()
        .filter(|c| c.is_ascii_graphic() && *c != '"' && *c != '\\')
        .take(128)
        .collect();
    if clean.is_empty() {
        format!("envoy-{provider_id}")
    } else {
        clean
    }
}

pub(crate) fn apply_opencode_session(
    req: reqwest::RequestBuilder,
    provider: &StoredProvider,
    session_id: &str,
) -> reqwest::RequestBuilder {
    if !requires_opencode_session(&provider.id, &provider.base_url) {
        return req;
    }
    req.header(
        OPENCODE_SESSION,
        opencode_session_value(&provider.id, session_id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_header_is_required_for_opencode_ids_and_hosts_only() {
        assert!(requires_opencode_session(
            "opencode-go",
            "http://127.0.0.1:9/v1"
        ));
        assert!(requires_opencode_session(
            "opencode-zen",
            "https://example.com/v1"
        ));
        assert!(requires_opencode_session(
            "custom",
            "https://opencode.ai/zen/go/v1"
        ));
        assert!(requires_opencode_session(
            "custom",
            "https://inference.opencode.ai/v1"
        ));
        assert!(!requires_opencode_session(
            "deepseek",
            "https://api.deepseek.com/v1"
        ));
        assert!(!requires_opencode_session(
            "custom",
            "https://opencode.ai.evil.example/v1"
        ));
    }

    #[test]
    fn session_value_keeps_envoy_ids_and_fills_an_empty_one() {
        assert_eq!(
            opencode_session_value("opencode-go", "ses_abc_def"),
            "ses_abc_def"
        );
        assert_eq!(
            opencode_session_value("opencode-go", "bad\nid"),
            "badid"
        );
        assert_eq!(
            opencode_session_value("opencode-go", "  "),
            "envoy-opencode-go"
        );
    }
}

pub(super) struct StreamReply {
    pub(super) text: String,
    pub(super) reasoning: String,
    pub(super) tool_calls: Vec<ToolCall>,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
}
