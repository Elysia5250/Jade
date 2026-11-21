//! Built-in subagent presets and declarative `agents/*.md` discovery.
//!
//! Precedence (later overrides earlier by `name`):
//! 1. Built-in presets (`explore`, `plan`, `review`, `coder`)
//! 2. User scope (always loaded): `<data_dir>/agents/*.md`, `~/.envoy/agents/*.md`
//! 3. Project scope (ONLY when workspace is trusted): `<cwd>/.envoy/agents/*.md`

use crate::permissions::{is_workspace_trusted, PermissionMode};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_MAX_STEPS: usize = 12;
const HARD_MAX_STEPS: usize = 25;
const DEFAULT_TIMEOUT_S: u64 = 180;
const MAX_TIMEOUT_S: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentMode {
    ReadOnly,
    Plan,
    Inherit,
}

impl SubagentMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "readonly",
            Self::Plan => "plan",
            Self::Inherit => "inherit",
        }
    }

    pub fn is_readonly(self) -> bool {
        matches!(self, Self::ReadOnly | Self::Plan)
    }

    pub fn permission_mode(self, parent_mode: PermissionMode) -> PermissionMode {
        match self {
            Self::ReadOnly | Self::Plan => PermissionMode::Plan,
            Self::Inherit => parent_mode,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScope {
    Builtin,
    User,
    Project,
}

impl AgentScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubagentSpec {
    pub name: String,
    pub description: String,
    pub mode: SubagentMode,
    pub model: String,
    pub tools: Vec<String>,
    pub max_steps: usize,
    pub timeout_s: u64,
    pub system_prompt: String,
    pub scope: AgentScope,
    #[allow(dead_code)]
    pub source_path: Option<PathBuf>,
}

impl SubagentSpec {
    /// Tool allowlist enforced on both provider schema and tool dispatch.
    /// `None` means inherit all parent-enabled tools (with `delegate` still
    /// stripped at `depth >= 1`).
    pub fn effective_tools(&self) -> Option<Vec<String>> {
        if !self.tools.is_empty() {
            return Some(
                self.tools
                    .iter()
                    .filter(|t| t.as_str() != "delegate")
                    .cloned()
                    .collect(),
            );
        }
        if self.mode.is_readonly() {
            return Some(vec![
                "read".into(),
                "ls".into(),
                "glob".into(),
                "grep".into(),
                "webfetch".into(),
                "skill".into(),
                "say".into(),
            ]);
        }
        None
    }

    pub fn is_readonly(&self) -> bool {
        if !self.mode.is_readonly() {
            return false;
        }
        // Job, todo, monitor, and MCP tools mutate state outside the worktree diff.
        !self.tools.iter().any(|t| tool_mutates(t))
    }
}

fn tool_mutates(name: &str) -> bool {
    matches!(name, "write" | "edit" | "exec" | "job" | "todo" | "monitor")
        || name.starts_with("mcp__")
}

pub(crate) fn builtin_agents() -> Vec<SubagentSpec> {
    vec![
        SubagentSpec {
            name: "explore".into(),
            description: "Fast read-only codebase search, symbol lookup, and architecture mapping.".into(),
            mode: SubagentMode::ReadOnly,
            model: "fast".into(),
            tools: Vec::new(),
            max_steps: 12,
            timeout_s: 120,
            system_prompt: "You are a fast read-only codebase exploration subagent.\n\
                Inspect files strictly with `glob`, `grep`, `ls`, and `read`.\n\
                Report symbols and findings in the final card.\n\
                Never fabricate file contents or claim to have modified anything."
                .into(),
            scope: AgentScope::Builtin,
            source_path: None,
        },
        SubagentSpec {
            name: "plan".into(),
            description: "Read-only design and step-by-step implementation planning.".into(),
            mode: SubagentMode::Plan,
            model: "flagship".into(),
            tools: Vec::new(),
            max_steps: 12,
            timeout_s: 180,
            system_prompt: "You are a software architecture and planning subagent.\n\
                Explore relevant modules using read-only tools, identify invariants, risks, and file boundaries,\n\
                and deliver a crisp, actionable implementation and verification plan."
                .into(),
            scope: AgentScope::Builtin,
            source_path: None,
        },
        SubagentSpec {
            name: "review".into(),
            description: "Read-only code review for bugs, security issues, edge cases, and tests.".into(),
            mode: SubagentMode::ReadOnly,
            model: "inherit".into(),
            tools: Vec::new(),
            max_steps: 12,
            timeout_s: 180,
            system_prompt: "You are a rigorous code-review subagent.\n\
                Check correctness, edge cases, permission/security boundaries, concurrency hazards, and missing tests.\n\
                Put each finding in the final card with its severity and a concrete fix."
                .into(),
            scope: AgentScope::Builtin,
            source_path: None,
        },
        SubagentSpec {
            name: "coder".into(),
            description: "Focused implementation subagent that edits files and runs scoped checks.".into(),
            mode: SubagentMode::Inherit,
            model: "inherit".into(),
            tools: Vec::new(),
            max_steps: 18,
            timeout_s: 300,
            system_prompt: "You are a focused implementation subagent.\n\
                Implement the assigned scoped task cleanly, run targeted verification commands when permitted,\n\
                and report exact changed files, verification outputs, and any remaining caveats."
                .into(),
            scope: AgentScope::Builtin,
            source_path: None,
        },
    ]
}

pub(crate) fn discover_agents(data_dir: &Path, cwd: &Path) -> Vec<SubagentSpec> {
    let mut by_name: BTreeMap<String, SubagentSpec> = BTreeMap::new();
    for spec in builtin_agents() {
        by_name.insert(spec.name.clone(), spec);
    }

    let mut roots: Vec<(AgentScope, PathBuf)> = vec![(AgentScope::User, data_dir.join("agents"))];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push((AgentScope::User, home.join(".envoy").join("agents")));
    }
    if is_workspace_trusted(data_dir, cwd) {
        roots.push((AgentScope::Project, cwd.join(".envoy").join("agents")));
    }

    for (scope, root) in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|ent| ent.path()))
            .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("md"))
            .collect();
        files.sort();
        for path in files {
            if let Some(spec) = parse_agent_file(scope, &path) {
                by_name.insert(spec.name.clone(), spec);
            }
        }
    }

    by_name.into_values().collect()
}

pub(crate) fn resolve_agent(data_dir: &Path, cwd: &Path, name: &str) -> Result<SubagentSpec> {
    let target = name.trim();
    if target.is_empty() {
        bail!("missing subagent name");
    }
    discover_agents(data_dir, cwd)
        .into_iter()
        .find(|s| s.name == target || s.name.eq_ignore_ascii_case(target))
        .with_context(|| format!("unknown subagent `{target}`"))
}

pub(crate) fn format_agents_catalog(specs: &[SubagentSpec]) -> Option<String> {
    if specs.is_empty() {
        return None;
    }
    let mut out = String::from("<available_agents>\n");
    for s in specs {
        out.push_str(&format!(
            "  <agent name=\"{}\" mode=\"{}\" model=\"{}\" scope=\"{}\">{}</agent>\n",
            escape_xml(&s.name),
            s.mode.label(),
            escape_xml(&s.model),
            s.scope.label(),
            escape_xml(&s.description),
        ));
    }
    out.push_str("</available_agents>");
    Some(out)
}

fn parse_agent_file(scope: AgentScope, path: &Path) -> Option<SubagentSpec> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (fm_opt, body) = crate::frontmatter::split_frontmatter(&raw);
    let fallback_name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("agent")
        .to_string();

    let mut name = fallback_name;
    let mut description = String::new();
    let mut mode = SubagentMode::ReadOnly;
    let mut model = "inherit".to_string();
    let mut tools: Vec<String> = Vec::new();
    let mut max_steps = DEFAULT_MAX_STEPS;
    let mut timeout_s = DEFAULT_TIMEOUT_S;

    if let Some(fm) = fm_opt {
        parse_frontmatter(
            fm,
            &mut name,
            &mut description,
            &mut mode,
            &mut model,
            &mut tools,
            &mut max_steps,
            &mut timeout_s,
        );
    }

    let body_trimmed = body.trim().to_string();
    if description.is_empty() {
        description = body_trimmed
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or("Custom subagent")
            .chars()
            .take(160)
            .collect();
    }

    Some(SubagentSpec {
        name,
        description,
        mode,
        model,
        tools,
        max_steps: max_steps.clamp(1, HARD_MAX_STEPS),
        timeout_s: timeout_s.clamp(10, MAX_TIMEOUT_S),
        system_prompt: if body_trimmed.is_empty() {
            "Complete the assigned subtask accurately and concisely.".into()
        } else {
            body_trimmed
        },
        scope,
        source_path: Some(path.to_path_buf()),
    })
}

#[allow(clippy::too_many_arguments)]
fn parse_frontmatter(
    fm: &str,
    name: &mut String,
    description: &mut String,
    mode: &mut SubagentMode,
    model: &mut String,
    tools: &mut Vec<String>,
    max_steps: &mut usize,
    timeout_s: &mut u64,
) {
    use crate::frontmatter::{parse_inline_list, strip_inline_comment, unquote};

    let mut in_tools = false;
    for raw_line in fm.lines() {
        let line = raw_line.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if (line.starts_with(' ') || line.starts_with('\t')) && in_tools {
            if let Some(item) = trimmed.strip_prefix('-').map(str::trim) {
                let cleaned = unquote(item);
                if !cleaned.is_empty() {
                    tools.push(cleaned);
                }
            }
            continue;
        }
        in_tools = false;
        let Some((key, val)) = trimmed.split_once(':') else {
            continue;
        };
        let k = key.trim();
        let v = strip_inline_comment(val).trim();
        match k {
            "name" if !v.is_empty() => *name = unquote(v),
            "description" if !v.is_empty() => *description = unquote(v),
            "mode" => {
                *mode = match unquote(v).to_ascii_lowercase().as_str() {
                    "inherit" | "write" | "full" => SubagentMode::Inherit,
                    "plan" => SubagentMode::Plan,
                    _ => SubagentMode::ReadOnly,
                };
            }
            "model" if !v.is_empty() => *model = unquote(v),
            "max_steps" | "max-steps" => {
                if let Ok(n) = unquote(v).parse::<usize>() {
                    *max_steps = n;
                }
            }
            "timeout_s" | "timeout-s" | "timeout" => {
                if let Ok(n) = unquote(v).parse::<u64>() {
                    *timeout_s = n;
                }
            }
            "tools" | "allowed-tools" | "allowed_tools" => {
                if v.is_empty() {
                    in_tools = true;
                } else {
                    tools.extend(parse_inline_list(v, false));
                }
            }
            _ => {}
        }
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::set_workspace_trusted;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "envoy-subagents-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn discovers_builtins_and_blocks_project_agents_until_trusted() {
        let root = tmpdir("trust");
        let data = root.join("data");
        let cwd = root.join("repo");
        std::fs::create_dir_all(data.join("agents")).unwrap();
        std::fs::create_dir_all(cwd.join(".envoy").join("agents")).unwrap();

        std::fs::write(
            data.join("agents").join("perf.md"),
            "---\nname: perf\ndescription: \"Benchmark helper\"\nmode: readonly\nmodel: fast\ntools: [read, grep]\nmax_steps: 9\n---\n\nMeasure carefully.\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join(".envoy").join("agents").join("explore.md"),
            "---\nname: explore\ndescription: \"Project override\"\nmode: inherit\nmodel: flagship\n---\n\nUse repo conventions.\n",
        )
        .unwrap();

        let untrusted = discover_agents(&data, &cwd);
        assert!(untrusted.iter().any(|a| a.name == "perf"));
        let explore_u = untrusted.iter().find(|a| a.name == "explore").unwrap();
        assert_eq!(explore_u.scope, AgentScope::Builtin);

        set_workspace_trusted(&data, &cwd, true).unwrap();
        let trusted = discover_agents(&data, &cwd);
        let explore_t = trusted.iter().find(|a| a.name == "explore").unwrap();
        assert_eq!(explore_t.scope, AgentScope::Project);
        assert_eq!(explore_t.mode, SubagentMode::Inherit);
        assert_eq!(explore_t.model, "flagship");

        let perf = trusted.iter().find(|a| a.name == "perf").unwrap();
        assert_eq!(perf.tools, vec!["read", "grep"]);
        assert_eq!(perf.max_steps, 9);

        let xml = format_agents_catalog(&trusted).unwrap();
        assert!(xml.contains("<available_agents>"));
        assert!(xml.contains("name=\"perf\""));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn job_todo_monitor_and_mcp_are_not_readonly() {
        let mut spec = SubagentSpec {
            name: "look".into(),
            description: String::new(),
            mode: SubagentMode::ReadOnly,
            model: "fast".into(),
            tools: vec!["read".into(), "grep".into()],
            max_steps: 4,
            timeout_s: 30,
            system_prompt: String::new(),
            scope: AgentScope::User,
            source_path: None,
        };
        assert!(spec.is_readonly());
        for tool in ["job", "todo", "monitor", "mcp__fs__write", "exec"] {
            spec.tools = vec![tool.into()];
            assert!(!spec.is_readonly(), "{tool}");
        }
    }
}
