//! GitHub Copilot device login and the Copilot session token exchange.
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::pkce::form_encode;
use super::{now_ms, Credential, ProviderProtocol, StoredProvider, GITHUB_COPILOT_CLIENT_ID};

#[derive(Debug, Clone)]
pub struct GitHubDeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval_s: u64,
}

pub async fn start_github_copilot_device(http: &reqwest::Client) -> Result<GitHubDeviceStart> {
    let body = form_encode(&[
        ("client_id", GITHUB_COPILOT_CLIENT_ID),
        ("scope", "read:user"),
    ]);
    let resp = http
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", format!("jade/{}", env!("CARGO_PKG_VERSION")))
        .body(body)
        .send()
        .await
        .context("starting GitHub device authorization")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("GitHub device start failed ({status}): {text}");
    }
    let val: serde_json::Value = serde_json::from_str(&text)?;
    Ok(GitHubDeviceStart {
        device_code: val["device_code"]
            .as_str()
            .context("missing device_code")?
            .to_string(),
        user_code: val["user_code"]
            .as_str()
            .context("missing user_code")?
            .to_string(),
        verification_uri: val["verification_uri"]
            .as_str()
            .unwrap_or("https://github.com/login/device")
            .to_string(),
        interval_s: val["interval"].as_u64().unwrap_or(5).max(1),
    })
}

const DEVICE_POLL_LIMIT: Duration = Duration::from_secs(15 * 60);

pub async fn poll_github_copilot_device(
    http: &reqwest::Client,
    start: &GitHubDeviceStart,
) -> Result<StoredProvider> {
    let mut interval = start.interval_s;
    let deadline = Instant::now() + DEVICE_POLL_LIMIT;
    loop {
        if Instant::now() >= deadline {
            bail!("GitHub device authorization timed out");
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if Instant::now() >= deadline {
            bail!("GitHub device authorization timed out");
        }
        let body = form_encode(&[
            ("client_id", GITHUB_COPILOT_CLIENT_ID),
            ("device_code", &start.device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ]);
        let resp = http
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", format!("jade/{}", env!("CARGO_PKG_VERSION")))
            .body(body)
            .send()
            .await
            .context("polling GitHub device authorization")?;
        let val: serde_json::Value = resp.json().await?;
        if let Some(access_token) = val["access_token"].as_str() {
            return Ok(StoredProvider {
                id: "github-copilot".to_string(),
                name: "GitHub Copilot".to_string(),
                protocol: ProviderProtocol::OpenAiChat,
                base_url: "https://api.githubcopilot.com".to_string(),
                credential: Credential::OAuth {
                    access_token: access_token.to_string(),
                    refresh_token: None,
                    expires_at_ms: None,
                    method_id: "github-copilot".to_string(),
                    account_id: None,
                },
                custom_models: Vec::new(),
                fetched_models: Vec::new(),
                model_routes: BTreeMap::new(),
                extra_headers: Default::default(),
                updated_ms: now_ms(),
            });
        }
        match val["error"].as_str() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += 5;
                continue;
            }
            Some(other) => {
                let desc = val["error_description"].as_str().unwrap_or(other);
                bail!("GitHub OAuth failed: {desc}");
            }
            None => bail!("Unexpected GitHub OAuth response: {val}"),
        }
    }
}

struct CopilotSessionCache {
    gh_token: String,
    token: String,
    base: String,
    expires_at_ms: u64,
}

static COPILOT_SESSION: Mutex<Option<CopilotSessionCache>> = Mutex::new(None);

fn cache_lock() -> std::sync::MutexGuard<'static, Option<CopilotSessionCache>> {
    COPILOT_SESSION
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

/// Refresh a minute early so an in-flight turn does not use an expired bearer.
fn cached_copilot(gh_token: &str, now: u64) -> Option<(String, String)> {
    let guard = cache_lock();
    let hit = guard.as_ref()?;
    if hit.gh_token != gh_token {
        return None;
    }
    if now.saturating_add(60_000) >= hit.expires_at_ms {
        return None;
    }
    Some((hit.token.clone(), hit.base.clone()))
}

fn store_copilot(gh_token: &str, token: String, base: String, expires_at_ms: u64) {
    *cache_lock() = Some(CopilotSessionCache {
        gh_token: gh_token.to_string(),
        token,
        base,
        expires_at_ms,
    });
}

fn expires_number(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn copilot_expires_at_ms(val: &serde_json::Value, now: u64) -> u64 {
    match val.get("expires_at").and_then(expires_number) {
        Some(n) if n >= 1_000_000_000_000 => n,
        Some(n) if n >= 1_000_000_000 => n.saturating_mul(1000),
        _ => now.saturating_add(10 * 60 * 1000),
    }
}

/// Exchange GitHub OAuth token for a Copilot session bearer token and API base URL.
/// Successful exchanges are cached until `expires_at` minus 60 seconds.
pub async fn copilot_session_token(
    http: &reqwest::Client,
    gh_token: &str,
) -> Result<(String, String)> {
    let now = now_ms();
    if let Some(hit) = cached_copilot(gh_token, now) {
        return Ok(hit);
    }
    let resp = http
        .get("https://api.github.com/copilot_internal/v2/token")
        .header("Authorization", format!("token {gh_token}"))
        .header("User-Agent", format!("jade/{}", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/json")
        .send()
        .await
        .context("exchanging GitHub token for Copilot session token")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("GitHub Copilot token exchange failed ({status}): {text}");
    }
    let val: serde_json::Value = serde_json::from_str(&text)?;
    let token = val["token"]
        .as_str()
        .context("missing Copilot token")?
        .to_string();
    let base = val["endpoints"]["api"]
        .as_str()
        .unwrap_or("https://api.githubcopilot.com")
        .trim_end_matches('/')
        .to_string();
    store_copilot(
        gh_token,
        token.clone(),
        base.clone(),
        copilot_expires_at_ms(&val, now),
    );
    Ok((token, base))
}

pub async fn start_github_copilot_device_default() -> Result<GitHubDeviceStart> {
    let http = crate::agent::shared_http_client()?;
    start_github_copilot_device(&http).await
}

pub async fn poll_github_copilot_device_default(
    start: &GitHubDeviceStart,
) -> Result<StoredProvider> {
    let http = crate::agent::shared_http_client()?;
    poll_github_copilot_device(&http, start).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copilot_cache_refreshes_sixty_seconds_early() {
        let now = 1_700_000_000_000;
        store_copilot(
            "gh-cache-test",
            "session".into(),
            "https://api.githubcopilot.com".into(),
            now + 120_000,
        );
        let hit = cached_copilot("gh-cache-test", now).unwrap();
        assert_eq!(hit.0, "session");
        assert!(cached_copilot("gh-cache-test", now + 70_000).is_none());
        assert!(cached_copilot("other-token", now).is_none());
    }

    #[test]
    fn copilot_expiry_accepts_unix_seconds_and_millis() {
        let now = 1_700_000_000_000u64;
        let seconds = serde_json::json!({"expires_at": 1_700_000_120u64});
        assert_eq!(copilot_expires_at_ms(&seconds, now), 1_700_000_120_000);
        let text = serde_json::json!({"expires_at": "1700000120000"});
        assert_eq!(copilot_expires_at_ms(&text, now), 1_700_000_120_000);
        let missing = serde_json::json!({});
        assert_eq!(copilot_expires_at_ms(&missing, now), now + 600_000);
    }
}
