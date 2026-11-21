//! Subagent model target parsing and tier/cross-provider resolution.

use super::{
    default_model_for, effective_providers_with, is_retired_model, load_prefs, Credential,
    ModelRef, ProviderProtocol, StoredProvider, BUILTIN_PROVIDERS,
};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelTargetSpec {
    Inherit,
    Fast,
    Flagship,
    Explicit {
        provider_id: Option<String>,
        model_id: String,
    },
}

pub fn parse_model_spec(
    raw: &str,
    providers: &BTreeMap<String, StoredProvider>,
) -> ModelTargetSpec {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("inherit") {
        return ModelTargetSpec::Inherit;
    }
    if matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "fast" | "mini" | "cheap" | "haiku" | "flash"
    ) {
        return ModelTargetSpec::Fast;
    }
    if matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "flagship" | "strong" | "smart" | "opus"
    ) {
        return ModelTargetSpec::Flagship;
    }
    if let Some((left, right)) = trimmed.split_once('/') {
        let p = left.trim();
        let m = right.trim();
        if !p.is_empty() && !m.is_empty() && providers.contains_key(p) {
            return ModelTargetSpec::Explicit {
                provider_id: Some(p.to_string()),
                model_id: m.to_string(),
            };
        }
    }
    ModelTargetSpec::Explicit {
        provider_id: None,
        model_id: trimmed.to_string(),
    }
}

/// Resolve a subagent `model` field (`inherit`, `fast`, `flagship`, or `<provider>/<model>`).
/// Always falls back safely to `(parent_provider, parent_model)` if no matching
/// model/provider is configured.
pub fn resolve_model_target(
    data_dir: &Path,
    parent_provider: &StoredProvider,
    parent_model: &ModelRef,
    spec: &str,
) -> (StoredProvider, ModelRef) {
    let providers = super::effective_providers(data_dir);
    match parse_model_spec(spec, &providers) {
        ModelTargetSpec::Inherit => (parent_provider.clone(), parent_model.clone()),
        ModelTargetSpec::Explicit {
            provider_id: Some(pid),
            model_id,
        } => {
            if !is_retired_model(&pid, &model_id) {
                if let Some(prov) = providers.get(&pid) {
                    return (
                        prov.clone(),
                        ModelRef {
                            provider_id: prov.id.clone(),
                            model_id,
                        },
                    );
                }
            }
            (parent_provider.clone(), parent_model.clone())
        }
        ModelTargetSpec::Explicit {
            provider_id: None,
            model_id,
        } => {
            if !is_retired_model(&parent_provider.id, &model_id)
                && parent_provider.all_models().iter().any(|m| m == &model_id)
            {
                return (
                    parent_provider.clone(),
                    ModelRef {
                        provider_id: parent_provider.id.clone(),
                        model_id,
                    },
                );
            }
            for prov in providers.values() {
                if !is_retired_model(&prov.id, &model_id)
                    && prov.all_models().iter().any(|m| m == &model_id)
                {
                    return (
                        prov.clone(),
                        ModelRef {
                            provider_id: prov.id.clone(),
                            model_id,
                        },
                    );
                }
            }
            let looks_like_provider_prefix = model_id.contains('/')
                && parent_provider.id != "openrouter"
                && !parent_provider.all_models().is_empty();
            if !looks_like_provider_prefix
                && !model_id.is_empty()
                && !is_retired_model(&parent_provider.id, &model_id)
            {
                return (
                    parent_provider.clone(),
                    ModelRef {
                        provider_id: parent_provider.id.clone(),
                        model_id,
                    },
                );
            }
            (parent_provider.clone(), parent_model.clone())
        }
        ModelTargetSpec::Fast => pick_by_tier(
            parent_provider,
            &parent_model.model_id,
            &providers,
            Tier::Fast,
        )
        .unwrap_or_else(|| (parent_provider.clone(), parent_model.clone())),
        ModelTargetSpec::Flagship => pick_by_tier(
            parent_provider,
            &parent_model.model_id,
            &providers,
            Tier::Flagship,
        )
        .unwrap_or_else(|| (parent_provider.clone(), parent_model.clone())),
    }
}

#[derive(Clone, Copy)]
enum Tier {
    Fast,
    Flagship,
}

fn pick_by_tier(
    parent_provider: &StoredProvider,
    parent_model_id: &str,
    providers: &BTreeMap<String, StoredProvider>,
    tier: Tier,
) -> Option<(StoredProvider, ModelRef)> {
    let mut best: Option<(i32, StoredProvider, String)> = None;

    let mut consider = |prov: &StoredProvider, same_provider: bool| {
        for model_id in prov.all_models() {
            if is_retired_model(&prov.id, &model_id) {
                continue;
            }
            let base = tier_score(&model_id, tier);
            if base <= 0 {
                continue;
            }
            let active_bonus = if same_provider && model_id == parent_model_id {
                15
            } else {
                0
            };
            let score = base + if same_provider { 25 } else { 0 } + active_bonus;
            let replace = best.as_ref().map(|(s, _, _)| score > *s).unwrap_or(true);
            if replace {
                best = Some((score, prov.clone(), model_id));
            }
        }
    };

    consider(parent_provider, true);
    for prov in providers.values() {
        if prov.id != parent_provider.id {
            consider(prov, false);
        }
    }

    best.map(|(_, prov, model_id)| {
        let model = ModelRef {
            provider_id: prov.id.clone(),
            model_id,
        };
        (prov, model)
    })
}

fn tier_score(model_id: &str, tier: Tier) -> i32 {
    let rt = match tier {
        Tier::Fast => crate::model_registry::RoutingTier::Fast,
        Tier::Flagship => crate::model_registry::RoutingTier::Flagship,
    };
    crate::model_registry::tier_score(model_id, rt)
}

pub(crate) fn env_base_url_var(provider_id: &str) -> Option<&'static str> {
    crate::model_registry::env_base_url_var(provider_id)
}

pub fn default_env_model(provider_id: &str) -> &'static str {
    crate::model_registry::default_env_model(provider_id)
}

fn non_empty_env(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env(name)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_env_protocol(raw: &str) -> Option<ProviderProtocol> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "anthropic_messages" => Some(ProviderProtocol::Anthropic),
        "openai" | "openai_chat" | "openai-chat" | "openai-compatible" => {
            Some(ProviderProtocol::OpenAiChat)
        }
        "openai_codex" | "openai-codex" | "codex" => Some(ProviderProtocol::OpenAiCodex),
        _ => None,
    }
}

pub(crate) fn merge_env_providers(
    map: &mut BTreeMap<String, StoredProvider>,
    env: &dyn Fn(&str) -> Option<String>,
) {
    for builtin in BUILTIN_PROVIDERS {
        if map.contains_key(builtin.id) {
            continue;
        }
        let Some(var) = builtin.env_var else {
            continue;
        };
        let Some(val) = non_empty_env(env, var) else {
            continue;
        };
        let base_url = env_base_url_var(builtin.id)
            .and_then(|v| non_empty_env(env, v))
            .unwrap_or_else(|| builtin.default_base_url.to_string());
        map.insert(
            builtin.id.to_string(),
            StoredProvider {
                id: builtin.id.to_string(),
                name: format!("{} (env)", builtin.name),
                protocol: builtin.protocol,
                base_url,
                credential: Credential::ApiKey { key: val },
                custom_models: Vec::new(),
                fetched_models: Vec::new(),
                model_routes: BTreeMap::new(),
                updated_ms: 0,
            },
        );
    }
    if !map.contains_key("custom") {
        if let Some(key) = non_empty_env(env, "JADE_API_KEY") {
            let base_url = non_empty_env(env, "JADE_BASE_URL")
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let protocol = non_empty_env(env, "JADE_PROTOCOL")
                .and_then(|p| parse_env_protocol(&p))
                .unwrap_or(ProviderProtocol::OpenAiChat);
            map.insert(
                "custom".to_string(),
                StoredProvider {
                    id: "custom".to_string(),
                    name: "Custom (env)".to_string(),
                    protocol,
                    base_url,
                    credential: Credential::ApiKey { key },
                    custom_models: Vec::new(),
                    fetched_models: Vec::new(),
                    model_routes: BTreeMap::new(),
                    updated_ms: 0,
                },
            );
        }
    }
}

fn infer_provider_id_from_model<'a>(
    model_id: &str,
    providers: &'a BTreeMap<String, StoredProvider>,
) -> Option<&'a str> {
    crate::model_registry::infer_provider_id_from_model(model_id, providers)
}

/// Resolve the active provider and model, honoring CLI `--model`, `JADE_MODEL`,
/// `JADE_BASE_URL`, `JADE_PROTOCOL`, and `<PROVIDER>_BASE_URL` env overrides.
pub fn resolve_provider_with_overrides(
    data_dir: &Path,
    model_override: Option<&str>,
) -> Option<(StoredProvider, ModelRef)> {
    resolve_provider_with_env(data_dir, model_override, &|k| std::env::var(k).ok())
}

pub(crate) fn resolve_provider_with_env(
    data_dir: &Path,
    model_override: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<(StoredProvider, ModelRef)> {
    let providers = effective_providers_with(data_dir, env);
    if providers.is_empty() {
        return None;
    }
    let prefs = load_prefs(data_dir);
    let default_pair = || -> Option<(StoredProvider, ModelRef)> {
        if let Some(active) = prefs.active.as_ref() {
            if let Some(prov) = providers.get(&active.provider_id) {
                let model_id = if active.model_id.is_empty()
                    || is_retired_model(&active.provider_id, &active.model_id)
                {
                    let d = default_model_for(prov);
                    if d.is_empty() {
                        default_env_model(&prov.id).to_string()
                    } else {
                        d
                    }
                } else {
                    active.model_id.clone()
                };
                return Some((
                    prov.clone(),
                    ModelRef {
                        provider_id: prov.id.clone(),
                        model_id,
                    },
                ));
            }
        }
        let first = providers.values().next()?.clone();
        let d = default_model_for(&first);
        let model_id = if d.is_empty() {
            default_env_model(&first.id).to_string()
        } else {
            d
        };
        Some((
            first.clone(),
            ModelRef {
                provider_id: first.id,
                model_id,
            },
        ))
    };

    let spec = model_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| non_empty_env(env, "JADE_MODEL"));

    let (mut prov, model_ref) = match spec {
        Some(raw) => {
            if let Some((left, right)) = raw.split_once('/') {
                let pid = left.trim();
                let mid = right.trim();
                if let Some(p) = (!mid.is_empty()).then(|| providers.get(pid)).flatten() {
                    (
                        p.clone(),
                        ModelRef {
                            provider_id: p.id.clone(),
                            model_id: mid.to_string(),
                        },
                    )
                } else if let Some((p, _)) = default_pair() {
                    (
                        p.clone(),
                        ModelRef {
                            provider_id: p.id,
                            model_id: raw,
                        },
                    )
                } else {
                    return None;
                }
            } else if let Some(p) = providers
                .values()
                .find(|p| p.all_models().iter().any(|m| m == &raw))
            {
                (
                    p.clone(),
                    ModelRef {
                        provider_id: p.id.clone(),
                        model_id: raw,
                    },
                )
            } else if let Some(pid) = infer_provider_id_from_model(&raw, &providers) {
                let p = providers.get(pid)?.clone();
                (
                    p.clone(),
                    ModelRef {
                        provider_id: p.id,
                        model_id: raw,
                    },
                )
            } else if let Some((p, _)) = default_pair() {
                (
                    p.clone(),
                    ModelRef {
                        provider_id: p.id,
                        model_id: raw,
                    },
                )
            } else {
                return None;
            }
        }
        None => default_pair()?,
    };

    if let Some(base_url) = non_empty_env(env, "JADE_BASE_URL")
        .or_else(|| env_base_url_var(&prov.id).and_then(|var| non_empty_env(env, var)))
    {
        prov.base_url = base_url;
    }
    if let Some(proto) = non_empty_env(env, "JADE_PROTOCOL").and_then(|s| parse_env_protocol(&s)) {
        prov.protocol = proto;
    }

    Some((prov, model_ref))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{now_ms, record_provider_login, Credential, ProviderProtocol};
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "jade-routing-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn mk_provider(id: &str, models: Vec<&str>) -> StoredProvider {
        StoredProvider {
            id: id.into(),
            name: id.into(),
            protocol: ProviderProtocol::Anthropic,
            base_url: "https://example.invalid/v1".into(),
            credential: Credential::ApiKey {
                key: "sk-test".into(),
            },
            custom_models: models.into_iter().map(String::from).collect(),
            fetched_models: Vec::new(),
            model_routes: BTreeMap::new(),
            updated_ms: now_ms(),
        }
    }

    #[test]
    fn resolves_fast_flagship_explicit_and_fallback() {
        let dir = tmpdir("tiers");
        let ant = mk_provider(
            "anthropic",
            vec!["claude-sonnet-5", "claude-haiku-5", "claude-opus-5"],
        );
        let oai = mk_provider("openai", vec!["gpt-5.5", "gpt-5-mini"]);
        record_provider_login(&dir, ant.clone(), Some("claude-sonnet-5".into())).unwrap();
        record_provider_login(&dir, oai, Some("gpt-5.5".into())).unwrap();

        let parent_ref = ModelRef {
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-5".into(),
        };

        let (p_fast, m_fast) = resolve_model_target(&dir, &ant, &parent_ref, "fast");
        assert_eq!(p_fast.id, "anthropic");
        assert_eq!(m_fast.model_id, "claude-haiku-5");

        let (p_flag, m_flag) = resolve_model_target(&dir, &ant, &parent_ref, "flagship");
        assert_eq!(p_flag.id, "anthropic");
        assert_eq!(m_flag.model_id, "claude-opus-5");

        let (p_cross, m_cross) = resolve_model_target(&dir, &ant, &parent_ref, "openai/gpt-5-mini");
        assert_eq!(p_cross.id, "openai");
        assert_eq!(m_cross.model_id, "gpt-5-mini");

        let (p_inh, m_inh) = resolve_model_target(&dir, &ant, &parent_ref, "inherit");
        assert_eq!(p_inh.id, "anthropic");
        assert_eq!(m_inh.model_id, "claude-sonnet-5");

        let (p_fb, m_fb) = resolve_model_target(&dir, &ant, &parent_ref, "missing-prov/foo");
        assert_eq!(p_fb.id, "anthropic");
        assert_eq!(m_fb.model_id, "claude-sonnet-5");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_env_provider_base_url_and_cli_model_overrides() {
        let dir = tmpdir("env-overrides");
        let env_fn = |k: &str| -> Option<String> {
            match k {
                "OPENAI_API_KEY" => Some("sk-oai-env".into()),
                "OPENAI_BASE_URL" => Some("http://127.0.0.1:8080/v1".into()),
                "ANTHROPIC_API_KEY" => Some("sk-ant-env".into()),
                "JADE_MODEL" => Some("gpt-5.4-mini".into()),
                _ => None,
            }
        };
        let (prov, model) = resolve_provider_with_env(&dir, None, &env_fn).unwrap();
        assert_eq!(prov.id, "openai");
        assert_eq!(prov.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(model.model_id, "gpt-5.4-mini");

        let (prov2, model2) =
            resolve_provider_with_env(&dir, Some("anthropic/claude-opus-4-5"), &env_fn).unwrap();
        assert_eq!(prov2.id, "anthropic");
        assert_eq!(model2.model_id, "claude-opus-4-5");

        let custom_env = |k: &str| -> Option<String> {
            match k {
                "JADE_API_KEY" => Some("sk-custom".into()),
                "JADE_BASE_URL" => Some("http://localhost:4000".into()),
                "JADE_PROTOCOL" => Some("anthropic".into()),
                "JADE_MODEL" => Some("relay-sonnet".into()),
                _ => None,
            }
        };
        let (prov3, model3) = resolve_provider_with_env(&dir, None, &custom_env).unwrap();
        assert_eq!(prov3.id, "custom");
        assert_eq!(prov3.protocol, ProviderProtocol::Anthropic);
        assert_eq!(prov3.base_url, "http://localhost:4000");
        assert_eq!(model3.model_id, "relay-sonnet");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
