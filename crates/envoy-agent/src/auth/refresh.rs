//! Refresh an OAuth access token that is about to expire.
//!
//! One in-process lock per provider id serializes refreshes so two subagents
//! cannot rotate the same refresh token. A routed or env base URL on the
//! in-memory provider is never written back to `auth.json`.
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use super::pkce::{extract_chatgpt_account_id, form_encode};
use super::{
    load_auth, now_ms, save_auth, Credential, StoredProvider, ANTHROPIC_CLIENT_ID,
    ANTHROPIC_TOKEN_URL, OPENAI_AUTH_BASE, OPENAI_CLIENT_ID,
};

pub async fn ensure_fresh_provider(
    http: &reqwest::Client,
    data_dir: &Path,
    provider: StoredProvider,
) -> Result<StoredProvider> {
    ensure_fresh_at(http, data_dir, provider, None).await
}

pub(crate) async fn ensure_fresh_at(
    http: &reqwest::Client,
    data_dir: &Path,
    mut provider: StoredProvider,
    oauth_origin: Option<&str>,
) -> Result<StoredProvider> {
    let gate = refresh_gate(&provider.id);
    let _hold = gate.lock().await;
    if let Some(disk) = load_auth(data_dir).credentials.get(&provider.id).cloned() {
        if !credential_expiring(&disk.credential) {
            provider.credential = disk.credential;
            return Ok(provider);
        }
        provider.credential = disk.credential;
    }
    if !credential_expiring(&provider.credential) {
        return Ok(provider);
    }

    let Credential::OAuth {
        refresh_token: Some(ref refresh),
        ref method_id,
        ref account_id,
        ..
    } = provider.credential
    else {
        return Ok(provider);
    };

    if provider.id == "openai" {
        let body = form_encode(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", OPENAI_CLIENT_ID),
        ]);
        let resp = http
            .post(openai_token_url(oauth_origin))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .context("refreshing OpenAI OAuth token")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("OpenAI OAuth refresh failed ({status}): {text} — please run /login again");
        }
        let tok: serde_json::Value = resp.json().await?;
        let access_token = tok["access_token"]
            .as_str()
            .context("missing refreshed access_token")?
            .to_string();
        let new_refresh = tok["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or_else(|| Some(refresh.clone()));
        let expires_in = tok["expires_in"].as_u64().unwrap_or(3600);
        let new_acct = tok["id_token"]
            .as_str()
            .and_then(extract_chatgpt_account_id)
            .or_else(|| extract_chatgpt_account_id(&access_token))
            .or_else(|| account_id.clone());
        provider.credential = Credential::OAuth {
            access_token,
            refresh_token: new_refresh,
            expires_at_ms: Some(now_ms() + expires_in * 1000),
            method_id: method_id.clone(),
            account_id: new_acct,
        };
    } else if provider.id == "anthropic" {
        let resp = http
            .post(anthropic_token_url(oauth_origin))
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh,
                "client_id": ANTHROPIC_CLIENT_ID,
            }))
            .send()
            .await
            .context("refreshing Anthropic OAuth token")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("Anthropic OAuth refresh failed ({status}): {text} — please run /login again");
        }
        let tok: serde_json::Value = resp.json().await?;
        let access_token = tok["access_token"]
            .as_str()
            .context("missing refreshed access_token")?
            .to_string();
        let new_refresh = tok["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or_else(|| Some(refresh.clone()));
        let expires_in = tok["expires_in"].as_u64().unwrap_or(3600);
        provider.credential = Credential::OAuth {
            access_token,
            refresh_token: new_refresh,
            expires_at_ms: Some(now_ms() + expires_in * 1000),
            method_id: method_id.clone(),
            account_id: account_id.clone(),
        };
    }

    provider.updated_ms = now_ms();
    persist_credential(data_dir, &provider);
    Ok(provider)
}

fn credential_expiring(credential: &Credential) -> bool {
    match credential {
        Credential::OAuth {
            expires_at_ms: Some(exp),
            refresh_token: Some(_),
            ..
        } => now_ms() + 60_000 >= *exp,
        _ => false,
    }
}

/// Write the new credential onto the stored provider. `base_url` stays as it
/// was saved at login, even when this process routed through `ENVOY_BASE_URL`.
fn persist_credential(data_dir: &Path, provider: &StoredProvider) {
    let mut auth = load_auth(data_dir);
    let Some(stored) = auth.credentials.get_mut(&provider.id) else {
        return;
    };
    stored.credential = provider.credential.clone();
    stored.updated_ms = provider.updated_ms;
    let _ = save_auth(data_dir, &auth);
}

fn refresh_gate(provider_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static GATES: OnceLock<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let map = GATES.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|err| err.into_inner());
    guard
        .entry(provider_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn openai_token_url(origin: Option<&str>) -> String {
    match origin {
        Some(origin) => format!("{}/oauth/token", origin.trim_end_matches('/')),
        None => format!("{OPENAI_AUTH_BASE}/oauth/token"),
    }
}

fn anthropic_token_url(origin: Option<&str>) -> String {
    match origin {
        Some(origin) => format!("{}/anthropic/token", origin.trim_end_matches('/')),
        None => ANTHROPIC_TOKEN_URL.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{save_auth, AuthFile, Credential, ProviderProtocol};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn access_token(credential: &Credential) -> &str {
        match credential {
            Credential::OAuth { access_token, .. } => access_token,
            _ => "",
        }
    }

    fn tmpdir() -> std::path::PathBuf {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "envoy-refresh-{:02x}{:02x}{:02x}{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3]
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn expired_openai(base_url: &str) -> StoredProvider {
        StoredProvider {
            id: "openai".into(),
            name: "OpenAI".into(),
            protocol: ProviderProtocol::OpenAiCodex,
            base_url: base_url.into(),
            credential: Credential::OAuth {
                access_token: "old".into(),
                refresh_token: Some("rotating".into()),
                expires_at_ms: Some(1),
                method_id: "chatgpt".into(),
                account_id: None,
            },
            custom_models: Vec::new(),
            fetched_models: Vec::new(),
            model_routes: BTreeMap::new(),
            updated_ms: 1,
        }
    }

    #[tokio::test]
    async fn concurrent_refresh_hits_upstream_once_and_keeps_stored_base_url() {
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let counter = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = r#"{"access_token":"fresh","refresh_token":"next","expires_in":3600}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let dir = tmpdir();
        let stored = expired_openai("https://stored.example/v1");
        let mut auth = AuthFile::default();
        auth.credentials.insert("openai".into(), stored);
        save_auth(&dir, &auth).unwrap();

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let routed = expired_openai("http://override.example/v1");
        let left = routed.clone();
        let right = routed.clone();
        let (a, b) = tokio::join!(
            ensure_fresh_at(&http, &dir, left, Some(&origin)),
            ensure_fresh_at(&http, &dir, right, Some(&origin)),
        );
        let a = a.unwrap();
        let b = b.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(a.base_url, "http://override.example/v1");
        assert_eq!(b.base_url, "http://override.example/v1");
        assert_eq!(access_token(&a.credential), "fresh");
        let saved = load_auth(&dir);
        let disk = saved.credentials.get("openai").unwrap();
        assert_eq!(disk.base_url, "https://stored.example/v1");
        assert_eq!(access_token(&disk.credential), "fresh");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
