//! Subagent model target parsing and tier/cross-provider resolution.

use super::{effective_providers, is_retired_model, ModelRef, StoredProvider};
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
    let providers = effective_providers(data_dir);
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
                && parent_provider
                    .custom_models
                    .iter()
                    .any(|m| m == &model_id)
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
                    && prov.custom_models.iter().any(|m| m == &model_id)
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
                && !parent_provider.custom_models.is_empty();
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
        ModelTargetSpec::Fast => {
            pick_by_tier(parent_provider, &providers, Tier::Fast)
                .unwrap_or_else(|| (parent_provider.clone(), parent_model.clone()))
        }
        ModelTargetSpec::Flagship => {
            pick_by_tier(parent_provider, &providers, Tier::Flagship)
                .unwrap_or_else(|| (parent_provider.clone(), parent_model.clone()))
        }
    }
}

#[derive(Clone, Copy)]
enum Tier {
    Fast,
    Flagship,
}

fn pick_by_tier(
    parent_provider: &StoredProvider,
    providers: &BTreeMap<String, StoredProvider>,
    tier: Tier,
) -> Option<(StoredProvider, ModelRef)> {
    let mut best: Option<(i32, StoredProvider, String)> = None;

    let mut consider = |prov: &StoredProvider, same_provider: bool| {
        for model_id in &prov.custom_models {
            if is_retired_model(&prov.id, model_id) {
                continue;
            }
            let base = tier_score(model_id, tier);
            if base <= 0 {
                continue;
            }
            let score = base + if same_provider { 25 } else { 0 };
            let replace = best
                .as_ref()
                .map(|(s, _, _)| score > *s)
                .unwrap_or(true);
            if replace {
                best = Some((score, prov.clone(), model_id.clone()));
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
    let id = model_id.to_ascii_lowercase();
    match tier {
        Tier::Fast => {
            if id.contains("haiku") {
                100
            } else if id.contains("flash") {
                95
            } else if id.contains("nano") {
                90
            } else if id.contains("mini") {
                88
            } else if id.contains("lite") || id.contains("fast") || id.contains("small") {
                80
            } else {
                0
            }
        }
        Tier::Flagship => {
            if id.contains("mythos") || id.contains("fable") || id.contains("opus") {
                100
            } else if id.contains("gpt-6") || id.contains("gpt-5.6") || id.contains("gpt-5.5") {
                96
            } else if id.contains("gpt-5") && !id.contains("mini") && !id.contains("nano") {
                90
            } else if id.contains("gemini-4") || id.contains("pro") {
                88
            } else if id.contains("sonnet") {
                82
            } else {
                0
            }
        }
    }
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
            "envoy-routing-{tag}-{:02x}{:02x}{:02x}{:02x}",
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

        let (p_cross, m_cross) =
            resolve_model_target(&dir, &ant, &parent_ref, "openai/gpt-5-mini");
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
}
