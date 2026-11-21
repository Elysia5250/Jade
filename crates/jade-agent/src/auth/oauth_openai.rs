//! ChatGPT Plus/Pro browser and device-code login.
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::pkce::{extract_chatgpt_account_id, extract_query_param, form_encode, Pkce};
use super::{
    now_ms, Credential, ProviderProtocol, StoredProvider, OPENAI_AUTH_BASE, OPENAI_CLIENT_ID,
    OPENAI_CODEX_ENDPOINT,
};

pub fn openai_authorize_url(redirect_uri: &str, pkce: &Pkce, state: &str) -> String {
    let q = form_encode(&[
        ("response_type", "code"),
        ("client_id", OPENAI_CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", "jade"),
    ]);
    format!("{OPENAI_AUTH_BASE}/oauth/authorize?{q}")
}

pub async fn bind_openai_callback_listener() -> Result<(TcpListener, u16)> {
    for port in [1455u16, 1457u16] {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            return Ok((listener, port));
        }
    }
    // Fall back to ephemeral port if 1455/1457 are occupied; user can still paste URL.
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("binding local OAuth callback listener")?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

pub async fn wait_openai_browser_callback(
    listener: TcpListener,
    expected_state: String,
    mut cancel_rx: oneshot::Receiver<()>,
) -> Result<String> {
    loop {
        let accepted = tokio::select! {
            _ = &mut cancel_rx => bail!("OAuth login cancelled"),
            res = listener.accept() => res?,
        };
        let (mut stream, _) = accepted;
        let request_line = match tokio::time::timeout(
            Duration::from_secs(5),
            read_request_line(&mut stream),
        )
        .await
        {
            Ok(Ok(line)) if !line.is_empty() => line,
            _ => continue,
        };
        let target = request_line.split_whitespace().nth(1).unwrap_or("/");
        if !target.starts_with("/auth/callback") {
            let resp =
                "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot found";
            let _ = stream.write_all(resp.as_bytes()).await;
            continue;
        }
        if let Some(err) = extract_query_param(target, "error_description")
            .or_else(|| extract_query_param(target, "error"))
        {
            let body = oauth_error_page(&err);
            let _ = stream
                .write_all(html_response(400, "Bad Request", &body).as_bytes())
                .await;
            bail!("OpenAI authorization error: {err}");
        }
        let code = extract_query_param(target, "code");
        let state = extract_query_param(target, "state");
        if code.is_none() || state.as_deref() != Some(expected_state.as_str()) {
            let body = "<html><body><h2>Invalid OAuth state or missing code</h2></body></html>";
            let resp = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            continue;
        }
        let body = "<html><body style='font-family:sans-serif;padding:2rem'><h2>Connected to Jade</h2><p>Authorization complete. You can close this tab and return to your terminal.</p></body></html>";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        return Ok(code.unwrap());
    }
}

pub async fn exchange_openai_code(
    http: &reqwest::Client,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
    method_id: &str,
) -> Result<StoredProvider> {
    let body = form_encode(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", OPENAI_CLIENT_ID),
        ("code_verifier", code_verifier),
    ]);
    let resp = http
        .post(format!("{OPENAI_AUTH_BASE}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("exchanging OpenAI OAuth code")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("OpenAI token exchange failed ({status}): {text}");
    }
    let tok: serde_json::Value = serde_json::from_str(&text).context("parsing OpenAI token")?;
    let access_token = tok["access_token"]
        .as_str()
        .context("missing access_token")?
        .to_string();
    let refresh_token = tok["refresh_token"].as_str().map(str::to_string);
    let expires_in = tok["expires_in"].as_u64().unwrap_or(3600);
    let account_id = tok["id_token"]
        .as_str()
        .and_then(extract_chatgpt_account_id)
        .or_else(|| extract_chatgpt_account_id(&access_token));

    Ok(StoredProvider {
        id: "openai".to_string(),
        name: "OpenAI / ChatGPT".to_string(),
        protocol: ProviderProtocol::OpenAiCodex,
        base_url: OPENAI_CODEX_ENDPOINT.to_string(),
        credential: Credential::OAuth {
            access_token,
            refresh_token,
            expires_at_ms: Some(now_ms() + expires_in * 1000),
            method_id: method_id.to_string(),
            account_id,
        },
        custom_models: Vec::new(),
        fetched_models: Vec::new(),
        model_routes: BTreeMap::new(),
        updated_ms: now_ms(),
    })
}

#[derive(Debug, Clone)]
pub struct OpenAiDeviceStart {
    pub device_auth_id: String,
    pub user_code: String,
    pub verification_url: String,
    pub interval_s: u64,
}

pub async fn start_openai_device_auth(http: &reqwest::Client) -> Result<OpenAiDeviceStart> {
    let resp = http
        .post(format!(
            "{OPENAI_AUTH_BASE}/api/accounts/deviceauth/usercode"
        ))
        .json(&serde_json::json!({ "client_id": OPENAI_CLIENT_ID }))
        .send()
        .await
        .context("starting OpenAI device authorization")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("OpenAI device auth start failed ({status}): {text}");
    }
    let val: serde_json::Value = serde_json::from_str(&text)?;
    let device_auth_id = val["device_auth_id"]
        .as_str()
        .context("missing device_auth_id")?
        .to_string();
    let user_code = val["user_code"]
        .as_str()
        .context("missing user_code")?
        .to_string();
    let interval_s = val["interval"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| val["interval"].as_u64())
        .unwrap_or(5)
        .max(1);
    Ok(OpenAiDeviceStart {
        device_auth_id,
        user_code,
        verification_url: format!("{OPENAI_AUTH_BASE}/codex/device"),
        interval_s,
    })
}

const DEVICE_POLL_LIMIT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollClass {
    Ready,
    Pending,
    Fatal,
}

/// 2xx is ready. 403/404 stays pending only for `authorization_pending`,
/// `slow_down`, or a body that does not name another error. Anything else
/// fails the poll immediately.
fn classify_poll(status: u16, body: &str) -> PollClass {
    if (200..300).contains(&status) {
        return PollClass::Ready;
    }
    if status == 403 || status == 404 {
        let error = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|val| {
                val.get("error")
                    .and_then(|item| item.as_str())
                    .map(str::to_string)
            });
        return match error.as_deref() {
            None | Some("authorization_pending") | Some("slow_down") => PollClass::Pending,
            Some(_) => PollClass::Fatal,
        };
    }
    PollClass::Fatal
}

pub async fn poll_openai_device_auth(
    http: &reqwest::Client,
    start: &OpenAiDeviceStart,
) -> Result<StoredProvider> {
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let result = poll_openai_device_until(http, start, cancel_rx, DEVICE_POLL_LIMIT).await;
    drop(cancel_tx);
    result
}

pub async fn poll_openai_device_until(
    http: &reqwest::Client,
    start: &OpenAiDeviceStart,
    mut cancel_rx: oneshot::Receiver<()>,
    limit: Duration,
) -> Result<StoredProvider> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if tokio::time::Instant::now() >= deadline {
            bail!("OpenAI device authorization timed out");
        }
        let resp = http
            .post(format!("{OPENAI_AUTH_BASE}/api/accounts/deviceauth/token"))
            .json(&serde_json::json!({
                "device_auth_id": start.device_auth_id,
                "user_code": start.user_code,
            }))
            .send()
            .await
            .context("polling OpenAI device authorization")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        match classify_poll(status.as_u16(), &text) {
            PollClass::Ready => {
                let val: serde_json::Value =
                    serde_json::from_str(&text).context("parsing OpenAI device token")?;
                let code = val["authorization_code"]
                    .as_str()
                    .context("missing authorization_code")?;
                let verifier = val["code_verifier"]
                    .as_str()
                    .context("missing code_verifier")?;
                return exchange_openai_code(
                    http,
                    code,
                    &format!("{OPENAI_AUTH_BASE}/deviceauth/callback"),
                    verifier,
                    "chatgpt-device",
                )
                .await;
            }
            PollClass::Fatal => bail!("OpenAI device auth failed ({status}): {text}"),
            PollClass::Pending => {}
        }
        let wait = Duration::from_secs(start.interval_s.saturating_add(2));
        tokio::select! {
            _ = &mut cancel_rx => bail!("OpenAI device authorization cancelled"),
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

async fn read_request_line(stream: &mut tokio::net::TcpStream) -> std::io::Result<String> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(String::new());
    }
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    Ok(request_line)
}

fn html_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn oauth_error_page(err: &str) -> String {
    format!(
        "<html><body><h2>Authorization failed</h2><p>{}</p></body></html>",
        html_escape(err)
    )
}

fn html_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

pub async fn exchange_openai_code_default(
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
    method_id: &str,
) -> Result<StoredProvider> {
    let http = crate::agent::shared_http_client()?;
    exchange_openai_code(&http, code, redirect_uri, code_verifier, method_id).await
}

pub async fn start_openai_device_auth_default() -> Result<OpenAiDeviceStart> {
    let http = crate::agent::shared_http_client()?;
    start_openai_device_auth(&http).await
}

pub async fn poll_openai_device_auth_default(start: &OpenAiDeviceStart) -> Result<StoredProvider> {
    let http = crate::agent::shared_http_client()?;
    poll_openai_device_auth(&http, start).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_error_page_escapes_script_tags() {
        let body = oauth_error_page("<script>alert(1)</script>");
        assert!(body.contains("&lt;script&gt;"));
        assert!(!body.contains("<script>"));
        let page = html_response(400, "Bad Request", &body);
        let length = page
            .split("Content-Length: ")
            .nth(1)
            .and_then(|rest| rest.split("\r\n").next())
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap();
        assert_eq!(length, body.len());
    }

    #[test]
    fn device_poll_classifies_pending_and_fatal_bodies() {
        assert_eq!(classify_poll(200, "{}"), PollClass::Ready);
        assert_eq!(classify_poll(403, ""), PollClass::Pending);
        assert_eq!(
            classify_poll(404, r#"{"error":"authorization_pending"}"#),
            PollClass::Pending
        );
        assert_eq!(
            classify_poll(403, r#"{"error":"slow_down"}"#),
            PollClass::Pending
        );
        assert_eq!(
            classify_poll(403, r#"{"error":"access_denied"}"#),
            PollClass::Fatal
        );
        assert_eq!(
            classify_poll(404, r#"{"error":"expired_token"}"#),
            PollClass::Fatal
        );
        assert_eq!(classify_poll(500, "nope"), PollClass::Fatal);
    }
}
