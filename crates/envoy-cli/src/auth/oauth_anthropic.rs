//! Claude Pro/Max and Anthropic Console OAuth.
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;

use super::pkce::{extract_query_param, form_encode, Pkce};
use super::{
    now_ms, Credential, ProviderProtocol, StoredProvider, ANTHROPIC_CLAUDE_AUTH_URL,
    ANTHROPIC_CLIENT_ID, ANTHROPIC_CONSOLE_AUTH_URL, ANTHROPIC_CREATE_KEY_URL,
    ANTHROPIC_REDIRECT_URL, ANTHROPIC_TOKEN_URL,
};

pub fn anthropic_authorize_url(console: bool, pkce: &Pkce, state: &str) -> String {
    let base = if console {
        ANTHROPIC_CONSOLE_AUTH_URL
    } else {
        ANTHROPIC_CLAUDE_AUTH_URL
    };
    let q = form_encode(&[
        ("code", "true"),
        ("client_id", ANTHROPIC_CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", ANTHROPIC_REDIRECT_URL),
        ("scope", "org:create_api_key user:profile user:inference"),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ]);
    format!("{base}?{q}")
}

/// Parse either `code#state`, a full redirect URL `...?code=...&state=...`, or raw code.
pub fn parse_code_and_state(input: &str, fallback_state: &str) -> Result<(String, String)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("authorization code is empty");
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let code = extract_query_param(trimmed, "code")
            .context("redirect URL does not contain `code=`")?;
        let state =
            extract_query_param(trimmed, "state").unwrap_or_else(|| fallback_state.to_string());
        return Ok((code, state));
    }
    if let Some((code, state)) = trimmed.split_once('#') {
        return Ok((code.trim().to_string(), state.trim().to_string()));
    }
    Ok((trimmed.to_string(), fallback_state.to_string()))
}

pub async fn exchange_anthropic_code(
    http: &reqwest::Client,
    raw_input: &str,
    verifier: &str,
    expected_state: &str,
    create_api_key: bool,
) -> Result<StoredProvider> {
    let (code, state) = parse_code_and_state(raw_input, expected_state)?;
    let resp = http
        .post(ANTHROPIC_TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
            "state": state,
            "client_id": ANTHROPIC_CLIENT_ID,
            "redirect_uri": ANTHROPIC_REDIRECT_URL,
            "code_verifier": verifier,
        }))
        .send()
        .await
        .context("exchanging Anthropic OAuth code")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Anthropic token exchange failed ({status}): {text}");
    }
    let tok: serde_json::Value = serde_json::from_str(&text)?;
    let access_token = tok["access_token"]
        .as_str()
        .context("missing access_token")?
        .to_string();
    let refresh_token = tok["refresh_token"].as_str().map(str::to_string);
    let expires_in = tok["expires_in"].as_u64().unwrap_or(3600);

    if create_api_key {
        let key_resp = http
            .post(ANTHROPIC_CREATE_KEY_URL)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .send()
            .await
            .context("creating Anthropic API key via OAuth")?;
        let k_status = key_resp.status();
        let k_text = key_resp.text().await.unwrap_or_default();
        if !k_status.is_success() {
            bail!("Anthropic create_api_key failed ({k_status}): {k_text}");
        }
        let k_val: serde_json::Value = serde_json::from_str(&k_text)?;
        let raw_key = k_val["raw_key"]
            .as_str()
            .context("missing raw_key in response")?
            .to_string();
        return Ok(StoredProvider {
            id: "anthropic".to_string(),
            name: "Anthropic (Claude)".to_string(),
            protocol: ProviderProtocol::Anthropic,
            base_url: "https://api.anthropic.com/v1".to_string(),
            credential: Credential::ApiKey { key: raw_key },
            custom_models: Vec::new(),
            model_routes: BTreeMap::new(),
            updated_ms: now_ms(),
        });
    }

    Ok(StoredProvider {
        id: "anthropic".to_string(),
        name: "Anthropic (Claude)".to_string(),
        protocol: ProviderProtocol::Anthropic,
        base_url: "https://api.anthropic.com/v1".to_string(),
        credential: Credential::OAuth {
            access_token,
            refresh_token,
            expires_at_ms: Some(now_ms() + expires_in * 1000),
            method_id: "claude-oauth".to_string(),
            account_id: None,
        },
        custom_models: Vec::new(),
        model_routes: BTreeMap::new(),
        updated_ms: now_ms(),
    })
}
