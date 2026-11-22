//! `jade plugin` CLI subcommands (`list`, `inspect`, `enable`, `disable`, `install`, `remove`).

use anyhow::{bail, Result};
use clap::Subcommand;
use jade_agent::plugins::{
    discover_plugins, install_local_plugin, remove_user_plugin, set_plugin_enabled, PluginScope,
};
use std::path::{Path, PathBuf};

#[derive(Subcommand, Debug)]
pub(crate) enum PluginCmd {
    /// List discovered plugins across managed, user, and trusted project roots.
    List,
    /// Inspect components and suggested permission rules of a plugin.
    Inspect { id: String },
    /// Enable a user or project plugin.
    Enable { id: String },
    /// Disable a user or project plugin.
    Disable { id: String },
    /// Install a plugin bundle from a local directory into `~/.jade/plugins`.
    Install { path: PathBuf },
    /// Remove a user-installed plugin from `~/.jade/plugins`.
    Remove { id: String },
}

pub(crate) fn cmd_plugin(data_dir: &Path, cwd: &Path, cmd: PluginCmd) -> Result<()> {
    match cmd {
        PluginCmd::List => {
            let report = discover_plugins(data_dir, cwd);
            if !report.skipped_untrusted.is_empty() {
                println!(
                    "skipped untrusted project plugins (trust workspace via TUI `/trust on`):"
                );
                for p in &report.skipped_untrusted {
                    println!("  - {}", p.display());
                }
            }
            if report.plugins.is_empty() {
                println!("no plugins installed");
                return Ok(());
            }
            for p in report.plugins {
                let status = if p.enabled { "enabled" } else { "disabled" };
                println!(
                    "{:<20} v{:<8} scope={:<7} status={:<8} {}",
                    p.id,
                    p.version,
                    p.scope.label(),
                    status,
                    p.description
                );
            }
            Ok(())
        }
        PluginCmd::Inspect { id } => {
            let report = discover_plugins(data_dir, cwd);
            let Some(p) = report
                .plugins
                .into_iter()
                .find(|p| p.id.eq_ignore_ascii_case(id.trim()))
            else {
                bail!("unknown plugin `{id}`");
            };
            println!("Plugin: {} ({})", p.name, p.id);
            println!("Version: {}", p.version);
            println!("Scope: {}", p.scope.label());
            println!("Enabled: {}", p.enabled);
            println!("Root: {}", p.root_dir.display());
            if !p.description.is_empty() {
                println!("Description: {}", p.description);
            }
            println!("Components:");
            if let Some(d) = &p.skills_dir {
                println!("  skills:      {}", d.display());
            }
            if let Some(d) = &p.agents_dir {
                println!("  agents:      {}", d.display());
            }
            if let Some(d) = &p.commands_dir {
                println!("  commands:    {}", d.display());
            }
            if let Some(f) = &p.hooks_file {
                println!("  hooks:       {}", f.display());
            }
            if let Some(f) = &p.mcp_file {
                println!("  mcp:         {}", f.display());
            }
            if let Some(f) = &p.permissions_file {
                println!("  permissions: {} (deny/ask active)", f.display());
            }
            if !p.suggested_allows.is_empty() {
                println!(
                    "Suggested allow rules (not auto-applied; approve via `/permissions allow <rule>`):"
                );
                for rule in &p.suggested_allows {
                    println!("  - {rule}");
                }
            }
            Ok(())
        }
        PluginCmd::Enable { id } => {
            set_plugin_enabled(data_dir, id.trim(), true)?;
            println!("Enabled plugin `{}`", id.trim());
            Ok(())
        }
        PluginCmd::Disable { id } => {
            let report = discover_plugins(data_dir, cwd);
            if let Some(p) = report
                .plugins
                .iter()
                .find(|p| p.id.eq_ignore_ascii_case(id.trim()))
            {
                if p.scope == PluginScope::Managed {
                    bail!("cannot disable managed plugin `{}`", p.id);
                }
            }
            set_plugin_enabled(data_dir, id.trim(), false)?;
            println!("Disabled plugin `{}`", id.trim());
            Ok(())
        }
        PluginCmd::Install { path } => {
            let installed = install_local_plugin(data_dir, &path)?;
            println!(
                "Installed plugin `{}` v{} into {}",
                installed.id,
                installed.version,
                installed.root_dir.display()
            );
            if !installed.suggested_allows.is_empty() {
                println!(
                    "Note: plugin lists {} suggested allow rule(s); run `jade plugin inspect {}` to review.",
                    installed.suggested_allows.len(),
                    installed.id
                );
            }
            Ok(())
        }
        PluginCmd::Remove { id } => {
            if remove_user_plugin(data_dir, id.trim())? {
                println!("Removed user plugin `{}`", id.trim());
            } else {
                bail!("no user-installed plugin `{}` found", id.trim());
            }
            Ok(())
        }
    }
}
