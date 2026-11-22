//! Declarative `plugin.json` manifest parsing and safe sub-path resolution.

use crate::permissions::types::PolicyFile;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginScope {
    Managed = 0,
    User = 1,
    Project = 2,
}

impl PluginScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PluginManifest {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub skills: Option<String>,
    #[serde(default)]
    pub agents: Option<String>,
    #[serde(default)]
    pub commands: Option<String>,
    #[serde(default)]
    pub hooks: Option<String>,
    #[serde(default)]
    pub mcp: Option<String>,
    #[serde(default)]
    pub permissions: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub scope: PluginScope,
    pub enabled: bool,
    pub root_dir: PathBuf,
    pub skills_dir: Option<PathBuf>,
    pub agents_dir: Option<PathBuf>,
    pub commands_dir: Option<PathBuf>,
    pub hooks_file: Option<PathBuf>,
    pub mcp_file: Option<PathBuf>,
    pub permissions_file: Option<PathBuf>,
    /// `allow` rules listed in the plugin's `permissions.json` (never auto-applied;
    /// surfaced by `jade plugin inspect` for explicit user approval).
    pub suggested_allows: Vec<String>,
}

pub fn sanitize_plugin_id(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in raw.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if (c == '-' || c == ' ' || c == '.') && !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "plugin".into()
    } else {
        out
    }
}

/// Resolve a relative subpath inside `plugin_root`, rejecting `..`, absolute paths,
/// or symlinks that escape `plugin_root`.
pub fn resolve_contained_path(plugin_root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel.trim());
    if rel.trim().is_empty() || p.is_absolute() {
        return None;
    }
    for comp in p.components() {
        if matches!(
            comp,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return None;
        }
    }
    let root_canon = std::fs::canonicalize(plugin_root).ok()?;
    let candidate = plugin_root.join(p);
    let cand_canon = std::fs::canonicalize(&candidate).ok()?;
    if cand_canon.starts_with(&root_canon) {
        Some(cand_canon)
    } else {
        None
    }
}

pub(super) fn load_plugin_dir(scope: PluginScope, dir: &Path) -> Option<LoadedPlugin> {
    let manifest_path = dir.join("plugin.json");
    if !manifest_path.is_file() {
        return None;
    }
    let raw = std::fs::read_to_string(&manifest_path).ok()?;
    let manifest: PluginManifest = serde_json::from_str(&raw).ok()?;
    let fallback_id = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("plugin");
    let id = sanitize_plugin_id(manifest.id.as_deref().unwrap_or(fallback_id));
    let name = manifest
        .name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| id.clone());
    let version = manifest
        .version
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "0.0.0".into());
    let description = manifest.description.unwrap_or_default();

    let skills_dir = resolve_opt_dir(dir, manifest.skills.as_deref(), "skills");
    let agents_dir = resolve_opt_dir(dir, manifest.agents.as_deref(), "agents");
    let commands_dir = resolve_opt_dir(dir, manifest.commands.as_deref(), "commands");
    let hooks_file = resolve_opt_file(dir, manifest.hooks.as_deref(), "hooks.json");
    let mcp_file = resolve_opt_file(dir, manifest.mcp.as_deref(), "mcp.json");
    let permissions_file =
        resolve_opt_file(dir, manifest.permissions.as_deref(), "permissions.json");

    let suggested_allows = permissions_file
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<PolicyFile>(&s).ok())
        .map(|pf| pf.allow)
        .unwrap_or_default();

    Some(LoadedPlugin {
        id,
        name,
        version,
        description,
        scope,
        enabled: true,
        root_dir: std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()),
        skills_dir,
        agents_dir,
        commands_dir,
        hooks_file,
        mcp_file,
        permissions_file,
        suggested_allows,
    })
}

fn resolve_opt_dir(root: &Path, declared: Option<&str>, default_rel: &str) -> Option<PathBuf> {
    let rel = declared.unwrap_or(default_rel);
    resolve_contained_path(root, rel).filter(|p| p.is_dir())
}

fn resolve_opt_file(root: &Path, declared: Option<&str>, default_rel: &str) -> Option<PathBuf> {
    let rel = declared.unwrap_or(default_rel);
    resolve_contained_path(root, rel).filter(|p| p.is_file())
}
