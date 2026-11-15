//! Standalone agent policy configuration (`agent.json`) and caller gate.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Remote-tool policy for this device. Off by default: remote execution needs
/// an explicit opt-in and an explicit caller allowlist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub enabled: bool,
    /// Caller ids allowed to invoke tools here (`local` is always allowed in-process).
    pub allow: Vec<String>,
    /// Concurrent invocations before new ones are refused.
    pub max_jobs: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow: Vec::new(),
            max_jobs: 4,
        }
    }
}

/// Persistent standalone store for `agent.json` under `root`.
#[derive(Debug, Clone)]
pub struct AgentConfigStore {
    pub root: PathBuf,
    pub config: AgentConfig,
}

impl AgentConfigStore {
    pub fn open(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(root.join("tmp"))?;
        let path = root.join("agent.json");
        let config = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            serde_json::from_str(&raw)?
        } else {
            AgentConfig::default()
        };
        Ok(Self { root, config })
    }

    /// Re-read `agent.json` from disk so external `enable`/`allow` changes take
    /// effect immediately without restarting a long-lived service.
    pub fn reload(&mut self) -> Result<()> {
        let path = self.root.join("agent.json");
        if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            self.config = serde_json::from_str(&raw)?;
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let data = serde_json::to_string_pretty(&self.config)?;
        let tmp = self.root.join("tmp").join("agent.json.tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, self.root.join("agent.json"))
            .with_context(|| "write agent.json")?;
        Ok(())
    }

    pub fn set_enabled(&mut self, enabled: bool) -> Result<()> {
        self.config.enabled = enabled;
        self.save()
    }

    pub fn allow(&mut self, caller_id: String) -> Result<()> {
        if !self.config.allow.contains(&caller_id) {
            self.config.allow.push(caller_id);
        }
        self.save()
    }

    pub fn deny(&mut self, caller_id: &str) -> Result<()> {
        self.config.allow.retain(|a| a != caller_id);
        self.save()
    }

    pub fn default_data_dir() -> PathBuf {
        home_dir()
            .map(|h| h.join(".envoy"))
            .unwrap_or_else(|| PathBuf::from(".envoy"))
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Why a call is refused, or `None` when it may run.
///
/// Same-user local connections ([`envoy_proto::LOCAL_CALLER`]) bypass the gate:
/// `enabled` and the allowlist govern *remote* callers, and a local shell can
/// run the same commands anyway.
pub fn gate(cfg: &AgentConfig, caller: &str) -> Option<String> {
    if caller == envoy_proto::LOCAL_CALLER {
        return None;
    }
    if !cfg.enabled {
        return Some("agent disabled on this device (envoy enable)".into());
    }
    if cfg.allow.iter().any(|d| d == caller) {
        None
    } else {
        Some(format!(
            "caller {caller} is not allowlisted here (envoy allow {caller})"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, allow: &[&str]) -> AgentConfig {
        AgentConfig {
            enabled,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            max_jobs: 4,
        }
    }

    #[test]
    fn gate_refuses_until_enabled_and_allowlisted() {
        assert!(gate(&cfg(false, &["dev-a"]), "dev-a").is_some());
        let denied = gate(&cfg(true, &["dev-a"]), "dev-b").unwrap();
        assert!(denied.contains("not allowlisted"), "{denied}");
        assert!(gate(&cfg(true, &["dev-a"]), "dev-a").is_none());
    }

    #[test]
    fn local_callers_bypass_the_remote_gate() {
        assert!(gate(&cfg(false, &[]), envoy_proto::LOCAL_CALLER).is_none());
        assert!(gate(&cfg(true, &[]), envoy_proto::LOCAL_CALLER).is_none());
    }

    #[test]
    fn store_persists_enable_allow_and_deny() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("agent-cfg-{n}"));
        let mut store = AgentConfigStore::open(dir.clone()).unwrap();
        assert!(!store.config.enabled);
        store.set_enabled(true).unwrap();
        store.allow("peer-1".into()).unwrap();
        store.allow("peer-1".into()).unwrap();
        assert_eq!(store.config.allow, vec!["peer-1"]);

        let reloaded = AgentConfigStore::open(dir.clone()).unwrap();
        assert!(reloaded.config.enabled);
        assert_eq!(reloaded.config.allow, vec!["peer-1"]);

        let mut store2 = reloaded;
        store2.deny("peer-1").unwrap();
        assert!(store2.config.allow.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
