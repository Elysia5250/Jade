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
        ("originator", "envoy"),
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
        let mut reader = BufReader::new(&mut stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
            continue;
        }
        // Drain HTTP headers
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" || line == "\n"
            {
                break;
            }
        }
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
            let body =
                format!("<html><body><h2>Authorization failed</h2><p>{err}</p></body></html>");
            let resp = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
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
        let body = "<html><body style='font-family:sans-serif;padding:2rem'><h2>Connected to Envoy</h2><p>Authorization complete. You can close this tab and return to your terminal.</p></body></html>";
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

pub async fn poll_openai_device_auth(
    http: &reqwest::Client,
    start: &OpenAiDeviceStart,
) -> Result<StoredProvider> {
    loop {
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
        if status.is_success() {
            let val: serde_json::Value = resp.json().await?;
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
        if status.as_u16() != 403 && status.as_u16() != 404 {
            let text = resp.text().await.unwrap_or_default();
            bail!("OpenAI device auth failed ({status}): {text}");
        }
        tokio::time::sleep(Duration::from_secs(start.interval_s + 2)).await;
    }
}
