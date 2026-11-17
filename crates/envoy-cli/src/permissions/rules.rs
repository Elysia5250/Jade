//! Permission rule loading, explicit precedence evaluation, and persistence.
//!
//! Precedence is strictly explicit — never "more specific wins":
//!   Managed Deny > Local Deny > Hard Protected Guard > Managed Ask > Local Ask
//!   > Managed Allow > Local Allow > Mode Baseline.

use super::paths::{
    expand_tilde, is_in_workspace, is_plan_file, is_protected, resolve_path_pair,
};
use super::shell::analyze_shell;
use super::types::{
    Decision, Effect, PermissionMode, PermissionRule, PolicyFile, RuleScope, TrustStore,
};
use anyhow::Result;
use envoy_core::search::glob_match;
use serde_json::Value;
use std::path::{Path, PathBuf};

const DEFAULT_MANAGED_POLICY_PATH: &str = "/etc/envoy/policy.json";

#[derive(Debug, Clone)]
pub struct LoadedPolicies {
    pub rules: Vec<PermissionRule>,
    pub default_mode: Option<PermissionMode>,
    pub extra_rw: Vec<String>,
}

pub fn user_policy_path(data_dir: &Path) -> PathBuf {
    data_dir.join("permissions.json")
}

pub fn project_policy_path(cwd: &Path) -> PathBuf {
    cwd.join(".envoy").join("permissions.json")
}

pub fn trust_store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("trusted.json")
}

pub fn is_workspace_trusted(data_dir: &Path, cwd: &Path) -> bool {
    let canon = super::paths::canonicalize_with_missing_tail(cwd)
        .display()
        .to_string();
    let store: TrustStore = read_json(&trust_store_path(data_dir)).unwrap_or_default();
    store.trusted_dirs.contains(&canon)
}

pub fn set_workspace_trusted(data_dir: &Path, cwd: &Path, trusted: bool) -> Result<()> {
    let canon = super::paths::canonicalize_with_missing_tail(cwd)
        .display()
        .to_string();
    let path = trust_store_path(data_dir);
    let mut store: TrustStore = read_json(&path).unwrap_or_default();
    if trusted {
        store.trusted_dirs.insert(canon);
    } else {
        store.trusted_dirs.remove(&canon);
    }
    write_json(&path, &store)
}

pub fn load_policies(
    data_dir: &Path,
    cwd: &Path,
    trusted: bool,
    managed_override: Option<&Path>,
    session_rules: &[PermissionRule],
) -> LoadedPolicies {
    let mut rules = Vec::new();
    let mut default_mode = None;
    let mut extra_rw = Vec::new();

    let managed_path = managed_override
        .map(PathBuf::from)
        .or_else(|| std::env::var("ENVOY_MANAGED_POLICY").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MANAGED_POLICY_PATH));
    if let Some(p) = read_json::<PolicyFile>(&managed_path) {
        if p.default_mode.is_some() {
            default_mode = p.default_mode;
        }
        extra_rw.extend(p.extra_rw.iter().cloned());
        push_policy_rules(RuleScope::Managed, &p, &mut rules);
    }

    if let Some(p) = read_json::<PolicyFile>(&user_policy_path(data_dir)) {
        if default_mode.is_none() {
            default_mode = p.default_mode;
        }
        extra_rw.extend(p.extra_rw.iter().cloned());
        push_policy_rules(RuleScope::User, &p, &mut rules);
    }

    // Project policy is only loaded once the user has explicitly trusted `cwd`.
    if trusted {
        if let Some(p) = read_json::<PolicyFile>(&project_policy_path(cwd)) {
            extra_rw.extend(p.extra_rw.iter().cloned());
            push_policy_rules(RuleScope::Project, &p, &mut rules);
        }
    }

    rules.extend(session_rules.iter().cloned());
    LoadedPolicies {
        rules,
        default_mode,
        extra_rw,
    }
}

pub fn save_allow_rule(
    data_dir: &Path,
    cwd: &Path,
    scope: RuleScope,
    pattern: &str,
) -> Result<()> {
    save_rule(data_dir, cwd, scope, Effect::Allow, pattern)
}

pub fn save_rule(
    data_dir: &Path,
    cwd: &Path,
    scope: RuleScope,
    effect: Effect,
    pattern: &str,
) -> Result<()> {
    let path = match scope {
        RuleScope::Project => project_policy_path(cwd),
        RuleScope::User => user_policy_path(data_dir),
        RuleScope::Managed | RuleScope::Session => {
            anyhow::bail!("cannot persist rule into scope {scope:?}")
        }
    };
    let mut policy: PolicyFile = read_json(&path).unwrap_or_default();
    match effect {
        Effect::Allow => {
            if policy.deny.iter().any(|d| d == pattern) {
                anyhow::bail!("refusing to persist allow `{pattern}` because an identical deny rule exists");
            }
            if !policy.allow.iter().any(|a| a == pattern) {
                policy.allow.push(pattern.to_string());
            }
        }
        Effect::Ask => {
            if !policy.ask.iter().any(|a| a == pattern) {
                policy.ask.push(pattern.to_string());
            }
        }
        Effect::Deny => {
            policy.allow.retain(|a| a != pattern);
            if !policy.deny.iter().any(|d| d == pattern) {
                policy.deny.push(pattern.to_string());
            }
        }
    }
    write_json(&path, &policy)
}

fn push_policy_rules(scope: RuleScope, file: &PolicyFile, out: &mut Vec<PermissionRule>) {
    for pat in &file.deny {
        out.push(PermissionRule {
            scope,
            effect: Effect::Deny,
            pattern: pat.clone(),
        });
    }
    for pat in &file.ask {
        out.push(PermissionRule {
            scope,
            effect: Effect::Ask,
            pattern: pat.clone(),
        });
    }
    for pat in &file.allow {
        out.push(PermissionRule {
            scope,
            effect: Effect::Allow,
            pattern: pat.clone(),
        });
    }
}

pub fn evaluate_tool_call(
    data_dir: &Path,
    cwd: &Path,
    mode: PermissionMode,
    policies: &LoadedPolicies,
    tool: &str,
    args: &Value,
) -> Decision {
    let ctx = InspectContext::build(data_dir, cwd, tool, args);

    // 1. Managed Deny -> 2. Local Deny (never overridden by any allow).
    if let Some(rule) = find_match(&policies.rules, Effect::Deny, Some(RuleScope::Managed), &ctx) {
        return deny(format!("managed deny rule `{}`", rule.pattern), &ctx);
    }
    if let Some(rule) = find_match(&policies.rules, Effect::Deny, None, &ctx) {
        return deny(format!("deny rule `{}`", rule.pattern), &ctx);
    }

    // 3. Plan mode hard read-only gate (only own plan markdown files may write).
    if mode == PermissionMode::Plan {
        match tool {
            "write" | "edit" => {
                if !ctx.is_plan_target {
                    return deny("plan mode is read-only (only .envoy/plans/*.md may be written)", &ctx);
                }
            }
            "exec" => {
                if !ctx.shell_readonly {
                    return deny("plan mode forbids non-readonly shell commands", &ctx);
                }
            }
            _ => {}
        }
    }

    // 4. Protected-path & symlink-escape guard (cannot be silently auto-allowed by mode).
    if ctx.protected_hit
        && matches!(tool, "write" | "edit" | "read" | "exec")
        && !ctx.is_plan_target
    {
        // Explicit Managed/Local Ask overrides below will also catch; ensure at least Ask.
        if let Some(rule) = find_match(&policies.rules, Effect::Ask, Some(RuleScope::Managed), &ctx) {
            return ask(format!("managed ask rule `{}`", rule.pattern), &ctx);
        }
        return ask(
            "targets a protected path (.git, .ssh, .env, .envoy, .codex, .agents, .aws)",
            &ctx,
        );
    }
    if ctx.symlink_escape {
        return ask("path escapes workspace through a symlink", &ctx);
    }
    if ctx.parse_failed {
        return ask("shell command syntax could not be parsed cleanly", &ctx);
    }
    if ctx.uses_sudo {
        return ask("command elevates privileges via sudo/doas", &ctx);
    }

    // 5. Managed Ask -> 6. Local Ask
    if let Some(rule) = find_match(&policies.rules, Effect::Ask, Some(RuleScope::Managed), &ctx) {
        return ask(format!("managed ask rule `{}`", rule.pattern), &ctx);
    }
    if let Some(rule) = find_match(&policies.rules, Effect::Ask, None, &ctx) {
        return ask(format!("ask rule `{}`", rule.pattern), &ctx);
    }

    // 7. Managed Allow -> 8. Local Allow
    if let Some(rule) = find_match(&policies.rules, Effect::Allow, Some(RuleScope::Managed), &ctx) {
        return allow(format!("managed allow rule `{}`", rule.pattern), &ctx);
    }
    if let Some(rule) = find_match(&policies.rules, Effect::Allow, None, &ctx) {
        return allow(format!("allow rule `{}`", rule.pattern), &ctx);
    }

    // 9. Mode baseline
    mode_baseline(mode, tool, &ctx)
}

fn mode_baseline(mode: PermissionMode, tool: &str, ctx: &InspectContext) -> Decision {
    match tool {
        "say" | "ask" | "todo" | "skill" => allow("interactive control tool", ctx),
        "read" | "ls" | "glob" | "grep" | "job" => {
            if ctx.outside_workspace {
                match mode {
                    PermissionMode::Auto => allow("auto mode read outside workspace", ctx),
                    _ => ask("read/search outside workspace", ctx),
                }
            } else {
                allow("workspace read/search tool", ctx)
            }
        }
        "write" | "edit" => {
            if ctx.outside_workspace {
                ask("write outside workspace", ctx)
            } else if matches!(mode, PermissionMode::AcceptEdits | PermissionMode::Auto)
                || (mode == PermissionMode::Plan && ctx.is_plan_target)
            {
                allow(format!("{} mode workspace edit", mode.label()), ctx)
            } else {
                ask("workspace file modification", ctx)
            }
        }
        "webfetch" => ask("external web fetch", ctx),
        "exec" => {
            if ctx.destructive {
                return ask("destructive shell command", ctx);
            }
            if ctx.outside_workspace {
                return ask("shell command references path outside workspace", ctx);
            }
            if ctx.needs_net {
                return ask("shell command requests outbound network", ctx);
            }
            match mode {
                PermissionMode::Plan => {
                    if ctx.shell_readonly {
                        allow("plan mode read-only shell command", ctx)
                    } else {
                        deny("plan mode forbids mutating shell commands", ctx)
                    }
                }
                PermissionMode::Auto => allow("auto mode workspace command", ctx),
                PermissionMode::Default | PermissionMode::AcceptEdits => {
                    if ctx.shell_readonly {
                        allow("read-only shell command inside workspace", ctx)
                    } else {
                        ask("shell command execution", ctx)
                    }
                }
            }
        }
        mcp if mcp.starts_with("mcp__") => match mode {
            PermissionMode::Plan => deny("plan mode blocks external MCP tool calls", ctx),
            PermissionMode::Auto => allow("auto mode MCP tool", ctx),
            PermissionMode::Default | PermissionMode::AcceptEdits => ask("MCP tool execution", ctx),
        },
        _ => ask("unrecognized tool", ctx),
    }
}

struct InspectContext {
    match_keys: Vec<String>,
    segment_keys: Vec<Vec<String>>,
    suggested_rule: Option<String>,
    protected_hit: bool,
    symlink_escape: bool,
    outside_workspace: bool,
    is_plan_target: bool,
    parse_failed: bool,
    uses_sudo: bool,
    shell_readonly: bool,
    needs_net: bool,
    destructive: bool,
    extra_rw: Vec<String>,
}

impl InspectContext {
    fn build(data_dir: &Path, cwd: &Path, tool: &str, args: &Value) -> Self {
        let mut ctx = Self {
            match_keys: vec![tool.to_string()],
            segment_keys: Vec::new(),
            suggested_rule: None,
            protected_hit: false,
            symlink_escape: false,
            outside_workspace: false,
            is_plan_target: false,
            parse_failed: false,
            uses_sudo: false,
            shell_readonly: false,
            needs_net: false,
            destructive: false,
            extra_rw: Vec::new(),
        };

        match tool {
            "read" | "write" | "edit" | "ls" | "glob" | "grep" => {
                let raw = args["path"]
                    .as_str()
                    .or_else(|| args["root"].as_str())
                    .unwrap_or(".");
                let resolved = resolve_path_pair(cwd, raw);
                let canon_s = resolved.canonical.display().to_string();
                let rule_tool = match tool {
                    "read" | "ls" | "glob" | "grep" => "Read",
                    "write" => "Write",
                    "edit" => "Edit",
                    _ => tool,
                };
                ctx.match_keys.push(format!("{rule_tool}({raw})"));
                ctx.match_keys.push(format!("{rule_tool}({canon_s})"));
                if let Ok(rel) = resolved.canonical.strip_prefix(
                    super::paths::canonicalize_with_missing_tail(cwd),
                ) {
                    ctx.match_keys
                        .push(format!("{rule_tool}({})", rel.display()));
                }
                ctx.suggested_rule = Some(format!("{rule_tool}({canon_s})"));
                ctx.protected_hit = is_protected(&resolved);
                ctx.symlink_escape = resolved.escaped_via_symlink;
                ctx.outside_workspace = !is_in_workspace(cwd, &resolved);
                ctx.is_plan_target = is_plan_file(data_dir, cwd, &resolved);
            }
            "webfetch" => {
                let url = args["url"].as_str().unwrap_or("");
                let domain = extract_domain(url);
                ctx.match_keys.push(format!("WebFetch(domain:{domain})"));
                ctx.match_keys.push(format!("WebFetch({url})"));
                ctx.suggested_rule = Some(format!("WebFetch(domain:{domain})"));
                ctx.needs_net = true;
            }
            "exec" => {
                let cmd = args["command"].as_str().unwrap_or("").trim();
                ctx.match_keys.push(format!("Bash({cmd})"));
                let prefix = cmd.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
                if !prefix.is_empty() {
                    ctx.suggested_rule = Some(format!("Bash({prefix}*)"));
                }
                if let Some(job_cwd) = args["cwd"].as_str() {
                    let r = resolve_path_pair(cwd, job_cwd);
                    if is_protected(&r) {
                        ctx.protected_hit = true;
                    }
                    if !is_in_workspace(cwd, &r) {
                        ctx.outside_workspace = true;
                        ctx.extra_rw.push(r.canonical.display().to_string());
                    }
                }
                match analyze_shell(cmd) {
                    Ok(a) => {
                        ctx.needs_net = a.needs_net;
                        ctx.destructive = a.destructive;
                        ctx.shell_readonly = a.read_only_all;
                        ctx.uses_sudo = a.segments.iter().any(|s| s.uses_sudo);
                        for seg in &a.segments {
                            let key = format!("Bash({})", seg.argv.join(" "));
                            ctx.match_keys.push(key.clone());
                            ctx.segment_keys.push(vec![tool.to_string(), key]);
                        }
                        for p in &a.referenced_paths {
                            let r = resolve_path_pair(cwd, p);
                            if is_protected(&r) {
                                ctx.protected_hit = true;
                            }
                            if r.escaped_via_symlink {
                                ctx.symlink_escape = true;
                            }
                            if !is_in_workspace(cwd, &r) {
                                ctx.outside_workspace = true;
                                if a.write_paths.contains(p) {
                                    ctx.extra_rw.push(r.canonical.display().to_string());
                                }
                            }
                        }
                    }
                    Err(_) => {
                        ctx.parse_failed = true;
                    }
                }
            }
            mcp if mcp.starts_with("mcp__") => {
                if let Some(rest) = mcp.strip_prefix("mcp__") {
                    if let Some((srv, tool_name)) = rest.split_once("__") {
                        ctx.match_keys.push(format!("Mcp({srv}:{tool_name})"));
                        ctx.suggested_rule = Some(format!("Mcp({srv}:{tool_name})"));
                    }
                }
            }
            _ => {}
        }
        ctx
    }
}

fn find_match<'a>(
    rules: &'a [PermissionRule],
    effect: Effect,
    scope_filter: Option<RuleScope>,
    ctx: &InspectContext,
) -> Option<&'a PermissionRule> {
    if effect == Effect::Allow && ctx.segment_keys.len() > 1 {
        let mut first_hit = None;
        for seg_keys in &ctx.segment_keys {
            let hit = rules.iter().find(|r| {
                r.effect == Effect::Allow
                    && scope_filter.is_none_or(|s| r.scope == s)
                    && rule_matches(&r.pattern, seg_keys)
            })?;
            if first_hit.is_none() {
                first_hit = Some(hit);
            }
        }
        return first_hit;
    }
    rules.iter().find(|r| {
        r.effect == effect
            && scope_filter.is_none_or(|s| r.scope == s)
            && rule_matches(&r.pattern, &ctx.match_keys)
    })
}

pub fn rule_matches(pattern: &str, keys: &[String]) -> bool {
    let pat = expand_tilde(pattern.trim());
    keys.iter().any(|k| {
        if pat == *k || pat == "*" {
            return true;
        }
        if let (Some((p_tool, p_arg)), Some((k_tool, k_arg))) =
            (split_rule(&pat), split_rule(k))
        {
            if p_tool.eq_ignore_ascii_case(k_tool) {
                return glob_match(p_arg, k_arg)
                    || (p_arg.ends_with('*')
                        && k_arg.starts_with(p_arg.trim_end_matches('*')));
            }
        }
        false
    })
}

fn split_rule(raw: &str) -> Option<(&str, &str)> {
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    if close <= open {
        return None;
    }
    Some((&raw[..open], &raw[open + 1..close]))
}

pub fn is_tool_denied(policies: &LoadedPolicies, tool: &str) -> bool {
    let keys = vec![tool.to_string()];
    policies
        .rules
        .iter()
        .any(|r| r.effect == Effect::Deny && rule_matches(&r.pattern, &keys))
}

fn extract_domain(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', '?', '#', ':'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn allow(reason: impl Into<String>, ctx: &InspectContext) -> Decision {
    Decision {
        effect: Effect::Allow,
        reason: reason.into(),
        suggested_rule: ctx.suggested_rule.clone(),
        needs_net: ctx.needs_net,
        extra_rw: ctx.extra_rw.clone(),
        outside_workspace: ctx.outside_workspace,
        destructive: ctx.destructive,
    }
}

fn ask(reason: impl Into<String>, ctx: &InspectContext) -> Decision {
    Decision {
        effect: Effect::Ask,
        reason: reason.into(),
        suggested_rule: ctx.suggested_rule.clone(),
        needs_net: ctx.needs_net,
        extra_rw: ctx.extra_rw.clone(),
        outside_workspace: ctx.outside_workspace,
        destructive: ctx.destructive,
    }
}

fn deny(reason: impl Into<String>, ctx: &InspectContext) -> Decision {
    Decision {
        effect: Effect::Deny,
        reason: reason.into(),
        suggested_rule: ctx.suggested_rule.clone(),
        needs_net: ctx.needs_net,
        extra_rw: ctx.extra_rw.clone(),
        outside_workspace: ctx.outside_workspace,
        destructive: ctx.destructive,
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_json<T: serde::Serialize>(path: &Path, val: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(val)?;
    std::fs::write(path, body)?;
    Ok(())
}
