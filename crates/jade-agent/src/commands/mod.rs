//! Declarative custom `/command` prompt templates (`commands/*.md`).
//!
//! Discovery order (later overrides earlier for the bare command name;
//! plugin commands remain addressable via `<plugin_id>:<name>`):
//! 1. Enabled Managed / User plugin `commands/*.md`
//! 2. User roots: `<data_dir>/commands`, `~/.jade/commands`, `~/.claude/commands`
//! 3. Enabled Project plugin `commands/*.md` (trusted workspaces only)
//! 4. Project roots (trusted workspaces only): `<cwd>/.jade/commands`, `<cwd>/.claude/commands`
//!
//! Built-in slash commands always take precedence in the CLI/TUI and can never
//! be shadowed by custom commands.

use crate::permissions::is_workspace_trusted;
use crate::plugins::{enabled_plugins, PluginScope};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    User,
    Project,
    Plugin,
}

impl CommandScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Plugin => "plugin",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub args: String,
    pub template: String,
    pub scope: CommandScope,
    pub plugin_id: Option<String>,
    pub source_path: PathBuf,
}

impl CustomCommand {
    /// Qualified `<plugin>:<name>` identifier when provided by a plugin.
    pub fn qualified_name(&self) -> Option<String> {
        self.plugin_id
            .as_ref()
            .map(|pid| format!("{pid}:{}", self.name))
    }
}

pub fn discover_commands(data_dir: &Path, cwd: &Path) -> Vec<CustomCommand> {
    let plugins = enabled_plugins(data_dir, cwd);
    let trusted = is_workspace_trusted(data_dir, cwd);

    let mut roots: Vec<(CommandScope, Option<String>, PathBuf)> = Vec::new();
    for p in plugins
        .iter()
        .filter(|p| p.scope != PluginScope::Project)
    {
        if let Some(dir) = &p.commands_dir {
            roots.push((CommandScope::Plugin, Some(p.id.clone()), dir.clone()));
        }
    }
    roots.push((CommandScope::User, None, data_dir.join("commands")));
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push((CommandScope::User, None, home.join(".jade").join("commands")));
        roots.push((
            CommandScope::User,
            None,
            home.join(".claude").join("commands"),
        ));
    }
    if trusted {
        for p in plugins.iter().filter(|p| p.scope == PluginScope::Project) {
            if let Some(dir) = &p.commands_dir {
                roots.push((CommandScope::Plugin, Some(p.id.clone()), dir.clone()));
            }
        }
        roots.push((
            CommandScope::Project,
            None,
            cwd.join(".jade").join("commands"),
        ));
        roots.push((
            CommandScope::Project,
            None,
            cwd.join(".claude").join("commands"),
        ));
    }

    let mut by_name: BTreeMap<String, CustomCommand> = BTreeMap::new();
    let mut seen_files: BTreeSet<PathBuf> = BTreeSet::new();

    for (scope, plugin_id, root) in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|ent| ent.path()))
            .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("md"))
            .collect();
        files.sort();
        for path in files {
            let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if !seen_files.insert(canon) {
                continue;
            }
            if let Some(cmd) = parse_command_file(scope, plugin_id.as_deref(), &path) {
                by_name.insert(cmd.name.clone(), cmd);
            }
        }
    }

    by_name.into_values().collect()
}

/// Resolve either a bare `<name>` or a namespaced `<plugin_id>:<name>` custom command.
pub fn resolve_command(data_dir: &Path, cwd: &Path, raw_name: &str) -> Option<CustomCommand> {
    let target = raw_name.trim().trim_start_matches('/');
    if target.is_empty() {
        return None;
    }
    if let Some((pid, cname)) = target.split_once(':') {
        let plugins = enabled_plugins(data_dir, cwd);
        let plugin = plugins
            .into_iter()
            .find(|p| p.id.eq_ignore_ascii_case(pid.trim()))?;
        let dir = plugin.commands_dir?;
        let entries = std::fs::read_dir(&dir).ok()?;
        for ent in entries.flatten() {
            let path = ent.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Some(cmd) = parse_command_file(CommandScope::Plugin, Some(&plugin.id), &path)
                {
                    if cmd.name.eq_ignore_ascii_case(cname.trim()) {
                        return Some(cmd);
                    }
                }
            }
        }
        return None;
    }
    discover_commands(data_dir, cwd)
        .into_iter()
        .find(|c| c.name == target || c.name.eq_ignore_ascii_case(target))
}

/// Render a custom command template with `$ARGUMENTS` and positional `$1`..`$9`.
/// If no placeholder appears in the template and `raw_arg` is non-empty, `raw_arg`
/// is appended after a blank line.
pub fn render_command(cmd: &CustomCommand, raw_arg: &str) -> String {
    let arg = raw_arg.trim();
    let positional = split_args(arg);
    let mut out = cmd.template.clone();
    let mut used_placeholder = false;

    if out.contains("$ARGUMENTS") {
        out = out.replace("$ARGUMENTS", arg);
        used_placeholder = true;
    }
    for idx in (1..=9).rev() {
        let token = format!("${idx}");
        if out.contains(&token) {
            let val = positional.get(idx - 1).map(String::as_str).unwrap_or("");
            out = out.replace(&token, val);
            used_placeholder = true;
        }
    }
    if !used_placeholder && !arg.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(arg);
    }
    out.trim().to_string()
}

fn split_args(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in raw.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn parse_command_file(
    scope: CommandScope,
    plugin_id: Option<&str>,
    path: &Path,
) -> Option<CustomCommand> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (fm_opt, body) = crate::frontmatter::split_frontmatter(&raw);
    let template = body.trim().to_string();
    if template.is_empty() {
        return None;
    }
    let fallback_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cmd")
        .to_string();

    let mut name = fallback_name;
    let mut description = String::new();
    let mut args = String::new();

    if let Some(fm) = fm_opt {
        for line in fm.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((k, v)) = trimmed.split_once(':') else {
                continue;
            };
            let key = k.trim();
            let val = crate::frontmatter::unquote(crate::frontmatter::strip_inline_comment(v));
            match key {
                "name" if !val.is_empty() => name = val.trim_start_matches('/').to_string(),
                "description" if !val.is_empty() => description = val,
                "args" | "argument-hint" | "argument_hint" => args = val,
                _ => {}
            }
        }
    }

    if description.is_empty() {
        description = template
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or("Custom prompt command")
            .chars()
            .take(80)
            .collect();
    }

    Some(CustomCommand {
        name,
        description,
        args,
        template,
        scope,
        plugin_id: plugin_id.map(str::to_string),
        source_path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::set_workspace_trusted;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "jade-cmds-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn discovers_renders_and_gates_project_commands() {
        let root = tmpdir("render");
        let data = root.join("data");
        let cwd = root.join("repo");
        std::fs::create_dir_all(data.join("commands")).unwrap();
        std::fs::create_dir_all(cwd.join(".jade/commands")).unwrap();

        std::fs::write(
            data.join("commands/review.md"),
            "---\nname: review\ndescription: Review diff\nargs: \"[focus]\"\n---\nReview $1 carefully. All: $ARGUMENTS\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join(".jade/commands/deploy.md"),
            "---\ndescription: Deploy checklist\n---\nCheck staging first.\n",
        )
        .unwrap();

        // Untrusted -> only user command visible
        let cmds_u = discover_commands(&data, &cwd);
        assert!(cmds_u.iter().any(|c| c.name == "review"));
        assert!(!cmds_u.iter().any(|c| c.name == "deploy"));

        let rev = resolve_command(&data, &cwd, "review").unwrap();
        assert_eq!(
            render_command(&rev, "\"src/main.rs\" extra"),
            "Review src/main.rs carefully. All: \"src/main.rs\" extra"
        );

        // Trusted -> project command visible and appends args when no placeholder
        set_workspace_trusted(&data, &cwd, true).unwrap();
        let dep = resolve_command(&data, &cwd, "deploy").unwrap();
        assert_eq!(render_command(&dep, "prod"), "Check staging first.\n\nprod");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plugin_commands_support_namespaced_resolution() {
        let root = tmpdir("ns");
        let data = root.join("data");
        let cwd = root.join("repo");
        let plug = data.join("plugins/alpha");
        std::fs::create_dir_all(plug.join("commands")).unwrap();
        std::fs::create_dir_all(data.join("commands")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        std::fs::write(plug.join("plugin.json"), r#"{"id":"alpha"}"#).unwrap();
        std::fs::write(
            plug.join("commands/check.md"),
            "---\ndescription: Plugin check\n---\nFrom plugin: $ARGUMENTS\n",
        )
        .unwrap();
        std::fs::write(
            data.join("commands/check.md"),
            "---\ndescription: User check\n---\nFrom user: $ARGUMENTS\n",
        )
        .unwrap();

        let bare = resolve_command(&data, &cwd, "check").unwrap();
        assert_eq!(bare.scope, CommandScope::User);
        let ns = resolve_command(&data, &cwd, "alpha:check").unwrap();
        assert_eq!(ns.scope, CommandScope::Plugin);
        assert_eq!(render_command(&ns, "fast"), "From plugin: fast");

        let _ = std::fs::remove_dir_all(&root);
    }
}
