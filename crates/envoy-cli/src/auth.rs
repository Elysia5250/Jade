//! Provider credentials, OAuth flows, API key storage, and model catalog.
//!
//! Stores credentials in `<data_dir>/auth.json` (mode `0600` on Unix) and UI/model
//! preferences in `<data_dir>/model.json`, mirroring `opencode`'s credential model.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

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
    pub updated_ms: u64,
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
}

impl Default for ModelPrefs {
    fn default() -> Self {
        Self {
            active: None,
            recent: Vec::new(),
            theme: 0,
            show_sidebar: false,
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
    pub models: &'static [&'static str],
}

pub const BUILTIN_PROVIDERS: &[BuiltinProvider] = &[
    BuiltinProvider {
        id: "openai",
        name: "OpenAI / ChatGPT",
        subtitle: "ChatGPT Plus/Pro OAuth or API Key",
        default_base_url: "https://api.openai.com/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("OPENAI_API_KEY"),
        methods: &[
            AuthMethodDef {
                kind: AuthMethodKind::OpenAiBrowserOAuth,
                label: "ChatGPT Plus / Pro (OAuth — Browser)",
                hint: "Opens browser login on localhost:1455 (with manual paste fallback)",
            },
            AuthMethodDef {
                kind: AuthMethodKind::OpenAiDeviceOAuth,
                label: "ChatGPT Plus / Pro (OAuth — Device Code)",
                hint: "Headless/SSH friendly: enter a short code at auth.openai.com",
            },
            AuthMethodDef {
                kind: AuthMethodKind::OpenAiApiKey,
                label: "OpenAI API Key",
                hint: "Pay-as-you-go platform.openai.com key (optional custom Base URL)",
            },
        ],
        models: &[
            "gpt-5.2-codex",
            "gpt-5.1-codex",
            "gpt-5.1",
            "gpt-4.1",
            "o3",
            "o4-mini",
            "gpt-4o",
        ],
    },
    BuiltinProvider {
        id: "anthropic",
        name: "Anthropic (Claude)",
        subtitle: "Claude Pro/Max OAuth or API Key",
        default_base_url: "https://api.anthropic.com/v1",
        protocol: ProviderProtocol::Anthropic,
        env_var: Some("ANTHROPIC_API_KEY"),
        methods: &[
            AuthMethodDef {
                kind: AuthMethodKind::AnthropicClaudeOAuth,
                label: "Claude Pro / Max (OAuth)",
                hint: "Sign in with your claude.ai subscription and paste the code",
            },
            AuthMethodDef {
                kind: AuthMethodKind::AnthropicConsoleOAuth,
                label: "Anthropic Console (OAuth -> Create API Key)",
                hint: "Sign in with Anthropic Console to mint an API key automatically",
            },
            AuthMethodDef {
                kind: AuthMethodKind::AnthropicApiKey,
                label: "Anthropic API Key",
                hint: "Enter an sk-ant-... API key (optional custom Base URL)",
            },
        ],
        models: &[
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-5-20251101",
            "claude-haiku-4-5-20251001",
            "claude-3-7-sonnet-20250219",
        ],
    },
    BuiltinProvider {
        id: "github-copilot",
        name: "GitHub Copilot",
        subtitle: "GitHub OAuth Device Flow",
        default_base_url: "https://api.githubcopilot.com",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: None,
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::GitHubCopilotDeviceOAuth,
            label: "GitHub Copilot (OAuth Device Code)",
            hint: "Sign in at github.com/login/device with your Copilot subscription",
        }],
        models: &["gpt-4o", "claude-sonnet-4", "o3-mini", "gemini-2.5-pro"],
    },
    BuiltinProvider {
        id: "deepseek",
        name: "DeepSeek",
        subtitle: "API Key",
        default_base_url: "https://api.deepseek.com/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("DEEPSEEK_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "DeepSeek API Key",
            hint: "platform.deepseek.com API key",
        }],
        models: &["deepseek-chat", "deepseek-reasoner"],
    },
    BuiltinProvider {
        id: "google",
        name: "Google Gemini",
        subtitle: "API Key",
        default_base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("GEMINI_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "Gemini API Key",
            hint: "Google AI Studio API key",
        }],
        models: &["gemini-2.5-pro", "gemini-2.5-flash"],
    },
    BuiltinProvider {
        id: "openrouter",
        name: "OpenRouter",
        subtitle: "API Key",
        default_base_url: "https://openrouter.ai/api/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("OPENROUTER_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "OpenRouter API Key",
            hint: "openrouter.ai key",
        }],
        models: &[
            "anthropic/claude-sonnet-4.5",
            "openai/gpt-5",
            "deepseek/deepseek-chat-v3-0324",
            "google/gemini-2.5-pro",
        ],
    },
    BuiltinProvider {
        id: "opencode",
        name: "OpenCode",
        subtitle: "OpenCode Zen / Go API Key",
        default_base_url: "https://opencode.ai/zen/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: Some("OPENCODE_API_KEY"),
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::SimpleApiKey,
            label: "OpenCode API Key",
            hint: "opencode.ai API key (optional custom Base URL)",
        }],
        models: &["claude-sonnet-4-5", "deepseek-v3", "gpt-5-nano"],
    },
    BuiltinProvider {
        id: "custom",
        name: "Custom Provider",
        subtitle: "OpenAI / Anthropic Compatible (Ollama, vLLM, Relay...)",
        default_base_url: "http://127.0.0.1:11434/v1",
        protocol: ProviderProtocol::OpenAiChat,
        env_var: None,
        methods: &[AuthMethodDef {
            kind: AuthMethodKind::CustomForm,
            label: "Configure custom endpoint",
            hint: "Specify Base URL, Protocol, optional API key, and Model ID",
        }],
        models: &[],
    },
];

pub fn find_builtin(id: &str) -> Option<&'static BuiltinProvider> {
    BUILTIN_PROVIDERS.iter().find(|p| p.id == id)
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
    std::fs::create_dir_all(data_dir)?;
    let tmp_dir = data_dir.join("tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let data = serde_json::to_string_pretty(auth)?;
    let tmp = tmp_dir.join("auth.json.tmp");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    let dst = auth_path(data_dir);
    std::fs::rename(&tmp, &dst).context("writing auth.json")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o600));
    }
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
    std::fs::create_dir_all(data_dir)?;
    let tmp_dir = data_dir.join("tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let data = serde_json::to_string_pretty(prefs)?;
    let tmp = tmp_dir.join("model.json.tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, prefs_path(data_dir)).context("writing model.json")?;
    Ok(())
}

/// Merge persisted credentials in `auth.json` with environment-variable API keys.
pub fn effective_providers(data_dir: &Path) -> BTreeMap<String, StoredProvider> {
    let mut map = load_auth(data_dir).credentials;
    for builtin in BUILTIN_PROVIDERS {
        if map.contains_key(builtin.id) {
            continue;
        }
        if let Some(var) = builtin.env_var {
            if let Ok(val) = std::env::var(var) {
                let val = val.trim().to_string();
                if !val.is_empty() {
                    map.insert(
                        builtin.id.to_string(),
                        StoredProvider {
                            id: builtin.id.to_string(),
                            name: format!("{} (env)", builtin.name),
                            protocol: builtin.protocol,
                            base_url: builtin.default_base_url.to_string(),
                            credential: Credential::ApiKey { key: val },
                            custom_models: Vec::new(),
                            updated_ms: 0,
                        },
                    );
                }
            }
        }
    }
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
            if !seen.contains(m) {
                seen.push(m.clone());
            }
        }
        if let Some(builtin) = find_builtin(&prov.id) {
            for &m in builtin.models {
                if prov.id == "openai"
                    && matches!(prov.protocol, ProviderProtocol::OpenAiCodex)
                    && matches!(m, "gpt-4o")
                {
                    continue;
                }
                if !seen.iter().any(|x| x == m) {
                    seen.push(m.to_string());
                }
            }
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
    if let Some(first) = provider.custom_models.first() {
        return first.clone();
    }
    if let Some(builtin) = find_builtin(&provider.id) {
        if let Some(&first) = builtin.models.first() {
            return first.to_string();
        }
    }
    "gpt-4o".to_string()
}

pub fn record_provider_login(
    data_dir: &Path,
    provider: StoredProvider,
    preferred_model: Option<String>,
) -> Result<ModelRef> {
    let model_id = preferred_model
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default_model_for(&provider));

    let mut auth = load_auth(data_dir);
    auth.credentials.insert(provider.id.clone(), provider.clone());
    save_auth(data_dir, &auth)?;

    let mut prefs = load_prefs(data_dir);
    let model_ref = ModelRef {
        provider_id: provider.id,
        model_id,
    };
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

// ------------------------------------------------------------------ OAuth PKCE & JWT

#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> Result<Pkce> {
    const CHARSET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut raw = [0u8; 43];
    getrandom::fill(&mut raw).map_err(|e| anyhow!("rng failed: {e}"))?;
    let verifier: String = raw
        .iter()
        .map(|&b| CHARSET[(b as usize) % CHARSET.len()] as char)
        .collect();
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    Ok(Pkce {
        verifier,
        challenge,
    })
}

pub fn random_state() -> Result<String> {
    let mut raw = [0u8; 24];
    getrandom::fill(&mut raw).map_err(|e| anyhow!("rng failed: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(raw))
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn extract_query_param(query: &str, key: &str) -> Option<String> {
    let q = query.split('?').nth(1).unwrap_or(query);
    let q = q.split('#').next().unwrap_or(q);
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn extract_chatgpt_account_id(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let val: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    if let Some(id) = val.get("chatgpt_account_id").and_then(|v| v.as_str()) {
        return Some(id.to_string());
    }
    if let Some(id) = val
        .get("https://api.openai.com/auth")
        .and_then(|o| o.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
    {
        return Some(id.to_string());
    }
    val.get("organizations")
        .and_then(|o| o.as_array())
        .and_then(|a| a.first())
        .and_then(|o| o.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

pub fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// ------------------------------------------------------------------ OpenAI OAuth

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
            let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot found";
            let _ = stream.write_all(resp.as_bytes()).await;
            continue;
        }
        if let Some(err) = extract_query_param(target, "error_description")
            .or_else(|| extract_query_param(target, "error"))
        {
            let body = format!(
                "<html><body><h2>Authorization failed</h2><p>{err}</p></body></html>"
            );
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
        .post(format!("{OPENAI_AUTH_BASE}/api/accounts/deviceauth/usercode"))
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

// ------------------------------------------------------------------ Anthropic OAuth

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
        let state = extract_query_param(trimmed, "state")
            .unwrap_or_else(|| fallback_state.to_string());
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
        updated_ms: now_ms(),
    })
}

// ------------------------------------------------------------------ GitHub Copilot OAuth

#[derive(Debug, Clone)]
pub struct GitHubDeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval_s: u64,
}

pub async fn start_github_copilot_device(http: &reqwest::Client) -> Result<GitHubDeviceStart> {
    let body = form_encode(&[("client_id", GITHUB_COPILOT_CLIENT_ID), ("scope", "read:user")]);
    let resp = http
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", format!("envoy/{}", env!("CARGO_PKG_VERSION")))
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

pub async fn poll_github_copilot_device(
    http: &reqwest::Client,
    start: &GitHubDeviceStart,
) -> Result<StoredProvider> {
    let mut interval = start.interval_s;
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let body = form_encode(&[
            ("client_id", GITHUB_COPILOT_CLIENT_ID),
            ("device_code", &start.device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ]);
        let resp = http
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", format!("envoy/{}", env!("CARGO_PKG_VERSION")))
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

/// Exchange GitHub OAuth token for a Copilot session bearer token and API base URL.
pub async fn copilot_session_token(
    http: &reqwest::Client,
    gh_token: &str,
) -> Result<(String, String)> {
    let resp = http
        .get("https://api.github.com/copilot_internal/v2/token")
        .header("Authorization", format!("token {gh_token}"))
        .header("User-Agent", format!("envoy/{}", env!("CARGO_PKG_VERSION")))
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
    Ok((token, base))
}

// ------------------------------------------------------------------ Token Refresh & Remote Models

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
        auth.credentials.insert(provider.id.clone(), provider.clone());
        let _ = save_auth(data_dir, &auth);
    }
    Ok(provider)
}

pub async fn fetch_openai_models(http: &reqwest::Client, base_url: &str, api_key: &str) -> Vec<String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = http.get(&url);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let Ok(resp) = req.timeout(Duration::from_secs(8)).send().await else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(val) = resp.json::<serde_json::Value>().await else {
        return Vec::new();
    };
    let mut models: Vec<String> = val["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    models.sort();
    models.dedup();
    models
}

// ------------------------------------------------------------------ Interactive CLI `envoy login`

pub async fn cmd_auth_list(data_dir: &Path) -> Result<()> {
    let providers = effective_providers(data_dir);
    let prefs = load_prefs(data_dir);
    if providers.is_empty() {
        println!("No providers connected. Run `envoy login` or type `/login` in the console.");
        return Ok(());
    }
    println!("{:<18} {:<24} {:<10} {}", "ID", "NAME", "METHOD", "CREDENTIAL");
    for prov in providers.values() {
        let active_mark = if prefs
            .active
            .as_ref()
            .is_some_and(|a| a.provider_id == prov.id)
        {
            "*"
        } else {
            " "
        };
        println!(
            "{active_mark}{:<17} {:<24} {:<10} {}",
            prov.id,
            prov.name,
            prov.credential.kind_label(),
            prov.credential.summary()
        );
    }
    if let Some(active) = prefs.active {
        println!("\nActive model: {}", active.display());
    }
    Ok(())
}

pub async fn cmd_auth_logout(data_dir: &Path, provider: Option<String>) -> Result<()> {
    let target = match provider {
        Some(p) => p,
        None => {
            let auth = load_auth(data_dir);
            if auth.credentials.is_empty() {
                println!("No stored credentials.");
                return Ok(());
            }
            let keys: Vec<String> = auth.credentials.keys().cloned().collect();
            for (i, k) in keys.iter().enumerate() {
                println!("  {}. {k}", i + 1);
            }
            let idx = prompt_index("Select provider to remove", keys.len())?;
            keys[idx].clone()
        }
    };
    if remove_provider(data_dir, &target)? {
        println!("Removed credential for `{target}`.");
    } else {
        println!("No stored credential for `{target}`.");
    }
    Ok(())
}

pub async fn cmd_auth_login(data_dir: &Path) -> Result<()> {
    println!("Connect a provider (credentials saved to {})\n", auth_path(data_dir).display());
    for (i, p) in BUILTIN_PROVIDERS.iter().enumerate() {
        println!("  {}. {:<22} — {}", i + 1, p.name, p.subtitle);
    }
    let idx = prompt_index("Select provider", BUILTIN_PROVIDERS.len())?;
    let prov = &BUILTIN_PROVIDERS[idx];

    let method = if prov.methods.len() == 1 {
        prov.methods[0]
    } else {
        println!("\nAuth methods for {}:", prov.name);
        for (i, m) in prov.methods.iter().enumerate() {
            println!("  {}. {} ({})", i + 1, m.label, m.hint);
        }
        let m_idx = prompt_index("Select auth method", prov.methods.len())?;
        prov.methods[m_idx]
    };

    let http = reqwest::Client::new();
    let (stored, chosen_model) = match method.kind {
        AuthMethodKind::OpenAiBrowserOAuth => {
            let pkce = generate_pkce()?;
            let state = random_state()?;
            let (listener, port) = bind_openai_callback_listener().await?;
            let redirect_uri = format!("http://localhost:{port}/auth/callback");
            let url = openai_authorize_url(&redirect_uri, &pkce, &state);
            println!("\nOpen this URL in your browser:\n  {url}\n");
            open_browser(&url);
            println!("Waiting for callback on {redirect_uri}...");
            let (_tx, rx) = oneshot::channel();
            let code = wait_openai_browser_callback(listener, state, rx).await?;
            let sp = exchange_openai_code(&http, &code, &redirect_uri, &pkce.verifier, "chatgpt-browser").await?;
            (sp, None)
        }
        AuthMethodKind::OpenAiDeviceOAuth => {
            let start = start_openai_device_auth(&http).await?;
            println!("\nOpen: {}\nCode: {}\n", start.verification_url, start.user_code);
            open_browser(&start.verification_url);
            println!("Waiting for authorization...");
            let sp = poll_openai_device_auth(&http, &start).await?;
            (sp, None)
        }
        AuthMethodKind::AnthropicClaudeOAuth | AuthMethodKind::AnthropicConsoleOAuth => {
            let console = matches!(method.kind, AuthMethodKind::AnthropicConsoleOAuth);
            let pkce = generate_pkce()?;
            let state = random_state()?;
            let url = anthropic_authorize_url(console, &pkce, &state);
            println!("\nOpen this URL in your browser:\n  {url}\n");
            open_browser(&url);
            let pasted = prompt_line("Paste the code#state (or redirect URL)")?;
            let sp = exchange_anthropic_code(&http, &pasted, &pkce.verifier, &state, console).await?;
            (sp, None)
        }
        AuthMethodKind::GitHubCopilotDeviceOAuth => {
            let start = start_github_copilot_device(&http).await?;
            println!("\nOpen: {}\nCode: {}\n", start.verification_uri, start.user_code);
            open_browser(&start.verification_uri);
            println!("Waiting for authorization...");
            let sp = poll_github_copilot_device(&http, &start).await?;
            (sp, None)
        }
        AuthMethodKind::OpenAiApiKey | AuthMethodKind::AnthropicApiKey | AuthMethodKind::SimpleApiKey => {
            let key = prompt_line(&format!("Enter {} API key", prov.name))?;
            if key.trim().is_empty() {
                bail!("API key cannot be empty");
            }
            let base = prompt_line_default("Base URL", prov.default_base_url)?;
            let sp = StoredProvider {
                id: prov.id.to_string(),
                name: prov.name.to_string(),
                protocol: prov.protocol,
                base_url: base,
                credential: Credential::ApiKey {
                    key: key.trim().to_string(),
                },
                custom_models: Vec::new(),
                updated_ms: now_ms(),
            };
            (sp, None)
        }
        AuthMethodKind::CustomForm => {
            let name = prompt_line_default("Provider name", "Custom OpenAI")?;
            let proto_choice = prompt_line_default("Protocol (1=OpenAI, 2=Anthropic)", "1")?;
            let protocol = if proto_choice.trim() == "2" {
                ProviderProtocol::Anthropic
            } else {
                ProviderProtocol::OpenAiChat
            };
            let default_url = if matches!(protocol, ProviderProtocol::Anthropic) {
                "https://api.anthropic.com/v1"
            } else {
                "http://127.0.0.1:11434/v1"
            };
            let base_url = prompt_line_default("Base URL", default_url)?;
            let key = prompt_line("API Key (optional, press Enter to skip)")?;
            let model = prompt_line("Default Model ID")?;
            let id = slugify_provider_id(&name);
            let custom_models = if model.trim().is_empty() {
                Vec::new()
            } else {
                vec![model.trim().to_string()]
            };
            let sp = StoredProvider {
                id,
                name,
                protocol,
                base_url: base_url.trim_end_matches('/').to_string(),
                credential: Credential::ApiKey {
                    key: key.trim().to_string(),
                },
                custom_models,
                updated_ms: now_ms(),
            };
            (sp, if model.trim().is_empty() { None } else { Some(model.trim().to_string()) })
        }
    };

    let model_ref = record_provider_login(data_dir, stored, chosen_model)?;
    println!("\nConnected! Active model: {}", model_ref.display());
    Ok(())
}

fn prompt_line(label: &str) -> Result<String> {
    print!("{label}: ");
    std::io::stdout().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(buf.trim_end_matches(&['\r', '\n'][..]).to_string())
}

fn prompt_line_default(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    std::io::stdout().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn prompt_index(label: &str, len: usize) -> Result<usize> {
    loop {
        let ans = prompt_line(&format!("{label} [1-{len}]"))?;
        if let Ok(n) = ans.trim().parse::<usize>() {
            if n >= 1 && n <= len {
                return Ok(n - 1);
            }
        }
        println!("Please enter a number between 1 and {len}.");
    }
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
    use super::*;

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
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_12345"}}"#,
        );
        let jwt = format!("{header}.{payload}.sig");
        assert_eq!(extract_chatgpt_account_id(&jwt).as_deref(), Some("acct_12345"));
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
            updated_ms: now_ms(),
        };
        let model_ref = record_provider_login(&dir, sp, None).unwrap();
        assert_eq!(model_ref.display(), "deepseek/deepseek-chat");

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
        assert_eq!(active_model.model_id, "deepseek-chat");

        assert!(remove_provider(&dir, "deepseek").unwrap());
        assert!(load_auth(&dir).credentials.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
