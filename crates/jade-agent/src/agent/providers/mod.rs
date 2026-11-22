//! Model provider adapters.
use crate::auth::StoredProvider;
use crate::session::ToolCall;
use anyhow::{bail, Context, Result};
use futures::StreamExt;
use sse::{SseEvent, SseParser};
use std::sync::OnceLock;
use std::time::Duration;

mod anthropic;
mod anthropic_body;
mod openai_chat;
mod openai_chat_body;
mod openai_codex;
pub mod registry;
mod sse;

pub use registry::{ModelProvider, ProviderRegistry, StreamRequest};

#[cfg(test)]
pub(crate) use anthropic_body::{anthropic_messages, mark_last_tool, system_blocks};
#[cfg(test)]
pub(crate) use openai_chat_body::{chat_body, chat_messages};
#[cfg(test)]
pub(crate) use openai_codex::codex_request_body;

const MAX_HTTP_ATTEMPTS: u32 = 3;
const STREAM_IDLE: Duration = Duration::from_secs(120);

/// Process-wide client for provider calls. No total timeout: a turn stream
/// can run longer than any fixed budget. Chunk idle is enforced separately.
pub(crate) fn shared_http_client() -> Result<reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(8)
        .build()
        .context("building shared HTTP client")?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 502 | 503 | 529)
}

fn transient_connect(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout()
}

fn retry_delay(headers: &reqwest::header::HeaderMap, attempt: u32) -> Duration {
    if let Some(raw) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(secs) = raw.trim().parse::<u64>() {
            return Duration::from_secs(secs.min(30));
        }
    }
    let shift = attempt.saturating_sub(1).min(4);
    Duration::from_millis(200u64.saturating_mul(1u64 << shift)).min(Duration::from_secs(2))
}

/// Retry idempotent provider POSTs that fail before any streamed output.
pub(crate) async fn send_with_retry(
    req: reqwest::RequestBuilder,
    body: &serde_json::Value,
    what: &'static str,
) -> Result<reqwest::Response> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let Some(builder) = req.try_clone() else {
            return req.json(body).send().await.context(what);
        };
        match builder.json(body).send().await {
            Ok(resp) if retryable_status(resp.status()) && attempt < MAX_HTTP_ATTEMPTS => {
                let delay = retry_delay(resp.headers(), attempt);
                drop(resp);
                tokio::time::sleep(delay).await;
            }
            Ok(resp) => return Ok(resp),
            Err(err) if transient_connect(&err) && attempt < MAX_HTTP_ATTEMPTS => {
                let delay = retry_delay(&reqwest::header::HeaderMap::new(), attempt);
                tokio::time::sleep(delay).await;
            }
            Err(err) => return Err(err).context(what),
        }
    }
}

/// Read an SSE body, including a tail event that never got a blank line.
pub(crate) async fn for_each_sse<S, B, F>(stream: S, mut on_event: F) -> Result<()>
where
    S: futures::Stream<Item = Result<B, reqwest::Error>>,
    B: AsRef<[u8]>,
    F: FnMut(SseEvent) -> Result<()>,
{
    let mut stream = std::pin::pin!(stream);
    let mut parser = SseParser::default();
    loop {
        match tokio::time::timeout(STREAM_IDLE, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                for ev in parser.feed(chunk.as_ref()) {
                    on_event(ev)?;
                }
            }
            Ok(Some(Err(err))) => return Err(err).context("reading provider stream"),
            Ok(None) => break,
            Err(_) => bail!("provider stream idle for 120s"),
        }
    }
    for ev in parser.finish() {
        on_event(ev)?;
    }
    Ok(())
}

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
        format!("jade-{provider_id}")
    } else {
        clean
    }
}

/// Sanitize a declarative header value, substituting `${SESSION_ID}` and
/// stripping CR/LF/control bytes to prevent header injection.
pub(crate) fn render_header_value(raw: &str, provider_id: &str, session_id: &str) -> String {
    let sess = opencode_session_value(provider_id, session_id);
    raw.replace("${SESSION_ID}", &sess)
        .chars()
        .filter(|c| *c != '\r' && *c != '\n' && *c != '\0' && (!c.is_control() || *c == '\t'))
        .collect()
}

/// Apply provider-declared `extra_headers` plus compatibility defaults (such as
/// `x-opencode-session` for OpenCode gateways unless explicitly overridden).
pub(crate) fn apply_extra_headers(
    mut req: reqwest::RequestBuilder,
    provider: &StoredProvider,
    session_id: &str,
) -> reqwest::RequestBuilder {
    let mut has_opencode_session = false;
    for (name, raw_val) in &provider.extra_headers {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty()
            || trimmed_name
                .bytes()
                .any(|b| b <= 0x20 || b >= 0x7f || b == b':')
        {
            continue;
        }
        if trimmed_name.eq_ignore_ascii_case(OPENCODE_SESSION) {
            has_opencode_session = true;
        }
        let val = render_header_value(raw_val, &provider.id, session_id);
        if !val.is_empty() {
            req = req.header(trimmed_name, val);
        }
    }
    if !has_opencode_session && requires_opencode_session(&provider.id, &provider.base_url) {
        req = req.header(
            OPENCODE_SESSION,
            opencode_session_value(&provider.id, session_id),
        );
    }
    req
}

#[derive(Debug, Default)]
pub struct StreamReply {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_create_tokens: u64,
    pub reasoning_blocks: Vec<crate::session::ReasoningBlock>,
    /// False when this reply followed overflow compaction, so its token
    /// count must not move [`crate::agent::context::TokenCalibration`].
    pub calibrate: bool,
}

pub(crate) fn prompt_cache_key(session_id: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in session_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("jade-{hash:016x}")
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
    fn session_value_keeps_jade_ids_and_fills_an_empty_one() {
        assert_eq!(
            opencode_session_value("opencode-go", "ses_abc_def"),
            "ses_abc_def"
        );
        assert_eq!(opencode_session_value("opencode-go", "bad\nid"), "badid");
        assert_eq!(
            opencode_session_value("opencode-go", "  "),
            "jade-opencode-go"
        );
    }

    #[test]
    fn render_header_value_substitutes_session_and_strips_crlf() {
        assert_eq!(
            render_header_value("trace-${SESSION_ID}", "corp", "ses_42"),
            "trace-ses_42"
        );
        assert_eq!(
            render_header_value("good\r\nX-Injected: evil", "corp", "ses_42"),
            "goodX-Injected: evil"
        );
    }

    #[test]
    fn shared_http_client_builds_twice() {
        let first = shared_http_client().unwrap();
        let second = shared_http_client().unwrap();
        drop((first, second));
    }

    #[tokio::test]
    async fn send_with_retry_retries_503_before_success() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for status in [503u16, 200] {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let (code, text) = if status == 503 {
                    ("503 Service Unavailable", "busy")
                } else {
                    ("200 OK", "ok")
                };
                let resp = format!(
                    "HTTP/1.1 {code}\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, resp.as_bytes()).await;
            }
        });
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let resp = send_with_retry(
            http.post(format!("http://{addr}/v1/chat")),
            &serde_json::json!({"n": 1}),
            "calling LLM API",
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
    }
}
