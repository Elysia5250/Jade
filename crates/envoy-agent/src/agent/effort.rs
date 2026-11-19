//! Thinking effort for the current model.
//!
//! The saved value is one level name. The levels a model accepts come from
//! the `models.dev` catalog (`reasoning_options`), not from a fixed table.
//! A request sends that level only when the model lists it. `off` sends
//! `none` when that is listed, or disables the thinking toggle on an
//! OpenAI-compatible chat model that advertises one.

use crate::agent::context::catalog::{load_caps, save_caps};
use crate::agent::context::reasoning::{self, ReasoningSpec};
use crate::auth::ProviderProtocol;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const DEFAULT_LEVEL: &str = "low";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pref {
    #[serde(default = "default_level")]
    level: String,
}

fn default_level() -> String {
    DEFAULT_LEVEL.to_string()
}

fn prefs_path(data_dir: &Path) -> PathBuf {
    data_dir.join("effort.json")
}

pub fn load(data_dir: &Path) -> String {
    let Ok(raw) = std::fs::read_to_string(prefs_path(data_dir)) else {
        return DEFAULT_LEVEL.to_string();
    };
    serde_json::from_str::<Pref>(&raw)
        .ok()
        .and_then(|prefs| normalize(&prefs.level))
        .unwrap_or_else(|| DEFAULT_LEVEL.to_string())
}

pub fn normalize(raw: &str) -> Option<String> {
    let level = raw.trim().to_ascii_lowercase();
    let mut chars = level.chars();
    let first = chars.next()?;
    if !first.is_ascii_lowercase() || level.len() > 32 {
        return None;
    }
    chars
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
        .then_some(level)
}

pub fn set(data_dir: &Path, level: &str) -> Result<(), String> {
    let Some(level) = normalize(level) else {
        return Err("usage: /effort [level]".into());
    };
    let text = serde_json::to_string_pretty(&Pref { level }).map_err(|err| err.to_string())?;
    if let Some(parent) = prefs_path(data_dir).parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("cannot create: {err}"))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(prefs_path(data_dir))
        .map_err(|err| format!("cannot create: {err}"))?;
    file.write_all(text.as_bytes())
        .map_err(|err| format!("cannot create: {err}"))?;
    Ok(())
}

pub fn advertised(data_dir: &Path, provider_id: &str, model_id: &str) -> Option<ReasoningSpec> {
    reasoning::lookup(&load_caps(data_dir), provider_id, model_id)
}

/// The model lists this level, or `off` maps onto something it publishes.
pub fn accepts(spec: &ReasoningSpec, protocol: ProviderProtocol, level: &str) -> bool {
    if spec.efforts.iter().any(|have| have == level) {
        return true;
    }
    level == "off" && can_disable(spec, protocol)
}

fn can_disable(spec: &ReasoningSpec, protocol: ProviderProtocol) -> bool {
    spec.efforts.iter().any(|level| level == "none")
        || (spec.toggle && protocol == ProviderProtocol::OpenAiChat)
}

pub fn describe(spec: Option<&ReasoningSpec>) -> String {
    let Some(spec) = spec else {
        return "no thinking levels advertised for this model".into();
    };
    if spec.efforts.is_empty() {
        return if spec.toggle {
            "thinking can be turned off; no named levels".into()
        } else {
            "no thinking levels advertised for this model".into()
        };
    }
    let mut text = format!("this model accepts {}", spec.efforts.join(", "));
    if spec.toggle && !spec.efforts.iter().any(|level| level == "none") {
        text.push_str(", off");
    }
    text
}

/// Attach the saved level when this model publishes it.
pub fn apply(
    body: &mut Value,
    protocol: ProviderProtocol,
    provider_id: &str,
    model_id: &str,
    data_dir: &Path,
) {
    let Some(spec) = advertised(data_dir, provider_id, model_id) else {
        return;
    };
    let saved = load(data_dir);
    if !accepts(&spec, protocol, &saved) {
        return;
    }
    let effort = if saved == "off" {
        spec.efforts
            .iter()
            .find(|level| level.as_str() == "none")
            .cloned()
    } else {
        Some(saved)
    };
    match protocol {
        ProviderProtocol::OpenAiChat => apply_chat(body, &spec, effort.as_deref()),
        ProviderProtocol::OpenAiCodex => {
            if let Some(level) = effort {
                body["reasoning"] = json!({ "effort": level });
            }
        }
        ProviderProtocol::Anthropic => {
            if let Some(level) = effort {
                body["output_config"] = json!({ "effort": level });
            }
        }
    }
}

fn apply_chat(body: &mut Value, spec: &ReasoningSpec, effort: Option<&str>) {
    if let Some(level) = effort {
        body["reasoning_effort"] = json!(level);
        if spec.toggle && level != "none" {
            body["thinking"] = json!({ "type": "enabled" });
        }
        return;
    }
    if spec.toggle {
        body["thinking"] = json!({ "type": "disabled" });
    }
}

#[allow(dead_code)]
pub fn store_spec(data_dir: &Path, provider_id: &str, model_id: &str, spec: ReasoningSpec) {
    let mut cache = load_caps(data_dir);
    reasoning::record(&mut cache, provider_id, model_id, spec);
    let _ = save_caps(data_dir, &cache);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("envoy-effort-{n}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_level_is_sent_only_when_that_model_lists_it() {
        let dir = tmpdir();
        assert_eq!(load(&dir), "low");
        store_spec(
            &dir,
            "openai",
            "gpt-5.4",
            ReasoningSpec {
                efforts: vec!["none".into(), "low".into(), "medium".into(), "high".into()],
                toggle: false,
            },
        );
        set(&dir, "high").unwrap();
        let mut body = json!({});
        apply(
            &mut body,
            ProviderProtocol::OpenAiChat,
            "openai",
            "gpt-5.4",
            &dir,
        );
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("thinking").is_none());

        set(&dir, "max").unwrap();
        let mut skipped = json!({});
        apply(
            &mut skipped,
            ProviderProtocol::OpenAiChat,
            "openai",
            "gpt-5.4",
            &dir,
        );
        assert!(skipped.as_object().unwrap().is_empty());

        set(&dir, "off").unwrap();
        let mut off = json!({});
        apply(
            &mut off,
            ProviderProtocol::OpenAiChat,
            "openai",
            "gpt-5.4",
            &dir,
        );
        assert_eq!(off["reasoning_effort"], "none");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deepseek_off_uses_the_toggle_and_claude_uses_output_config() {
        let dir = tmpdir();
        store_spec(
            &dir,
            "deepseek",
            "deepseek-v4-flash",
            ReasoningSpec {
                efforts: vec!["low".into(), "high".into(), "max".into()],
                toggle: true,
            },
        );
        set(&dir, "max").unwrap();
        let mut chat = json!({});
        apply(
            &mut chat,
            ProviderProtocol::OpenAiChat,
            "deepseek",
            "deepseek-v4-flash",
            &dir,
        );
        assert_eq!(chat["reasoning_effort"], "max");
        assert_eq!(chat["thinking"]["type"], "enabled");
        set(&dir, "off").unwrap();
        let mut disabled = json!({});
        apply(
            &mut disabled,
            ProviderProtocol::OpenAiChat,
            "deepseek",
            "deepseek-v4-flash",
            &dir,
        );
        assert_eq!(disabled["thinking"]["type"], "disabled");
        assert!(disabled.get("reasoning_effort").is_none());

        store_spec(
            &dir,
            "anthropic",
            "claude-sonnet-5",
            ReasoningSpec {
                efforts: vec![
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                    "xhigh".into(),
                    "max".into(),
                ],
                toggle: true,
            },
        );
        set(&dir, "xhigh").unwrap();
        let mut claude = json!({});
        apply(
            &mut claude,
            ProviderProtocol::Anthropic,
            "anthropic",
            "claude-sonnet-5",
            &dir,
        );
        assert_eq!(claude["output_config"]["effort"], "xhigh");
        assert!(!accepts(
            &advertised(&dir, "anthropic", "claude-sonnet-5").unwrap(),
            ProviderProtocol::Anthropic,
            "off"
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
