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
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const UNKNOWN_FALLBACK_WINDOW: u64 = 65_536;
const MODELS_DEV_TTL_MS: u64 = 24 * 60 * 60 * 1000;

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
}

pub fn caps_path(data_dir: &Path) -> PathBuf {
    data_dir.join("model_caps.json")
}

pub fn load_caps(data_dir: &Path) -> CapsCache {
    let Ok(raw) = std::fs::read_to_string(caps_path(data_dir)) else {
        return CapsCache::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save_caps(data_dir: &Path, cache: &CapsCache) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let tmp_dir = data_dir.join("tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp = tmp_dir.join("model_caps.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(cache)?)?;
    std::fs::rename(&tmp, caps_path(data_dir))?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    for token in lower.split(|c: char| matches!(c, '-' | '_' | ':' | '/' | '[' | ']' | ' ')) {
        if token.len() < 2 {
            continue;
        }
        let (num_str, mult) = if let Some(n) = token.strip_suffix('k') {
            (n, 1_024u64)
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

/// Built-in 2026-11 model family capability table. Longest fragment wins.
pub fn builtin_model_cap(model_id: &str) -> Option<ModelCap> {
    let id = model_id.to_ascii_lowercase();
    struct Rule {
        frag: &'static str,
        window: u64,
        max_out: Option<u64>,
        cliff: Option<u64>,
    }
    const TABLE: &[Rule] = &[
        // Anthropic Claude 5 / Fable / Mythos (1M context, 128K output)
        Rule { frag: "claude-fable", window: 1_000_000, max_out: Some(128_000), cliff: None },
        Rule { frag: "claude-mythos", window: 1_000_000, max_out: Some(128_000), cliff: None },
        Rule { frag: "claude-opus-5", window: 1_000_000, max_out: Some(128_000), cliff: None },
        Rule { frag: "claude-sonnet-5", window: 1_000_000, max_out: Some(128_000), cliff: None },
        Rule { frag: "claude-haiku-5", window: 1_000_000, max_out: Some(128_000), cliff: None },
        // Anthropic Claude 3.x / 4.x (200K context)
        Rule { frag: "claude", window: 200_000, max_out: Some(64_000), cliff: None },
        // Google Gemini 4 / 2.5 / 3 (1M unified context)
        Rule { frag: "gemini-4", window: 1_048_576, max_out: Some(65_536), cliff: None },
        Rule { frag: "gemini", window: 1_048_576, max_out: Some(65_536), cliff: None },
        // OpenAI GPT-6 / GPT-5.5+ (1.05M raw, 272K billing cliff)
        Rule { frag: "gpt-6", window: 1_050_000, max_out: Some(128_000), cliff: Some(272_000) },
        Rule { frag: "gpt-5.6", window: 1_050_000, max_out: Some(128_000), cliff: Some(272_000) },
        Rule { frag: "gpt-5.5", window: 1_050_000, max_out: Some(128_000), cliff: Some(272_000) },
        Rule { frag: "gpt-5", window: 400_000, max_out: Some(128_000), cliff: Some(272_000) },
        Rule { frag: "gpt-4.1", window: 1_047_576, max_out: Some(32_768), cliff: None },
        Rule { frag: "gpt-4o", window: 128_000, max_out: Some(16_384), cliff: None },
        Rule { frag: "gpt-4-turbo", window: 128_000, max_out: Some(4_096), cliff: None },
        Rule { frag: "o1", window: 200_000, max_out: Some(100_000), cliff: None },
        Rule { frag: "o3", window: 200_000, max_out: Some(100_000), cliff: None },
        Rule { frag: "o4", window: 200_000, max_out: Some(100_000), cliff: None },
        // DeepSeek V4 (1M) vs legacy R1/Chat
        Rule { frag: "deepseek-v4", window: 1_000_000, max_out: Some(384_000), cliff: None },
        Rule { frag: "deepseek-flash", window: 1_000_000, max_out: Some(384_000), cliff: None },
        Rule { frag: "deepseek-reasoner", window: 65_536, max_out: Some(32_768), cliff: None },
        Rule { frag: "deepseek", window: 131_072, max_out: Some(32_768), cliff: None },
        // Qwen4 / Qwen3.8
        Rule { frag: "qwen4", window: 1_000_000, max_out: Some(262_144), cliff: None },
        Rule { frag: "qwen3.8-max", window: 1_000_000, max_out: Some(131_072), cliff: None },
        Rule { frag: "qwen3.8-flash-next", window: 262_144, max_out: Some(131_072), cliff: None },
        Rule { frag: "qwen3.8-flash", window: 1_000_000, max_out: Some(131_072), cliff: None },
        Rule { frag: "qwen3.8-2.4t", window: 262_144, max_out: Some(131_072), cliff: None },
        Rule { frag: "qwen", window: 131_072, max_out: Some(32_768), cliff: None },
        // xAI Grok
        Rule { frag: "grok-4.7", window: 500_000, max_out: None, cliff: Some(200_000) },
        Rule { frag: "grok-4.6", window: 500_000, max_out: None, cliff: Some(200_000) },
        Rule { frag: "grok-4.5", window: 500_000, max_out: None, cliff: Some(200_000) },
        Rule { frag: "grok-4.3", window: 1_000_000, max_out: None, cliff: Some(200_000) },
        Rule { frag: "grok-4.20", window: 1_000_000, max_out: None, cliff: Some(200_000) },
        Rule { frag: "grok-build", window: 256_000, max_out: None, cliff: None },
        Rule { frag: "grok", window: 131_072, max_out: None, cliff: None },
        // Moonshot Kimi & Z.ai GLM
        Rule { frag: "kimi-k3", window: 1_048_576, max_out: Some(128_000), cliff: None },
        Rule { frag: "kimi", window: 131_072, max_out: None, cliff: None },
        Rule { frag: "glm-5", window: 1_048_576, max_out: Some(131_072), cliff: None },
        Rule { frag: "glm", window: 131_072, max_out: None, cliff: None },
        Rule { frag: "llama", window: 128_000, max_out: None, cliff: None },
    ];
    TABLE
        .iter()
        .filter(|r| id.contains(r.frag))
        .max_by_key(|r| r.frag.len())
        .map(|r| ModelCap {
            context_window: r.window,
            max_output: r.max_out,
            pricing_cliff: r.cliff,
            source: "builtin".into(),
            updated_ms: 0,
        })
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

/// Fetch OpenAI-compatible `/models`, return sorted IDs, and persist any
/// advertised context/output limits into `model_caps.json`.
pub async fn fetch_openai_models_with_caps(
    http: &reqwest::Client,
    data_dir: Option<&Path>,
    provider_id: &str,
    base_url: &str,
    api_key: &str,
) -> Vec<String> {
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
    let Ok(val) = resp.json::<Value>().await else {
        return Vec::new();
    };
    let mut models = Vec::new();
    let mut discovered = Vec::new();
    if let Some(arr) = val["data"].as_array() {
        for item in arr {
            let Some(id) = item["id"].as_str() else {
                continue;
            };
            models.push(id.to_string());
            if let Some(ctx) = extract_context_from_model_json(item) {
                let max_out = extract_output_from_model_json(item);
                discovered.push((id.to_string(), ctx, max_out));
            }
        }
    }
    if let Some(dir) = data_dir.filter(|_| !discovered.is_empty()) {
        let mut cache = load_caps(dir);
        let ts = now_ms();
        for (id, ctx, max_out) in discovered {
            let key = cap_key(provider_id, &id);
            if cache.models.get(&key).is_some_and(|c| c.source == "override") {
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
    models.sort();
    models.dedup();
    models
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
    for key in ["max_output_tokens", "max_completion_tokens", "output_token_limit"] {
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
        assert_eq!(context_window("my-local-model-32k"), 32_768);
        assert_eq!(context_window("unknown-relay-model"), 65_536);
    }
}
