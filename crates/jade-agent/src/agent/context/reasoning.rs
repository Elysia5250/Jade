//! Per-model thinking controls published by `models.dev`.
//!
//! The catalog's `reasoning_options` lists the effort values that model
//! accepts, and whether thinking can be toggled. Provider `/models` responses
//! do not include this. Levels are never invented here.

use super::catalog::CapsCache;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReasoningSpec {
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default)]
    pub toggle: bool,
}

pub fn spec_from_model(model: &Value) -> Option<ReasoningSpec> {
    let options = model.get("reasoning_options")?.as_array()?;
    let mut spec = ReasoningSpec::default();
    for opt in options {
        match opt.get("type").and_then(Value::as_str) {
            Some("effort") => {
                let Some(values) = opt.get("values").and_then(Value::as_array) else {
                    continue;
                };
                for value in values {
                    if let Some(level) = value.as_str() {
                        push_level(&mut spec.efforts, level);
                    } else if value.is_null() {
                        push_level(&mut spec.efforts, "none");
                    }
                }
            }
            Some("toggle") => spec.toggle = true,
            _ => {}
        }
    }
    (!spec.efforts.is_empty() || spec.toggle).then_some(spec)
}

pub fn record(cache: &mut CapsCache, provider_id: &str, model_id: &str, spec: ReasoningSpec) {
    for key in keys(provider_id, model_id) {
        cache.reasoning.insert(key, spec.clone());
    }
}

pub fn lookup(cache: &CapsCache, provider_id: &str, model_id: &str) -> Option<ReasoningSpec> {
    keys(provider_id, model_id)
        .into_iter()
        .find_map(|key| cache.reasoning.get(&key).cloned())
}

fn push_level(levels: &mut Vec<String>, raw: &str) {
    let level = raw.trim().to_ascii_lowercase();
    if level.is_empty() || levels.iter().any(|have| have == &level) {
        return;
    }
    levels.push(level);
}

/// models.dev ids and this client's catalog ids are not always the same name.
fn keys(provider_id: &str, model_id: &str) -> Vec<String> {
    let mut ids = vec![provider_id.to_string()];
    let extra: &[&str] = match provider_id {
        "alibaba" | "qwen" => &["alibaba", "qwen"],
        "zhipuai" | "zhipu" => &["zhipuai", "zhipu"],
        "moonshotai" | "moonshot" => &["moonshotai", "moonshot"],
        "302ai" | "ai302" => &["302ai", "ai302"],
        "opencode" | "opencode-zen" | "opencode-go" => &["opencode", "opencode-zen", "opencode-go"],
        _ => &[],
    };
    for id in extra {
        if !ids.iter().any(|have| have == id) {
            ids.push((*id).to_string());
        }
    }
    ids.into_iter()
        .map(|id| format!("{id}/{model_id}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn effort_values_and_a_null_none_come_from_the_catalog_entry() {
        let model = json!({
            "reasoning_options": [
                {"type": "toggle"},
                {"type": "effort", "values": ["low", "high", null, "low"]},
                {"type": "budget_tokens", "max": 1024}
            ]
        });
        let spec = spec_from_model(&model).unwrap();
        assert_eq!(spec.efforts, ["low", "high", "none"]);
        assert!(spec.toggle);
        assert!(spec_from_model(&json!({})).is_none());
    }

    #[test]
    fn qwen_finds_the_alibaba_catalog_entry() {
        let mut cache = CapsCache::default();
        record(
            &mut cache,
            "alibaba",
            "qwen3.8-max",
            ReasoningSpec {
                efforts: vec!["low".into(), "medium".into(), "xhigh".into()],
                toggle: true,
            },
        );
        let found = lookup(&cache, "qwen", "qwen3.8-max").unwrap();
        assert_eq!(found.efforts, ["low", "medium", "xhigh"]);
    }
}
