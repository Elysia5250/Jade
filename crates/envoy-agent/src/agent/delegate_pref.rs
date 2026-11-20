//! How readily the parent agent calls `delegate`.
//!
//! Stored in `<data_dir>/delegate.json` (mode 0600). A missing or unreadable
//! file means `low`: the catalog is listed, and the model is not told to fan
//! out. The sentence is injected only into the depth-0 environment note. It
//! is not part of the stable system prompt, and a subagent never sees it.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegateLevel {
    Off,
    #[default]
    Low,
    Normal,
    High,
}

impl DelegateLevel {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "low" => Some(Self::Low),
            "normal" => Some(Self::Normal),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }

    /// One English sentence. The stable system prompt already tells the model
    /// to answer in the user's language; this line only sets willingness.
    pub fn instruction(self) -> &'static str {
        match self {
            Self::Off => {
                "Delegation: off. Do not call `delegate` unless the user explicitly asks you to."
            }
            Self::Low => {
                "Delegation: low. Call `delegate` only when the user asks, or for one clearly separate long read-only search. Do the rest of the work yourself."
            }
            Self::Normal => {
                "Delegation: normal. Use `delegate` for broad exploration, review, or several independent read-only lookups. Edit files yourself when you already know which files to change."
            }
            Self::High => {
                "Delegation: high. Prefer `delegate` for exploration, review, and multi-file investigation. You coordinate and edit. Batch independent read-only tasks into one turn."
            }
        }
    }
}

impl fmt::Display for DelegateLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pref {
    #[serde(default)]
    level: DelegateLevel,
}

fn prefs_path(data_dir: &Path) -> PathBuf {
    data_dir.join("delegate.json")
}

pub fn load(data_dir: &Path) -> DelegateLevel {
    let Ok(raw) = std::fs::read_to_string(prefs_path(data_dir)) else {
        return DelegateLevel::Low;
    };
    serde_json::from_str::<Pref>(&raw)
        .map(|prefs| prefs.level)
        .unwrap_or(DelegateLevel::Low)
}

pub fn instruction(data_dir: &Path) -> &'static str {
    load(data_dir).instruction()
}

pub fn set(data_dir: &Path, level: DelegateLevel) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&Pref { level }).map_err(|err| err.to_string())?;
    envoy_core::fs::write_atomic_sync(&prefs_path(data_dir), text.as_bytes(), 0o600)
        .map_err(|err| format!("cannot create: {err:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("envoy-delegate-{n}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn missing_or_corrupt_pref_is_low_and_set_is_private() {
        let dir = tmpdir();
        assert_eq!(load(&dir), DelegateLevel::Low);
        assert!(instruction(&dir).contains("Delegation: low"));
        set(&dir, DelegateLevel::High).unwrap();
        assert_eq!(load(&dir), DelegateLevel::High);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(prefs_path(&dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::write(prefs_path(&dir), "{").unwrap();
        assert_eq!(load(&dir), DelegateLevel::Low);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_level_has_a_different_sentence() {
        let mut seen = std::collections::BTreeSet::new();
        for level in [
            DelegateLevel::Off,
            DelegateLevel::Low,
            DelegateLevel::Normal,
            DelegateLevel::High,
        ] {
            let text = level.instruction();
            assert!(text.starts_with(&format!("Delegation: {}", level.as_str())));
            assert!(seen.insert(text));
        }
    }

    #[test]
    fn the_rule_is_depth_zero_and_outside_the_stable_prompt() {
        let dir = tmpdir();
        set(&dir, DelegateLevel::High).unwrap();
        let top = crate::agent::prompt::environment_note_for_depth(&dir, &dir, "sess", 0)
            .expect("builtins always produce a catalog");
        assert!(top.contains("Delegation: high"));
        assert!(top.contains("Available subagents"));
        let memory_at = top.find("Long-term memory");
        let agents_at = top.find("Available subagents").unwrap();
        if let Some(memory_at) = memory_at {
            assert!(memory_at < agents_at);
        }
        let deep = crate::agent::prompt::environment_note_for_depth(&dir, &dir, "sess", 1);
        if let Some(text) = deep {
            assert!(!text.contains("Delegation:"));
        }
        let system = crate::agent::prompt::build_system_prompt("local", &dir);
        assert!(!system.contains("Delegation:"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
