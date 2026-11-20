//! Console chrome persisted in `<data_dir>/console.json`.
//!
//! `theme` and `show_sidebar` used to live on the agent `ModelPrefs` record.
//! The first load copies them out of `model.json` when `console.json` is absent.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ConsolePrefs {
    #[serde(default)]
    pub(crate) theme: usize,
    #[serde(default = "sidebar_default")]
    pub(crate) show_sidebar: bool,
}

fn sidebar_default() -> bool {
    true
}

impl Default for ConsolePrefs {
    fn default() -> Self {
        Self {
            theme: 0,
            show_sidebar: true,
        }
    }
}

fn console_path(data_dir: &Path) -> PathBuf {
    data_dir.join("console.json")
}

pub(crate) fn load(data_dir: &Path) -> ConsolePrefs {
    let path = console_path(data_dir);
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(prefs) = serde_json::from_str::<ConsolePrefs>(&raw) {
            return prefs;
        }
    }
    let prefs = legacy_model_prefs(data_dir);
    let _ = save(data_dir, &prefs);
    prefs
}

pub(crate) fn save(data_dir: &Path, prefs: &ConsolePrefs) -> anyhow::Result<()> {
    envoy_core::fs::ensure_private_dir(data_dir)?;
    let data = serde_json::to_string_pretty(prefs)?;
    envoy_core::fs::write_atomic_sync(&console_path(data_dir), data.as_bytes(), 0o600)?;
    Ok(())
}

/// `show_sidebar` defaults to true when the old key is absent.
fn legacy_model_prefs(data_dir: &Path) -> ConsolePrefs {
    let Ok(raw) = std::fs::read_to_string(data_dir.join("model.json")) else {
        return ConsolePrefs::default();
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return ConsolePrefs::default();
    };
    let theme = val.get("theme").and_then(|item| item.as_u64()).unwrap_or(0) as usize;
    let show_sidebar = val
        .get("show_sidebar")
        .and_then(|item| item.as_bool())
        .unwrap_or(true);
    ConsolePrefs {
        theme,
        show_sidebar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "envoy-ui-{:02x}{:02x}{:02x}{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3]
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_sidebar_key_defaults_true_and_lands_in_console_json() {
        let dir = tmpdir();
        std::fs::write(dir.join("model.json"), r#"{"theme":2,"active":null}"#).unwrap();
        let prefs = load(&dir);
        assert_eq!(prefs.theme, 2);
        assert!(prefs.show_sidebar);
        let saved = std::fs::read_to_string(dir.join("console.json")).unwrap();
        assert!(saved.contains("\"theme\": 2"));
        assert!(saved.contains("\"show_sidebar\": true"));
        std::fs::write(
            dir.join("model.json"),
            r#"{"theme":0,"show_sidebar":false}"#,
        )
        .unwrap();
        let again = load(&dir);
        assert_eq!(again.theme, 2);
        assert!(again.show_sidebar);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
