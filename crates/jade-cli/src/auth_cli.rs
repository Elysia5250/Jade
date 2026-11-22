//! Interactive `jade login`, `jade auth list`, and `jade auth logout`.

use anyhow::{bail, Result};
use jade_agent::auth::{
    anthropic_authorize_url, auth_path, bind_openai_callback_listener, effective_providers,
    exchange_anthropic_code_default, exchange_openai_code_default, generate_pkce, load_auth,
    load_prefs, now_ms, open_browser, openai_authorize_url, poll_github_copilot_device_default,
    poll_openai_device_auth_default, random_state, record_provider_login, remove_provider,
    slugify_provider_id, start_github_copilot_device_default, start_openai_device_auth_default,
    wait_openai_browser_callback, AuthMethodKind, Credential, ProviderProtocol, StoredProvider,
    BUILTIN_PROVIDERS,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use tokio::sync::oneshot;

pub(crate) async fn cmd_auth_list(data_dir: &Path) -> Result<()> {
    let providers = effective_providers(data_dir);
    let prefs = load_prefs(data_dir);
    if providers.is_empty() {
        println!("No providers connected. Run `jade login` or type `/login` in the console.");
        return Ok(());
    }
    println!("{:<18} {:<24} {:<10} CREDENTIAL", "ID", "NAME", "METHOD");
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

pub(crate) async fn cmd_auth_logout(data_dir: &Path, provider: Option<String>) -> Result<()> {
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

pub(crate) async fn cmd_auth_login(data_dir: &Path) -> Result<()> {
    println!(
        "Connect a provider (credentials saved to {})\n",
        auth_path(data_dir).display()
    );
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
            let sp = exchange_openai_code_default(
                &code,
                &redirect_uri,
                &pkce.verifier,
                "chatgpt-browser",
            )
            .await?;
            (sp, None)
        }
        AuthMethodKind::OpenAiDeviceOAuth => {
            let start = start_openai_device_auth_default().await?;
            println!(
                "\nOpen: {}\nCode: {}\n",
                start.verification_url, start.user_code
            );
            open_browser(&start.verification_url);
            println!("Waiting for authorization...");
            let sp = poll_openai_device_auth_default(&start).await?;
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
            let sp =
                exchange_anthropic_code_default(&pasted, &pkce.verifier, &state, console).await?;
            (sp, None)
        }
        AuthMethodKind::GitHubCopilotDeviceOAuth => {
            let start = start_github_copilot_device_default().await?;
            println!(
                "\nOpen: {}\nCode: {}\n",
                start.verification_uri, start.user_code
            );
            open_browser(&start.verification_uri);
            println!("Waiting for authorization...");
            let sp = poll_github_copilot_device_default(&start).await?;
            (sp, None)
        }
        AuthMethodKind::OpenAiApiKey
        | AuthMethodKind::AnthropicApiKey
        | AuthMethodKind::SimpleApiKey => {
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
                fetched_models: Vec::new(),
                model_routes: BTreeMap::new(),
                extra_headers: Default::default(),
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
                fetched_models: Vec::new(),
                model_routes: BTreeMap::new(),
                extra_headers: Default::default(),
                updated_ms: now_ms(),
            };
            (
                sp,
                if model.trim().is_empty() {
                    None
                } else {
                    Some(model.trim().to_string())
                },
            )
        }
    };

    let model_ref = record_provider_login(data_dir, stored, chosen_model)?;
    println!(
        "\nConnected. Active model: {}",
        if model_ref.model_id.is_empty() {
            "(fetch with /models)".to_string()
        } else {
            model_ref.display()
        }
    );
    Ok(())
}

pub(crate) fn cmd_auth_add(
    data_dir: &Path,
    id: String,
    base_url: String,
    protocol: String,
    api_key: String,
    models: Vec<String>,
    headers: Vec<String>,
) -> Result<()> {
    let clean_id = slugify_provider_id(&id);
    let proto = match protocol.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "anthropic_messages" => ProviderProtocol::Anthropic,
        "codex" | "openai-codex" | "openai_codex" => ProviderProtocol::OpenAiCodex,
        "openai" | "openai-chat" | "openai_chat" | "openai-compatible" => {
            ProviderProtocol::OpenAiChat
        }
        other => bail!("unknown protocol `{other}` (expected: openai, anthropic, codex)"),
    };
    let mut extra_headers = BTreeMap::new();
    for raw in headers {
        let Some((k, v)) = raw.split_once('=').or_else(|| raw.split_once(':')) else {
            bail!("invalid --header `{raw}` (expected Key=Value)");
        };
        let k = k.trim();
        let v = v.trim();
        if k.is_empty() || v.is_empty() {
            bail!("invalid --header `{raw}` (empty key or value)");
        }
        extra_headers.insert(k.to_string(), v.to_string());
    }
    let chosen = models.first().cloned();
    let stored = StoredProvider {
        id: clean_id.clone(),
        name: id,
        protocol: proto,
        base_url: base_url.trim_end_matches('/').to_string(),
        credential: Credential::ApiKey {
            key: api_key.trim().to_string(),
        },
        custom_models: models,
        fetched_models: Vec::new(),
        model_routes: BTreeMap::new(),
        extra_headers,
        updated_ms: now_ms(),
    };
    let model_ref = record_provider_login(data_dir, stored, chosen)?;
    println!("Added provider `{clean_id}` (active: {})", model_ref.display());
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
