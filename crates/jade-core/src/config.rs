//! Standalone agent policy configuration (`agent.json`) and caller gate.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Remote-tool policy for this device. Off by default: remote execution needs
/// an explicit opt-in and an explicit caller allowlist.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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

/// Authenticated origin of a service request.
///
/// Only same-user local Unix socket connections (verified via `SO_PEERCRED`)
/// or in-process callers construct [`Caller::Local`]; remote Conduit handoffs
/// must always use [`Caller::Remote`] so a peer named `"local"` cannot bypass
/// [`gate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    Local,
    Remote(String),
}

impl Caller {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Local => jade_proto::LOCAL_CALLER,
            Self::Remote(id) => id.as_str(),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }
}

impl AgentConfigStore {
    pub fn open(root: PathBuf) -> Result<Self> {
        crate::fs::ensure_private_dir(&root)?;
        crate::fs::ensure_private_dir(&root.join("tmp"))?;
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
        } else {
            self.config = AgentConfig::default();
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let data = serde_json::to_string_pretty(&self.config)?;
        crate::fs::write_atomic_sync(&self.root.join("agent.json"), data.as_bytes(), 0o600)
            .with_context(|| "write agent.json")?;
        Ok(())
    }

    pub fn set_enabled(&mut self, enabled: bool) -> Result<()> {
        self.config.enabled = enabled;
        self.save()
    }

    pub fn allow(&mut self, caller_id: String) -> Result<()> {
        if caller_id.trim().is_empty() {
            anyhow::bail!("caller id cannot be empty");
        }
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
            .map(|h| h.join(".jade"))
            .unwrap_or_else(|| PathBuf::from(".jade"))
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
/// Same-user local connections ([`Caller::Local`]) bypass the gate:
/// `enabled` and the allowlist govern *remote* callers, and a local shell can
/// run the same commands anyway.
pub fn gate(cfg: &AgentConfig, caller: &Caller) -> Option<String> {
    match caller {
        Caller::Local => None,
        Caller::Remote(id) => {
            if id.trim().is_empty() {
                return Some("empty caller id is not allowed".into());
            }
            if !cfg.enabled {
                return Some("agent disabled on this device (jade enable)".into());
            }
            if cfg.allow.iter().any(|d| d == id) {
                None
            } else {
                Some(format!(
                    "caller {id} is not allowlisted here (jade allow {id})"
                ))
            }
        }
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
        assert!(gate(&cfg(false, &["dev-a"]), &Caller::Remote("dev-a".into())).is_some());
        let denied = gate(&cfg(true, &["dev-a"]), &Caller::Remote("dev-b".into())).unwrap();
        assert!(denied.contains("not allowlisted"), "{denied}");
        assert!(gate(&cfg(true, &["dev-a"]), &Caller::Remote("dev-a".into())).is_none());
    }

    #[test]
    fn local_callers_bypass_but_remote_named_local_cannot() {
        assert!(gate(&cfg(false, &[]), &Caller::Local).is_none());
        assert!(gate(&cfg(true, &[]), &Caller::Local).is_none());
        assert!(gate(&cfg(false, &["local"]), &Caller::Remote("local".into())).is_some());
        assert!(gate(&cfg(true, &[]), &Caller::Remote("local".into())).is_some());
    }

    #[test]
    fn reload_resets_to_disabled_when_agent_json_is_removed() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("agent-reload-{n}"));
        let mut store = AgentConfigStore::open(dir.clone()).unwrap();
        store.set_enabled(true).unwrap();
        store.allow("peer-1".into()).unwrap();
        assert!(store.config.enabled);

        std::fs::remove_file(dir.join("agent.json")).unwrap();
        store.reload().unwrap();
        assert!(!store.config.enabled);
        assert!(store.config.allow.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn gate_and_allow_reject_empty_caller_id() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("agent-cfg-empty-{n}"));
        let mut store = AgentConfigStore::open(dir.clone()).unwrap();
        assert!(store.allow(String::new()).is_err());
        assert!(store.allow("   ".into()).is_err());

        let c = cfg(true, &[""]);
        assert!(gate(&c, &Caller::Remote(String::new())).is_some());
        assert!(gate(&c, &Caller::Remote("  ".into())).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_agent_config_uses_defaults() {
        let parsed: AgentConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(parsed.enabled);
        assert!(parsed.allow.is_empty());
        assert_eq!(parsed.max_jobs, 4);
    }
}
