//! Lightweight declarative Plugin / Bundle discovery (`plugin.json`).
//!
//! A plugin directory bundles any subset of `skills/`, `agents/`, `commands/`,
//! `hooks.json`, `mcp.json`, and `permissions.json` under a single manifest,
//! strictly inheriting its scope (`Managed > User > Project`) and workspace trust gate.

pub mod manifest;
pub mod state;

use crate::permissions::is_workspace_trusted;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub use manifest::{LoadedPlugin, PluginManifest, PluginScope};
pub use state::{
    install_local_plugin, is_plugin_enabled, load_state, remove_user_plugin, set_plugin_enabled,
    PluginsState,
};

const DEFAULT_MANAGED_PLUGINS_DIR: &str = "/etc/jade/plugins";

#[derive(Debug, Clone, Default)]
pub struct PluginDiscoveryReport {
    pub plugins: Vec<LoadedPlugin>,
    pub skipped_untrusted: Vec<PathBuf>,
}

fn managed_plugins_dir(override_dir: Option<&Path>) -> PathBuf {
    if let Some(p) = override_dir {
        return p.to_path_buf();
    }
    #[cfg(test)]
    if let Ok(env_path) = std::env::var("JADE_MANAGED_PLUGINS") {
        if !env_path.trim().is_empty() {
            return PathBuf::from(env_path);
        }
    }
    PathBuf::from(DEFAULT_MANAGED_PLUGINS_DIR)
}

pub fn discover_plugins(data_dir: &Path, cwd: &Path) -> PluginDiscoveryReport {
    discover_plugins_with_managed(data_dir, cwd, None)
}

pub fn discover_plugins_with_managed(
    data_dir: &Path,
    cwd: &Path,
    managed_override: Option<&Path>,
) -> PluginDiscoveryReport {
    let state = load_state(data_dir);
    let mut report = PluginDiscoveryReport::default();
    let mut by_id: BTreeMap<String, LoadedPlugin> = BTreeMap::new();
    let mut seen_dirs: BTreeSet<PathBuf> = BTreeSet::new();

    // Higher precedence scopes load first and cannot be overwritten by lower scopes.
    let mut roots: Vec<(PluginScope, PathBuf)> = vec![
        (PluginScope::Managed, managed_plugins_dir(managed_override)),
        (PluginScope::User, data_dir.join("plugins")),
    ];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push((PluginScope::User, home.join(".jade").join("plugins")));
    }

    let project_root = cwd.join(".jade").join("plugins");
    if is_workspace_trusted(data_dir, cwd) {
        roots.push((PluginScope::Project, project_root));
    } else if project_root.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&project_root) {
            for ent in entries.flatten() {
                let p = ent.path();
                if p.join("plugin.json").is_file() {
                    report.skipped_untrusted.push(p);
                }
            }
        }
    }

    for (scope, root) in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|ent| ent.path()))
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let canon = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            if !seen_dirs.insert(canon) {
                continue;
            }
            let Some(mut plugin) = manifest::load_plugin_dir(scope, &dir) else {
                continue;
            };
            if by_id.contains_key(&plugin.id) {
                continue;
            }
            plugin.enabled = is_plugin_enabled(&state, scope, &plugin.id);
            by_id.insert(plugin.id.clone(), plugin);
        }
    }

    report.plugins = by_id.into_values().collect();
    report
}

pub fn enabled_plugins(data_dir: &Path, cwd: &Path) -> Vec<LoadedPlugin> {
    discover_plugins(data_dir, cwd)
        .plugins
        .into_iter()
        .filter(|p| p.enabled)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::set_workspace_trusted;
    use serde_json::json;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "jade-plugins-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn discovers_bundles_respects_trust_gate_and_blocks_traversal() {
        let root = tmpdir("bundle");
        let data = root.join("data");
        let cwd = root.join("repo");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        // Project plugin with a traversal attempt on hooks (`../../evil.json`)
        let proj_plug = cwd.join(".jade/plugins/demo");
        std::fs::create_dir_all(proj_plug.join("skills/lint")).unwrap();
        std::fs::write(
            proj_plug.join("skills/lint/SKILL.md"),
            "---\nname: lint\ndescription: Lint helper\n---\nRun clippy.\n",
        )
        .unwrap();
        std::fs::write(root.join("evil.json"), "{}").unwrap();
        std::fs::write(
            proj_plug.join("plugin.json"),
            serde_json::to_string_pretty(&json!({
                "id": "demo",
                "version": "1.0.0",
                "hooks": "../../../../evil.json",
                "permissions": "permissions.json"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            proj_plug.join("permissions.json"),
            r#"{"allow":["exec(*)"],"deny":["exec(rm *)"]}"#,
        )
        .unwrap();

        // Untrusted -> skipped
        let untrusted = discover_plugins(&data, &cwd);
        assert!(untrusted.plugins.is_empty());
        assert_eq!(untrusted.skipped_untrusted.len(), 1);

        // Trusted -> loaded, traversal rejected, suggested allows recorded
        set_workspace_trusted(&data, &cwd, true).unwrap();
        let trusted = discover_plugins(&data, &cwd);
        assert_eq!(trusted.plugins.len(), 1);
        let p = &trusted.plugins[0];
        assert_eq!(p.id, "demo");
        assert!(p.skills_dir.is_some());
        assert!(p.hooks_file.is_none(), "traversal path must be rejected");
        assert_eq!(p.suggested_allows, vec!["exec(*)"]);

        // Disable -> excluded from enabled_plugins
        set_plugin_enabled(&data, "demo", false).unwrap();
        assert!(enabled_plugins(&data, &cwd).is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_and_remove_user_plugin() {
        let root = tmpdir("install");
        let data = root.join("data");
        let src = root.join("src-pack");
        std::fs::create_dir_all(src.join("commands")).unwrap();
        std::fs::write(
            src.join("plugin.json"),
            r#"{"id":"my-pack","name":"My Pack","version":"0.2.0","description":"desc"}"#,
        )
        .unwrap();
        std::fs::write(
            src.join("commands/hello.md"),
            "---\ndescription: Say hi\n---\nHello $ARGUMENTS\n",
        )
        .unwrap();

        let installed = install_local_plugin(&data, &src).unwrap();
        assert_eq!(installed.id, "my-pack");
        assert!(installed.commands_dir.is_some());
        assert_eq!(enabled_plugins(&data, &root).len(), 1);

        assert!(remove_user_plugin(&data, "my-pack").unwrap());
        assert!(enabled_plugins(&data, &root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn full_bundle_wires_all_six_subsystems_and_disables_cleanly() {
        let root = tmpdir("six-way");
        let data = root.join("data");
        let cwd = root.join("repo");
        let plug = cwd.join(".jade/plugins/suite");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(plug.join("skills/sk1")).unwrap();
        std::fs::create_dir_all(plug.join("agents")).unwrap();
        std::fs::create_dir_all(plug.join("commands")).unwrap();

        std::fs::write(
            plug.join("plugin.json"),
            r#"{"id":"suite","version":"0.1.0","description":"full bundle"}"#,
        )
        .unwrap();
        std::fs::write(
            plug.join("skills/sk1/SKILL.md"),
            "---\nname: suite-skill\ndescription: Skill from suite\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            plug.join("agents/auditor.md"),
            "---\nname: suite-auditor\ndescription: Auditor\nmode: readonly\n---\nAudit.\n",
        )
        .unwrap();
        std::fs::write(
            plug.join("commands/audit.md"),
            "---\nname: suite-audit\ndescription: Run audit\n---\nAudit $ARGUMENTS\n",
        )
        .unwrap();
        std::fs::write(
            plug.join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"id":"h1","command":"echo ok","sandbox":"off"}]}}"#,
        )
        .unwrap();
        std::fs::write(
            plug.join("mcp.json"),
            r#"{"mcpServers":{"suite-srv":{"command":"echo","args":["hi"]}}}"#,
        )
        .unwrap();
        std::fs::write(
            plug.join("permissions.json"),
            r#"{"allow":["exec(*)"],"deny":["exec(rm -rf *)"],"ask":["webfetch(*)"]}"#,
        )
        .unwrap();

        set_workspace_trusted(&data, &cwd, true).unwrap();

        // 1. Skills
        assert!(crate::skills::discover_skills(&data, &cwd)
            .iter()
            .any(|s| s.name == "suite-skill"));
        // 2. Agents
        assert!(crate::agent::subagent::discover_agents(&data, &cwd)
            .iter()
            .any(|a| a.name == "suite-auditor"));
        // 3. Commands
        assert!(crate::commands::resolve_command(&data, &cwd, "suite-audit").is_some());
        // 4. Hooks (prefixed + project sandbox enforced!)
        let hooks = crate::hooks::config::load_hooks(&data, &cwd, true, None);
        let h = hooks
            .iter()
            .find(|r| r.id == "plugin:suite:h1")
            .expect("plugin hook loaded");
        assert!(!h.sandbox_off, "project plugin hook must remain sandboxed");
        // 5. MCP
        let mcp = crate::mcp::config::load_mcp_configs(&data, &cwd);
        assert!(mcp.servers.iter().any(|s| s.name == "suite-srv"));
        // 6. Permissions: deny & ask applied, allow NEVER auto-applied!
        let pol = crate::permissions::rules::load_policies(&data, &cwd, true, None, &[]);
        assert!(pol.rules.iter().any(|r| r.pattern == "exec(rm -rf *)"));
        assert!(pol.rules.iter().any(|r| r.pattern == "webfetch(*)"));
        assert!(!pol.rules.iter().any(|r| r.pattern == "exec(*)"));

        // Disable plugin -> all 6 subsystems drop it immediately.
        set_plugin_enabled(&data, "suite", false).unwrap();
        assert!(!crate::skills::discover_skills(&data, &cwd)
            .iter()
            .any(|s| s.name == "suite-skill"));
        assert!(!crate::agent::subagent::discover_agents(&data, &cwd)
            .iter()
            .any(|a| a.name == "suite-auditor"));
        assert!(crate::commands::resolve_command(&data, &cwd, "suite-audit").is_none());
        assert!(!crate::hooks::config::load_hooks(&data, &cwd, true, None)
            .iter()
            .any(|r| r.id == "plugin:suite:h1"));
        assert!(!crate::mcp::config::load_mcp_configs(&data, &cwd)
            .servers
            .iter()
            .any(|s| s.name == "suite-srv"));
        let pol_off = crate::permissions::rules::load_policies(&data, &cwd, true, None, &[]);
        assert!(!pol_off.rules.iter().any(|r| r.pattern == "exec(rm -rf *)"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
