//! Provider credentials, model preferences, and the builtin provider catalog.
//!
//! OAuth and device flows live in the sibling modules. Credentials go in
//! `<data_dir>/auth.json` (mode 0600 on Unix). Preferences go in `model.json`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

mod catalog;
mod cli;
mod github;
mod oauth_anthropic;
mod oauth_openai;
mod pkce;
mod refresh;
mod routing;

pub use catalog::BUILTIN_PROVIDERS;
pub use cli::{cmd_auth_list, cmd_auth_login, cmd_auth_logout};
pub use github::{copilot_session_token, poll_github_copilot_device, start_github_copilot_device};
pub use oauth_anthropic::{anthropic_authorize_url, exchange_anthropic_code};
pub use oauth_openai::{
    bind_openai_callback_listener, exchange_openai_code, openai_authorize_url,
    poll_openai_device_auth, start_openai_device_auth, wait_openai_browser_callback,
};
pub use pkce::{extract_query_param, generate_pkce, open_browser, random_state, Pkce};
pub use refresh::ensure_fresh_provider;
pub use routing::{resolve_model_target, resolve_provider_with_overrides};

pub const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const OPENAI_AUTH_BASE: &str = "https://auth.openai.com";
pub const OPENAI_CODEX_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

pub const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const ANTHROPIC_CLAUDE_AUTH_URL: &str = "https://claude.com/cai/oauth/authorize";
pub const ANTHROPIC_CONSOLE_AUTH_URL: &str = "https://platform.claude.com/oauth/authorize";
pub const ANTHROPIC_REDIRECT_URL: &str = "https://platform.claude.com/oauth/code/callback";
pub const ANTHROPIC_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const ANTHROPIC_CREATE_KEY_URL: &str =
    "https://api.anthropic.com/api/oauth/claude_cli/create_api_key";

pub const GITHUB_COPILOT_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    OpenAiChat,
    OpenAiCodex,
    Anthropic,
}

impl ProviderProtocol {
    pub fn label(self) -> &'static str {
        match self {
            ProviderProtocol::OpenAiChat => "OpenAI-Compatible",
            ProviderProtocol::OpenAiCodex => "OpenAI Codex (OAuth)",
            ProviderProtocol::Anthropic => "Anthropic Messages",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey {
        key: String,
    },
    OAuth {
        access_token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_ms: Option<u64>,
        method_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
    },
}

impl Credential {
    pub fn kind_label(&self) -> &'static str {
        match self {
            Credential::ApiKey { .. } => "api_key",
            Credential::OAuth { .. } => "oauth",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Credential::ApiKey { key } => mask_secret(key),
            Credential::OAuth {
                method_id,
                expires_at_ms,
                ..
            } => {
                let exp = match expires_at_ms {
                    Some(ms) if *ms <= now_ms() => "expired (auto-refresh)".to_string(),
                    Some(ms) => {
                        let mins = (*ms - now_ms()) / 60_000;
                        if mins >= 60 {
                            format!("expires in {}h", mins / 60)
                        } else {
                            format!("expires in {mins}m")
                        }
                    }
                    None => "active".to_string(),
                };
                format!("{method_id} · {exp}")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredProvider {
    pub id: String,
    pub name: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub credential: Credential,
    #[serde(default)]
    pub custom_models: Vec<String>,
    #[serde(default)]
    pub model_routes: BTreeMap<String, ModelRoute>,
    #[serde(default)]
    pub updated_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelRoute {
    pub protocol: ProviderProtocol,
    pub base_url: String,
}

impl StoredProvider {
    pub fn routed_for(&self, model_id: &str) -> Self {
        let Some(route) = self.model_routes.get(model_id) else {
            return self.clone();
        };
        let mut copy = self.clone();
        copy.protocol = route.protocol;
        copy.base_url = route.base_url.clone();
        copy
    }
}

pub fn is_retired_model(provider_id: &str, model_id: &str) -> bool {
    provider_id == "deepseek"
        && matches!(
            model_id,
            "deepseek-chat" | "deepseek-reasoner" | "deepseek-v4.1-flash"
        )
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthFile {
    #[serde(default)]
    pub credentials: BTreeMap<String, StoredProvider>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
}

impl ModelRef {
    pub fn display(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPrefs {
    pub active: Option<ModelRef>,
    #[serde(default)]
    pub recent: Vec<ModelRef>,
    #[serde(default)]
    pub theme: usize,
    #[serde(default)]
    pub show_sidebar: bool,
    #[serde(default)]
    pub budget_tier: crate::agent::CompactionTier,
    #[serde(default)]
    pub model_budgets: BTreeMap<String, u64>,
}

impl Default for ModelPrefs {
    fn default() -> Self {
        Self {
            active: None,
            recent: Vec::new(),
            theme: 0,
            show_sidebar: true,
            budget_tier: crate::agent::CompactionTier::Native,
            model_budgets: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethodKind {
    OpenAiBrowserOAuth,
    OpenAiDeviceOAuth,
    OpenAiApiKey,
    AnthropicClaudeOAuth,
    AnthropicConsoleOAuth,
    AnthropicApiKey,
    GitHubCopilotDeviceOAuth,
    SimpleApiKey,
    CustomForm,
}

#[derive(Debug, Clone, Copy)]
pub struct AuthMethodDef {
    pub kind: AuthMethodKind,
    pub label: &'static str,
    pub hint: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct BuiltinProvider {
    pub id: &'static str,
    pub name: &'static str,
    pub subtitle: &'static str,
    pub default_base_url: &'static str,
    pub protocol: ProviderProtocol,
    pub env_var: Option<&'static str>,
    pub methods: &'static [AuthMethodDef],
}

pub fn auth_path(data_dir: &Path) -> PathBuf {
    data_dir.join("auth.json")
}

pub fn prefs_path(data_dir: &Path) -> PathBuf {
    data_dir.join("model.json")
}

pub fn load_auth(data_dir: &Path) -> AuthFile {
    let path = auth_path(data_dir);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return AuthFile::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save_auth(data_dir: &Path, auth: &AuthFile) -> Result<()> {
    envoy_core::fs::ensure_private_dir(data_dir)?;
    let data = serde_json::to_string_pretty(auth)?;
    envoy_core::fs::write_atomic_sync(&auth_path(data_dir), data.as_bytes(), 0o600)
        .context("writing auth.json")?;
    Ok(())
}

pub fn load_prefs(data_dir: &Path) -> ModelPrefs {
    let path = prefs_path(data_dir);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return ModelPrefs::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save_prefs(data_dir: &Path, prefs: &ModelPrefs) -> Result<()> {
    envoy_core::fs::ensure_private_dir(data_dir)?;
    let data = serde_json::to_string_pretty(prefs)?;
    envoy_core::fs::write_atomic_sync(&prefs_path(data_dir), data.as_bytes(), 0o600)
        .context("writing model.json")?;
    Ok(())
}

/// Merge persisted credentials in `auth.json` with environment-variable API keys.
pub fn effective_providers(data_dir: &Path) -> BTreeMap<String, StoredProvider> {
    effective_providers_with(data_dir, &|k| std::env::var(k).ok())
}

pub(crate) fn effective_providers_with(
    data_dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> BTreeMap<String, StoredProvider> {
    let mut map = load_auth(data_dir).credentials;
    routing::merge_env_providers(&mut map, env);
    map
}

/// All selectable models across currently connected providers.
#[derive(Debug, Clone)]
pub struct AvailableModel {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub auth_kind: &'static str,
}

pub fn available_models(data_dir: &Path) -> Vec<AvailableModel> {
    let providers = effective_providers(data_dir);
    let mut out = Vec::new();
    for prov in providers.values() {
        let mut seen = Vec::new();
        for m in &prov.custom_models {
            if is_retired_model(&prov.id, m) || seen.contains(m) {
                continue;
            }
            seen.push(m.clone());
        }
        for m in seen {
            out.push(AvailableModel {
                provider_id: prov.id.clone(),
                provider_name: prov.name.clone(),
                model_id: m,
                auth_kind: prov.credential.kind_label(),
            });
        }
    }
    out
}

pub fn default_model_for(provider: &StoredProvider) -> String {
    provider
        .custom_models
        .iter()
        .find(|m| !is_retired_model(&provider.id, m))
        .cloned()
        .unwrap_or_default()
}

pub fn record_provider_login(
    data_dir: &Path,
    provider: StoredProvider,
    preferred_model: Option<String>,
) -> Result<ModelRef> {
    let model_id = preferred_model
        .filter(|s| !s.trim().is_empty())
        .filter(|s| !is_retired_model(&provider.id, s))
        .or_else(|| {
            let fallback = default_model_for(&provider);
            if fallback.is_empty() {
                None
            } else {
                Some(fallback)
            }
        });

    let mut auth = load_auth(data_dir);
    auth.credentials
        .insert(provider.id.clone(), provider.clone());
    save_auth(data_dir, &auth)?;

    let mut prefs = load_prefs(data_dir);
    let model_ref = ModelRef {
        provider_id: provider.id,
        model_id: model_id.unwrap_or_default(),
    };
    if model_ref.model_id.is_empty() {
        prefs.active = Some(model_ref.clone());
        save_prefs(data_dir, &prefs)?;
        return Ok(model_ref);
    }
    prefs.active = Some(model_ref.clone());
    prefs.recent.retain(|r| r != &model_ref);
    prefs.recent.insert(0, model_ref.clone());
    prefs.recent.truncate(12);
    save_prefs(data_dir, &prefs)?;
    Ok(model_ref)
}

pub fn set_active_model(data_dir: &Path, model_ref: ModelRef) -> Result<()> {
    let mut prefs = load_prefs(data_dir);
    prefs.active = Some(model_ref.clone());
    prefs.recent.retain(|r| r != &model_ref);
    prefs.recent.insert(0, model_ref);
    prefs.recent.truncate(12);
    save_prefs(data_dir, &prefs)
}

pub fn remove_provider(data_dir: &Path, provider_id: &str) -> Result<bool> {
    let mut auth = load_auth(data_dir);
    let removed = auth.credentials.remove(provider_id).is_some();
    save_auth(data_dir, &auth)?;
    let mut prefs = load_prefs(data_dir);
    if prefs
        .active
        .as_ref()
        .is_some_and(|a| a.provider_id == provider_id)
    {
        prefs.active = None;
    }
    prefs.recent.retain(|r| r.provider_id != provider_id);
    if prefs.active.is_none() {
        if let Some(next) = available_models(data_dir).into_iter().next() {
            prefs.active = Some(ModelRef {
                provider_id: next.provider_id,
                model_id: next.model_id,
            });
        }
    }
    save_prefs(data_dir, &prefs)?;
    Ok(removed)
}

pub fn slugify_provider_id(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "custom".to_string()
    } else {
        out
    }
}

pub fn resolve_active_provider(data_dir: &Path) -> Option<(StoredProvider, ModelRef)> {
    let providers = effective_providers(data_dir);
    let prefs = load_prefs(data_dir);
    if let Some(active) = prefs.active {
        if let Some(prov) = providers.get(&active.provider_id) {
            if is_retired_model(&active.provider_id, &active.model_id) {
                return Some((
                    prov.clone(),
                    ModelRef {
                        provider_id: prov.id.clone(),
                        model_id: default_model_for(prov),
                    },
                ));
            }
            return Some((prov.clone(), active));
        }
    }
    let first = providers.values().next()?.clone();
    let model = ModelRef {
        provider_id: first.id.clone(),
        model_id: default_model_for(&first),
    };
    Some((first, model))
}

pub fn mask_secret(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return "(none)".into();
    }
    if chars.len() <= 8 {
        return "••••••••".into();
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{head}…{tail}")
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::oauth_anthropic::parse_code_and_state;
    use super::pkce::extract_chatgpt_account_id;
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;

    fn tmpdir() -> PathBuf {
        let n = now_ms() + (std::process::id() as u64);
        let p = std::env::temp_dir().join(format!("envoy-auth-{n}-{}", rand_suffix()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn rand_suffix() -> u64 {
        let mut b = [0u8; 8];
        let _ = getrandom::fill(&mut b);
        u64::from_le_bytes(b)
    }

    #[test]
    fn pkce_and_state_are_valid_base64url() {
        let pkce = generate_pkce().unwrap();
        assert_eq!(pkce.verifier.len(), 43);
        assert!(!pkce.challenge.contains('='));
        assert!(!pkce.challenge.contains('+'));
        assert!(!pkce.challenge.contains('/'));
        let st = random_state().unwrap();
        assert!(!st.is_empty());
    }

    #[test]
    fn parses_anthropic_code_hash_state_and_redirect_url() {
        let (code, state) = parse_code_and_state("abc123#st456", "fallback").unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state, "st456");

        let (code2, state2) = parse_code_and_state(
            "https://platform.claude.com/oauth/code/callback?code=xyz%2B1&state=s9",
            "fallback",
        )
        .unwrap();
        assert_eq!(code2, "xyz+1");
        assert_eq!(state2, "s9");
    }

    #[test]
    fn extracts_chatgpt_account_id_from_jwt() {
        let header = URL_SAFE_NO_PAD.encode(b"{}");
        let payload = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_12345"}}"#);
        let jwt = format!("{header}.{payload}.sig");
        assert_eq!(
            extract_chatgpt_account_id(&jwt).as_deref(),
            Some("acct_12345")
        );
    }

    #[test]
    fn auth_store_roundtrips_with_0600_permissions() {
        let dir = tmpdir();
        let sp = StoredProvider {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            protocol: ProviderProtocol::OpenAiChat,
            base_url: "https://api.deepseek.com/v1".into(),
            credential: Credential::ApiKey {
                key: "sk-test-1234567890".into(),
            },
            custom_models: vec![],
            model_routes: BTreeMap::new(),
            updated_ms: now_ms(),
        };
        let model_ref = record_provider_login(&dir, sp, None).unwrap();
        assert!(model_ref.model_id.is_empty());

        let loaded = load_auth(&dir);
        assert!(loaded.credentials.contains_key("deepseek"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(auth_path(&dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        let (active_prov, active_model) = resolve_active_provider(&dir).unwrap();
        assert_eq!(active_prov.id, "deepseek");
        assert!(active_model.model_id.is_empty());

        assert!(remove_provider(&dir, "deepseek").unwrap());
        assert!(load_auth(&dir).credentials.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retired_active_id_stays_on_that_provider() {
        let dir = tmpdir();
        let anthropic = StoredProvider {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            protocol: ProviderProtocol::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            credential: Credential::ApiKey {
                key: "sk-ant".into(),
            },
            custom_models: vec!["claude-test".into()],
            model_routes: BTreeMap::new(),
            updated_ms: now_ms(),
        };
        let deepseek = StoredProvider {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            protocol: ProviderProtocol::OpenAiChat,
            base_url: "https://api.deepseek.com/v1".into(),
            credential: Credential::ApiKey {
                key: "sk-ds".into(),
            },
            custom_models: vec!["deepseek-chat".into(), "deepseek-flash".into()],
            model_routes: BTreeMap::new(),
            updated_ms: now_ms(),
        };
        record_provider_login(&dir, anthropic, Some("claude-test".into())).unwrap();
        record_provider_login(&dir, deepseek, Some("deepseek-flash".into())).unwrap();
        let mut prefs = load_prefs(&dir);
        prefs.active = Some(ModelRef {
            provider_id: "deepseek".into(),
            model_id: "deepseek-chat".into(),
        });
        save_prefs(&dir, &prefs).unwrap();

        let (prov, model) = resolve_active_provider(&dir).unwrap();
        assert_eq!(prov.id, "deepseek");
        assert_eq!(model.model_id, "deepseek-flash");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
