//! Model context-window discovery, caching, and 2026 family defaults.
//!
//! Resolution order for a model's physical (`hard_window`) spec:
//! 1. User explicit override (`model.json` / `model_caps.json` source="override")
//! 2. Runtime `/models` metadata or `models.dev` catalog cache
//! 3. Explicit size suffix in model ID (e.g., `-1m`, `-256k`, `:128k`)
//! 4. Updated 2026 family table
//! 5. Conservative 64k fallback for unknown local/relay models

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, SystemTime};

const UNKNOWN_FALLBACK_WINDOW: u64 = 65_536;
const MODELS_DEV_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// Family-table lookup. The table lives in `families` so this file stays under the size cap.
pub fn builtin_model_cap(model_id: &str) -> Option<ModelCap> {
    super::families::builtin_model_cap(model_id)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCap {
    pub context_window: u64,
    #[serde(default)]
    pub max_output: Option<u64>,
    #[serde(default)]
    pub pricing_cliff: Option<u64>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub updated_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapsCache {
    #[serde(default)]
    pub models_dev_fetched_ms: u64,
    #[serde(default)]
    pub models: BTreeMap<String, ModelCap>,
    /// Thinking levels from `models.dev` `reasoning_options`, keyed like `models`.
    #[serde(default)]
    pub reasoning: BTreeMap<String, super::reasoning::ReasoningSpec>,
}

pub fn caps_path(data_dir: &Path) -> PathBuf {
    data_dir.join("model_caps.json")
}

type CapsMemEntry = (Option<SystemTime>, u64, CapsCache);

fn caps_mem() -> &'static RwLock<HashMap<PathBuf, CapsMemEntry>> {
    static CAPS_MEM: OnceLock<RwLock<HashMap<PathBuf, CapsMemEntry>>> = OnceLock::new();
    CAPS_MEM.get_or_init(|| RwLock::new(HashMap::new()))
}

pub fn load_caps(data_dir: &Path) -> CapsCache {
    let path = caps_path(data_dir);
    let Ok(meta) = std::fs::metadata(&path) else {
        if let Ok(mut guard) = caps_mem().write() {
            guard.remove(&path);
        }
        return CapsCache::default();
    };
    let stamp = (meta.modified().ok(), meta.len());
    if let Ok(guard) = caps_mem().read() {
        if let Some((m, l, cached)) = guard.get(&path) {
            if *m == stamp.0 && *l == stamp.1 {
                return cached.clone();
            }
        }
    }
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return CapsCache::default();
    };
    let parsed: CapsCache = serde_json::from_str(&raw).unwrap_or_default();
    if let Ok(mut guard) = caps_mem().write() {
        guard.insert(path, (stamp.0, stamp.1, parsed.clone()));
    }
    parsed
}

pub fn save_caps(data_dir: &Path, cache: &CapsCache) -> anyhow::Result<()> {
    jade_core::fs::ensure_private_dir(data_dir)?;
    let path = caps_path(data_dir);
    let data = serde_json::to_string_pretty(cache)?;
    jade_core::fs::write_atomic_sync(&path, data.as_bytes(), 0o600)?;
    if let Ok(meta) = std::fs::metadata(&path) {
        if let Ok(mut guard) = caps_mem().write() {
            guard.insert(path, (meta.modified().ok(), meta.len(), cache.clone()));
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    jade_core::now_ms()
}

pub fn cap_key(provider_id: &str, model_id: &str) -> String {
    format!("{provider_id}/{model_id}")
}

/// Resolve physical model capabilities from cache, suffixes, or 2026 family defaults.
pub fn resolve_model_cap(data_dir: &Path, provider_id: &str, model_id: &str) -> ModelCap {
    let cache = load_caps(data_dir);
    let exact = cap_key(provider_id, model_id);
    if let Some(cap) = cache.models.get(&exact) {
        if cap.context_window > 0 {
            return cap.clone();
        }
    }
    if let Some(cap) = cache.models.get(model_id) {
        if cap.context_window > 0 {
            return cap.clone();
        }
    }
    if let Some(window) = parse_size_suffix(model_id) {
        return ModelCap {
            context_window: window,
            max_output: None,
            pricing_cliff: builtin_model_cap(model_id).and_then(|c| c.pricing_cliff),
            source: "suffix".into(),
            updated_ms: 0,
        };
    }
    if let Some(cap) = builtin_model_cap(model_id) {
        return cap;
    }
    ModelCap {
        context_window: UNKNOWN_FALLBACK_WINDOW,
        max_output: None,
        pricing_cliff: None,
        source: "fallback".into(),
        updated_ms: 0,
    }
}

/// Convenience helper for unit tests that only have a model ID.
#[cfg(test)]
fn context_window(model_id: &str) -> u64 {
    if let Some(w) = parse_size_suffix(model_id) {
        return w;
    }
    builtin_model_cap(model_id)
        .map(|c| c.context_window)
        .unwrap_or(UNKNOWN_FALLBACK_WINDOW)
}

/// Extract explicit context sizes like `-32k`, `:128k`, `-200k`, `-1m`, `[1m]`.
pub fn parse_size_suffix(model_id: &str) -> Option<u64> {
    let lower = model_id.to_ascii_lowercase();
    for token in lower.split(['-', '_', ':', '/', '[', ']', ' ']) {
        if token.len() < 2 {
            continue;
        }
        let (num_str, mult) = if let Some(n) = token.strip_suffix('k') {
            (n, 1_000u64)
        } else if let Some(n) = token.strip_suffix('m') {
            (n, 1_000_000u64)
        } else {
            continue;
        };
        if !num_str.is_empty() && num_str.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(n) = num_str.parse::<u64>() {
                let total = n.saturating_mul(mult);
                if (4_096..=4_000_000).contains(&total) {
                    return Some(total);
                }
            }
        }
    }
    None
}

/// Record a user or error-discovered override into `model_caps.json`.
pub fn record_model_cap(
    data_dir: &Path,
    provider_id: &str,
    model_id: &str,
    context_window: u64,
    source: &str,
) {
    let mut cache = load_caps(data_dir);
    let existing = resolve_model_cap(data_dir, provider_id, model_id);
    if existing.source == "override" && source != "override" {
        return;
    }
    cache.models.insert(
        cap_key(provider_id, model_id),
        ModelCap {
            context_window,
            max_output: existing.max_output,
            pricing_cliff: existing.pricing_cliff,
            source: source.into(),
            updated_ms: now_ms(),
        },
    );
    let _ = save_caps(data_dir, &cache);
}

/// One model advertised by a provider's `/models` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedModel {
    pub id: String,
    pub route: Option<crate::auth::ModelRoute>,
}

/// Parse an OpenAI-style `{ "data": [ { "id", "api"? } ] }` document.
/// An empty `api` object keeps the provider default. A non-empty block with no
/// chat, messages, or responses URL we can call is omitted.
pub fn parse_listed_models(value: &Value) -> Vec<ListedModel> {
    let Some(arr) = value["data"].as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        let Some(id) = item["id"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        match item.get("api").and_then(Value::as_object) {
            Some(obj) if obj.is_empty() => out.push(ListedModel {
                id: id.to_string(),
                route: None,
            }),
            Some(_) => {
                let Some(route) = route_from_api_block(&item["api"]) else {
                    continue;
                };
                out.push(ListedModel {
                    id: id.to_string(),
                    route: Some(route),
                });
            }
            None => out.push(ListedModel {
                id: id.to_string(),
                route: None,
            }),
        }
    }
    out
}

fn absolute_http_url(base: &str) -> bool {
    reqwest::Url::parse(base.trim())
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
}

fn http_route_base(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    let base = obj.get(key)?.get("base_url")?.as_str()?.trim();
    absolute_http_url(base).then(|| base.trim_end_matches('/').to_string())
}

fn route_from_api_block(api: &Value) -> Option<crate::auth::ModelRoute> {
    let obj = api.as_object()?;
    if let Some(base_url) = http_route_base(obj, "chat-completions") {
        return Some(crate::auth::ModelRoute {
            protocol: crate::auth::ProviderProtocol::OpenAiChat,
            base_url,
        });
    }
    if let Some(base_url) = http_route_base(obj, "messages") {
        return Some(crate::auth::ModelRoute {
            protocol: crate::auth::ProviderProtocol::Anthropic,
            base_url,
        });
    }
    if let Some(base) = http_route_base(obj, "responses") {
        let base_url = if base.ends_with("/responses") {
            base
        } else {
            format!("{base}/responses")
        };
        return Some(crate::auth::ModelRoute {
            protocol: crate::auth::ProviderProtocol::OpenAiCodex,
            base_url,
        });
    }
    None
}

/// Drop ids the provider still lists but no longer accepts.
pub fn usable_listed_models(provider_id: &str, mut models: Vec<ListedModel>) -> Vec<ListedModel> {
    models.retain(|m| !crate::auth::is_retired_model(provider_id, &m.id));
    let mut seen = Vec::new();
    models.retain(|m| {
        if seen.iter().any(|id: &String| id == &m.id) {
            return false;
        }
        seen.push(m.id.clone());
        true
    });
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

fn preferred_listed_id(provider_id: &str, ids: &[String]) -> Option<String> {
    if provider_id == "deepseek" {
        if let Some(id) = ids.iter().find(|id| id.as_str() == "deepseek-flash") {
            return Some(id.clone());
        }
    }
    ids.first().cloned()
}

/// Save a fetched list onto the provider and point the active model at a live id.
pub fn store_listed_models(
    data_dir: &Path,
    provider_id: &str,
    listed: &[ListedModel],
) -> anyhow::Result<Option<crate::auth::ModelRef>> {
    use crate::auth::{load_auth, load_prefs, save_auth, save_prefs, ModelRef};
    if listed.is_empty() {
        return Ok(None);
    }
    let mut auth = load_auth(data_dir);
    let Some(prov) = auth.credentials.get_mut(provider_id) else {
        return Ok(None);
    };
    let mut ids = Vec::new();
    prov.model_routes.clear();
    for model in listed {
        if ids.iter().any(|id: &String| id == &model.id) {
            continue;
        }
        ids.push(model.id.clone());
        if let Some(route) = model
            .route
            .clone()
            .filter(|route| absolute_http_url(&route.base_url))
        {
            prov.model_routes.insert(model.id.clone(), route);
        }
    }
    prov.fetched_models = ids.clone();
    let all_models = prov.all_models();
    save_auth(data_dir, &auth)?;

    let mut prefs = load_prefs(data_dir);
    let points_here = prefs
        .active
        .as_ref()
        .is_some_and(|active| active.provider_id == provider_id);
    if !points_here && prefs.active.is_some() {
        return Ok(None);
    }
    let keep = prefs.active.as_ref().is_some_and(|active| {
        !active.model_id.is_empty()
            && !crate::auth::is_retired_model(&active.provider_id, &active.model_id)
            && all_models.iter().any(|id| id == &active.model_id)
    });
    let chosen = if keep {
        prefs.active.clone()
    } else {
        preferred_listed_id(provider_id, &ids).map(|model_id| ModelRef {
            provider_id: provider_id.to_string(),
            model_id,
        })
    };
    if let Some(mref) = &chosen {
        prefs.active = Some(mref.clone());
        prefs.recent.retain(|r| r != mref);
        prefs.recent.insert(0, mref.clone());
        prefs.recent.truncate(12);
        save_prefs(data_dir, &prefs)?;
    }
    Ok(chosen)
}

/// Fetch `/models`, persist advertised context limits, and return callable ids.
pub async fn fetch_listed_models(
    http: &reqwest::Client,
    data_dir: Option<&Path>,
    provider_id: &str,
    protocol: crate::auth::ProviderProtocol,
    base_url: &str,
    api_key: &str,
) -> Vec<ListedModel> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = http.get(&url);
    if !api_key.is_empty() {
        req = match protocol {
            crate::auth::ProviderProtocol::Anthropic => req
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01"),
            _ => req.bearer_auth(api_key),
        };
    }
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
    usable_listed_models(provider_id, parse_listed_models(&val))
}

pub(crate) fn remember_caps_from_models(data_dir: Option<&Path>, provider_id: &str, val: &Value) {
    let Some(arr) = val["data"].as_array().or_else(|| val["models"].as_array()) else {
        return;
    };
    let mut discovered = Vec::new();
    let mut reasoning = Vec::new();
    for item in arr {
        let Some(id) = item["id"].as_str().or_else(|| item["slug"].as_str()) else {
            continue;
        };
        if let Some(ctx) = extract_context_from_model_json(item) {
            let max_out = extract_output_from_model_json(item);
            discovered.push((id.to_string(), ctx, max_out));
        }
        if let Some(spec) = super::reasoning::spec_from_model(item) {
            reasoning.push((id.to_string(), spec));
        }
    }
    let Some(dir) = data_dir.filter(|_| !discovered.is_empty() || !reasoning.is_empty()) else {
        return;
    };
    let mut cache = load_caps(dir);
    for (id, spec) in reasoning {
        super::reasoning::record(&mut cache, provider_id, &id, spec);
    }
    let ts = now_ms();
    for (id, ctx, max_out) in discovered {
        let key = cap_key(provider_id, &id);
        if cache
            .models
            .get(&key)
            .is_some_and(|c| c.source == "override")
        {
            continue;
        }
        cache.models.insert(
            key,
            ModelCap {
                context_window: ctx,
                max_output: max_out,
                pricing_cliff: builtin_model_cap(&id).and_then(|c| c.pricing_cliff),
                source: "api".into(),
                updated_ms: ts,
            },
        );
    }
    let _ = save_caps(dir, &cache);
}

fn extract_context_from_model_json(item: &Value) -> Option<u64> {
    for key in [
        "context_length",
        "context_window",
        "max_model_len",
        "max_input_tokens",
        "input_token_limit",
    ] {
        if let Some(v) = item[key].as_u64().filter(|&n| n >= 4_096) {
            return Some(v);
        }
    }
    item["limits"]["context"]
        .as_u64()
        .or_else(|| item["top_provider"]["context_length"].as_u64())
        .filter(|&n| n >= 4_096)
}

fn extract_output_from_model_json(item: &Value) -> Option<u64> {
    for key in [
        "max_output_tokens",
        "max_completion_tokens",
        "output_token_limit",
    ] {
        if let Some(v) = item[key].as_u64().filter(|&n| n > 0) {
            return Some(v);
        }
    }
    item["top_provider"]["max_completion_tokens"]
        .as_u64()
        .filter(|&n| n > 0)
}

/// Best-effort background refresh of `https://models.dev/api.json` (24h TTL).
pub async fn refresh_models_dev_if_stale(http: &reqwest::Client, data_dir: &Path) {
    let cache = load_caps(data_dir);
    let ts = now_ms();
    if cache.models_dev_fetched_ms > 0
        && ts.saturating_sub(cache.models_dev_fetched_ms) < MODELS_DEV_TTL_MS
    {
        return;
    }
    let Ok(resp) = http
        .get("https://models.dev/api.json")
        .timeout(Duration::from_secs(6))
        .send()
        .await
    else {
        return;
    };
    if !resp.status().is_success() {
        return;
    }
    let Ok(root) = resp.json::<Value>().await else {
        return;
    };
    let Some(providers) = root.as_object() else {
        return;
    };
    let mut cache = load_caps(data_dir);
    let ts = now_ms();
    cache.models_dev_fetched_ms = ts;
    for (prov_id, prov_val) in providers {
        let Some(models_obj) = prov_val["models"].as_object() else {
            continue;
        };
        for (model_id, m) in models_obj {
            if let Some(spec) = super::reasoning::spec_from_model(m) {
                super::reasoning::record(&mut cache, prov_id, model_id, spec);
            }
            let Some(ctx) = m["limit"]["context"].as_u64().filter(|&n| n >= 4_096) else {
                continue;
            };
            let max_out = m["limit"]["output"].as_u64().filter(|&n| n > 0);
            let cap = ModelCap {
                context_window: ctx,
                max_output: max_out,
                pricing_cliff: builtin_model_cap(model_id).and_then(|c| c.pricing_cliff),
                source: "models.dev".into(),
                updated_ms: ts,
            };
            for key in [cap_key(prov_id, model_id), model_id.clone()] {
                if cache
                    .models
                    .get(&key)
                    .is_none_or(|c| c.source != "override" && c.source != "api")
                {
                    cache.models.insert(key, cap.clone());
                }
            }
        }
    }
    let _ = save_caps(data_dir, &cache);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_2026_flagships_and_suffixes() {
        assert_eq!(context_window("claude-fable-5-2"), 1_000_000);
        assert_eq!(context_window("claude-sonnet-5-5"), 1_000_000);
        assert_eq!(context_window("claude-sonnet-4-5"), 200_000);
        assert_eq!(context_window("gemini-4-carbon"), 1_048_576);
        assert_eq!(context_window("gemini-4-hydrogen"), 1_048_576);
        assert_eq!(context_window("gpt-6.1-sol"), 1_050_000);
        assert_eq!(context_window("gpt-6-astra"), 1_050_000);
        assert_eq!(context_window("gpt-4.1"), 1_047_576);
        assert_eq!(context_window("deepseek-v4.1-flash"), 1_000_000);
        assert_eq!(context_window("deepseek-reasoner"), 65_536);
        assert_eq!(context_window("grok-4.7"), 500_000);
        assert_eq!(context_window("qwen3.8-flash-next"), 262_144);
        assert_eq!(context_window("qwen4-max"), 1_000_000);
        assert_eq!(context_window("kimi-k3.1-pro"), 1_048_576);
        assert_eq!(context_window("glm-5.3"), 1_048_576);
        assert_eq!(context_window("my-local-model-32k"), 32_000);
        assert_eq!(context_window("unknown-relay-model"), 65_536);
    }

    #[test]
    fn drops_retired_deepseek_ids_and_keeps_live_ones() {
        let raw = serde_json::json!({
            "data": [
                {"id": "deepseek-chat"},
                {"id": "deepseek-v4.1-flash"},
                {"id": "deepseek-reasoner"},
                {"id": "deepseek-flash"},
                {"id": "deepseek-v4-pro"},
                {"id": "deepseek-chat"}
            ]
        });
        let usable = usable_listed_models("deepseek", parse_listed_models(&raw));
        let ids: Vec<&str> = usable.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "deepseek-chat",
                "deepseek-flash",
                "deepseek-reasoner",
                "deepseek-v4-pro",
            ]
        );
    }

    #[test]
    fn routes_opencode_models_by_api_block() {
        let raw = serde_json::json!({
            "data": [
                {"id": "glm-5", "api": {"chat-completions": {"base_url": "https://opencode.ai/inference/openai/v1"}}},
                {"id": "claude-haiku-5-5", "api": {"messages": {"base_url": "https://opencode.ai/inference/anthropic/v1"}}},
                {"id": "gpt-6-luna", "api": {"responses": {"base_url": "https://opencode.ai/inference/go/openai/v1"}}},
                {"id": "gemini-4-argon", "api": {"generate-content": {"base_url": "https://opencode.ai/inference/google/v1beta"}}}
            ]
        });
        let listed = parse_listed_models(&raw);
        let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["glm-5", "claude-haiku-5-5", "gpt-6-luna"]);
        assert_eq!(
            listed[2].route.as_ref().map(|r| r.base_url.as_str()),
            Some("https://opencode.ai/inference/go/openai/v1/responses")
        );
        assert!(matches!(
            listed[1].route.as_ref().map(|r| r.protocol),
            Some(crate::auth::ProviderProtocol::Anthropic)
        ));
    }

    #[test]
    fn stored_list_becomes_the_active_model() {
        let dir = std::env::temp_dir().join(format!("jade-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sp = crate::auth::StoredProvider {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            protocol: crate::auth::ProviderProtocol::OpenAiChat,
            base_url: "https://api.deepseek.com/v1".into(),
            credential: crate::auth::Credential::ApiKey { key: "sk".into() },
            custom_models: vec!["deepseek-v4.1-flash".into()],
            fetched_models: Vec::new(),
            model_routes: BTreeMap::new(),
            extra_headers: Default::default(),
            updated_ms: 0,
        };
        crate::auth::record_provider_login(&dir, sp, Some("deepseek-v4.1-flash".into())).unwrap();
        let listed = usable_listed_models(
            "deepseek",
            parse_listed_models(&serde_json::json!({
                "data": [
                    {"id": "deepseek-v4.1-flash"},
                    {"id": "deepseek-v4-pro"},
                    {"id": "deepseek-flash"}
                ]
            })),
        );
        let active = store_listed_models(&dir, "deepseek", &listed)
            .unwrap()
            .unwrap();
        assert_eq!(active.model_id, "deepseek-flash");

        // Live custom_models stay intact and keep active pin on subsequent refresh.
        let mut auth = crate::auth::load_auth(&dir);
        auth.credentials
            .get_mut("deepseek")
            .unwrap()
            .custom_models
            .push("my-custom-pinned".into());
        crate::auth::save_auth(&dir, &auth).unwrap();
        crate::auth::set_active_model(
            &dir,
            crate::auth::ModelRef {
                provider_id: "deepseek".into(),
                model_id: "my-custom-pinned".into(),
            },
        )
        .unwrap();
        let kept = store_listed_models(&dir, "deepseek", &listed)
            .unwrap()
            .unwrap();
        assert_eq!(kept.model_id, "my-custom-pinned");
        let ids: Vec<_> = crate::auth::available_models(&dir)
            .into_iter()
            .map(|m| m.model_id)
            .collect();
        assert!(ids.contains(&"my-custom-pinned".to_string()));
        assert!(ids.contains(&"deepseek-flash".to_string()));
        assert!(ids.contains(&"deepseek-v4-pro".to_string()));
        assert!(!ids.contains(&"deepseek-v4.1-flash".to_string()));

        // Codex {"models":[{"slug":...}]} also writes caps into model_caps.json
        let codex_doc = serde_json::json!({
            "models": [{
                "slug": "gpt-codex-slug",
                "context_window": 320_000,
                "max_output_tokens": 64_000
            }]
        });
        remember_caps_from_models(Some(&dir), "openai", &codex_doc);
        let cap = resolve_model_cap(&dir, "openai", "gpt-codex-slug");
        assert_eq!(cap.context_window, 320_000);
        assert_eq!(cap.source, "api");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
