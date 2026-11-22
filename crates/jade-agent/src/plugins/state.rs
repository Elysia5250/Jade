//! Plugin enable/disable state (`<data_dir>/plugins.json`) and local install/remove.

use super::manifest::{load_plugin_dir, LoadedPlugin, PluginScope};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginsState {
    #[serde(default)]
    pub disabled: BTreeSet<String>,
}

pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("plugins.json")
}

pub fn load_state(data_dir: &Path) -> PluginsState {
    let Ok(raw) = std::fs::read_to_string(state_path(data_dir)) else {
        return PluginsState::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save_state(data_dir: &Path, state: &PluginsState) -> Result<()> {
    jade_core::fs::ensure_private_dir(data_dir)?;
    let body = serde_json::to_string_pretty(state)?;
    jade_core::fs::write_atomic_sync(&state_path(data_dir), body.as_bytes(), 0o600)?;
    Ok(())
}

pub fn is_plugin_enabled(state: &PluginsState, scope: PluginScope, id: &str) -> bool {
    if scope == PluginScope::Managed {
        return true;
    }
    !state.disabled.contains(id)
}

pub fn set_plugin_enabled(data_dir: &Path, id: &str, enabled: bool) -> Result<()> {
    let mut state = load_state(data_dir);
    if enabled {
        state.disabled.remove(id);
    } else {
        state.disabled.insert(id.to_string());
    }
    save_state(data_dir, &state)
}

/// Install a local plugin directory into `<data_dir>/plugins/<id>`, rejecting
/// invalid manifests or symlinks that escape the source tree.
pub fn install_local_plugin(data_dir: &Path, src: &Path) -> Result<LoadedPlugin> {
    if !src.is_dir() {
        bail!("plugin source is not a directory: {}", src.display());
    }
    let parsed = load_plugin_dir(PluginScope::User, src)
        .with_context(|| format!("missing or invalid plugin.json in {}", src.display()))?;
    let dest_root = data_dir.join("plugins");
    jade_core::fs::ensure_private_dir(&dest_root)?;
    let dest = dest_root.join(&parsed.id);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)
            .with_context(|| format!("replacing existing plugin {}", dest.display()))?;
    }
    let src_canon = std::fs::canonicalize(src)?;
    copy_dir_contained(&src_canon, &src_canon, &dest)?;
    let _ = set_plugin_enabled(data_dir, &parsed.id, true);
    load_plugin_dir(PluginScope::User, &dest)
        .context("installed plugin failed post-copy verification")
}

pub fn remove_user_plugin(data_dir: &Path, id: &str) -> Result<bool> {
    let dir = data_dir.join("plugins").join(id);
    if !dir.is_dir() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir)?;
    let mut state = load_state(data_dir);
    state.disabled.remove(id);
    let _ = save_state(data_dir, &state);
    Ok(true)
}

fn copy_dir_contained(root_canon: &Path, src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            let target = std::fs::canonicalize(&path)
                .with_context(|| format!("dangling symlink in plugin: {}", path.display()))?;
            if !target.starts_with(root_canon) {
                bail!(
                    "refusing plugin symlink escaping source directory: {}",
                    path.display()
                );
            }
        }
        let target_path = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_contained(root_canon, &path, &target_path)?;
        } else if path.is_file() {
            std::fs::copy(&path, &target_path)?;
        }
    }
    Ok(())
}
