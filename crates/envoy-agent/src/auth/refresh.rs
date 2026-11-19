//! Refresh an OAuth access token that is about to expire.
use anyhow::{bail, Context, Result};
use std::path::Path;

use super::pkce::{extract_chatgpt_account_id, form_encode};
use super::{
    load_auth, now_ms, save_auth, Credential, StoredProvider, ANTHROPIC_CLIENT_ID,
    ANTHROPIC_TOKEN_URL, OPENAI_AUTH_BASE, OPENAI_CLIENT_ID,
};

pub async fn ensure_fresh_provider(
    http: &reqwest::Client,
    data_dir: &Path,
    mut provider: StoredProvider,
) -> Result<StoredProvider> {
    let needs_refresh = match &provider.credential {
        Credential::OAuth {
            expires_at_ms: Some(exp),
            refresh_token: Some(_),
            ..
        } => now_ms() + 60_000 >= *exp,
        _ => false,
    };
    if !needs_refresh {
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
            .post(format!("{OPENAI_AUTH_BASE}/oauth/token"))
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
            .post(ANTHROPIC_TOKEN_URL)
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
    let mut auth = load_auth(data_dir);
    if auth.credentials.contains_key(&provider.id) {
        auth.credentials
            .insert(provider.id.clone(), provider.clone());
        let _ = save_auth(data_dir, &auth);
    }
    Ok(provider)
}
