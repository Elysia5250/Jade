//! Fetch a provider's model list with the credential its stream already uses.
//!
//! Codex stores the full `/responses` POST URL, so its catalog is `GET /codex/models`
//! on that host. Copilot chat uses the session token, not the GitHub device token.

use super::catalog::{
    parse_listed_models, remember_caps_from_models, usable_listed_models, ListedModel,
};
use crate::auth::{copilot_session_token, Credential, ProviderProtocol, StoredProvider};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

pub async fn refresh_active_provider_models(data_dir: &Path) -> Option<(String, Vec<ListedModel>)> {
    let (prov, _) = crate::auth::resolve_provider_with_overrides(data_dir, None)?;
    let http = crate::agent::shared_http_client().ok()?;
    let bg_http = http.clone();
    let bg_dir = data_dir.to_path_buf();
    tokio::spawn(async move {
        super::refresh_models_dev_if_stale(&bg_http, &bg_dir).await;
    });
    let list = fetch_provider_models(&http, Some(data_dir), &prov).await;
    Some((prov.id, list))
}

pub async fn fetch_provider_models(
    http: &reqwest::Client,
    data_dir: Option<&Path>,
    provider: &StoredProvider,
) -> Vec<ListedModel> {
    match &provider.credential {
        Credential::ApiKey { key } => {
            if provider.protocol == ProviderProtocol::OpenAiCodex {
                return fetch_codex_models(http, data_dir, provider, key, None).await;
            }
            super::catalog::fetch_listed_models(
                http,
                data_dir,
                &provider.id,
                provider.protocol,
                &provider.base_url,
                key,
            )
            .await
        }
        Credential::OAuth {
            access_token,
            method_id,
            account_id,
            ..
        } => {
            if method_id == "github-copilot" {
                let Ok((token, base)) = copilot_session_token(http, access_token).await else {
                    return Vec::new();
                };
                return super::catalog::fetch_listed_models(
                    http,
                    data_dir,
                    &provider.id,
                    ProviderProtocol::OpenAiChat,
                    &base,
                    &token,
                )
                .await;
            }
            if provider.protocol == ProviderProtocol::Anthropic {
                return fetch_anthropic_oauth_models(http, data_dir, provider, access_token).await;
            }
            if provider.protocol == ProviderProtocol::OpenAiCodex {
                return fetch_codex_models(
                    http,
                    data_dir,
                    provider,
                    access_token,
                    account_id.as_deref(),
                )
                .await;
            }
            super::catalog::fetch_listed_models(
                http,
                data_dir,
                &provider.id,
                provider.protocol,
                &provider.base_url,
                access_token,
            )
            .await
        }
    }
}

fn codex_models_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let root = trimmed.strip_suffix("/responses").unwrap_or(trimmed);
    format!("{root}/models")
}

async fn fetch_codex_models(
    http: &reqwest::Client,
    data_dir: Option<&Path>,
    provider: &StoredProvider,
    token: &str,
    account_id: Option<&str>,
) -> Vec<ListedModel> {
    let mut req = http
        .get(codex_models_url(&provider.base_url))
        .bearer_auth(token)
        .header("originator", "jade")
        .header("Accept", "application/json");
    if let Some(account) = account_id {
        req = req.header("ChatGPT-Account-ID", account);
    }
    finish_models_fetch(req, data_dir, &provider.id, true).await
}

async fn fetch_anthropic_oauth_models(
    http: &reqwest::Client,
    data_dir: Option<&Path>,
    provider: &StoredProvider,
    token: &str,
) -> Vec<ListedModel> {
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    let req = http
        .get(url)
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .header(
            "anthropic-beta",
            "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14",
        );
    finish_models_fetch(req, data_dir, &provider.id, false).await
}

async fn finish_models_fetch(
    req: reqwest::RequestBuilder,
    data_dir: Option<&Path>,
    provider_id: &str,
    codex_shape: bool,
) -> Vec<ListedModel> {
    let Ok(resp) = req.timeout(Duration::from_secs(8)).send().await else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(val) = resp.json::<Value>().await else {
        return Vec::new();
    };
    remember_caps_from_models(data_dir, provider_id, &val);
    let parsed = if codex_shape {
        parse_codex_models(&val)
    } else {
        parse_listed_models(&val)
    };
    usable_listed_models(provider_id, parsed)
}

/// Codex `GET /codex/models` returns `{ "models": [ { "slug" } ] }`.
pub(crate) fn parse_codex_models(value: &Value) -> Vec<ListedModel> {
    let Some(arr) = value.get("models").and_then(Value::as_array) else {
        return parse_listed_models(value);
    };
    let mut out = Vec::new();
    for item in arr {
        let Some(id) = item
            .get("slug")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        out.push(ListedModel {
            id: id.to_string(),
            route: None,
        });
    }
    if out.is_empty() {
        parse_listed_models(value)
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::catalog::{parse_listed_models, store_listed_models};
    use super::*;
    use crate::auth::{
        available_models, record_provider_login, resolve_active_provider, Credential,
        ProviderProtocol, StoredProvider,
    };

    #[test]
    fn empty_api_object_uses_provider_default_and_bad_urls_are_dropped() {
        let raw = serde_json::json!({
            "data": [
                {"id": "plain"},
                {"id": "empty-api", "api": {}},
                {"id": "bad", "api": {"chat-completions": {"base_url": "javascript:alert(1)"}}},
                {"id": "ok", "api": {"chat-completions": {"base_url": "https://example.com/v1"}}}
            ]
        });
        let listed = parse_listed_models(&raw);
        let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["plain", "empty-api", "ok"]);
        assert!(listed[0].route.is_none());
        assert!(listed[1].route.is_none());
        assert_eq!(
            listed[2].route.as_ref().map(|r| r.base_url.as_str()),
            Some("https://example.com/v1")
        );
    }

    #[test]
    fn codex_catalog_uses_slugs() {
        let raw = serde_json::json!({
            "models": [{"slug": "gpt-6.1-sol"}, {"slug": "gpt-6-luna"}]
        });
        let listed = parse_codex_models(&raw);
        let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["gpt-6.1-sol", "gpt-6-luna"]);
    }

    #[test]
    fn storing_one_provider_does_not_retarget_another() {
        let dir = std::env::temp_dir().join(format!("jade-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let deepseek = StoredProvider {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            protocol: ProviderProtocol::OpenAiChat,
            base_url: "https://api.deepseek.com/v1".into(),
            credential: Credential::ApiKey { key: "sk".into() },
            custom_models: Vec::new(),
            fetched_models: Vec::new(),
            model_routes: std::collections::BTreeMap::new(),
            extra_headers: Default::default(),
            updated_ms: 0,
        };
        let anthropic = StoredProvider {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            protocol: ProviderProtocol::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            credential: Credential::ApiKey {
                key: "sk-ant".into(),
            },
            custom_models: vec!["claude-test".into()],
            fetched_models: Vec::new(),
            model_routes: std::collections::BTreeMap::new(),
            extra_headers: Default::default(),
            updated_ms: 0,
        };
        record_provider_login(&dir, deepseek, None).unwrap();
        record_provider_login(&dir, anthropic, Some("claude-test".into())).unwrap();
        let listed = parse_listed_models(&serde_json::json!({
            "data": [{"id": "deepseek-flash"}]
        }));
        assert!(store_listed_models(&dir, "deepseek", &listed)
            .unwrap()
            .is_none());
        let (_, active) = resolve_active_provider(&dir).unwrap();
        assert_eq!(active.provider_id, "anthropic");
        assert_eq!(active.model_id, "claude-test");
        let deepseek_ids: Vec<_> = available_models(&dir)
            .into_iter()
            .filter(|m| m.provider_id == "deepseek")
            .map(|m| m.model_id)
            .collect();
        assert_eq!(deepseek_ids, vec!["deepseek-flash".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
