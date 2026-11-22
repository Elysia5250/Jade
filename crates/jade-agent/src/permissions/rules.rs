//! Permission rule loading, explicit precedence evaluation, and persistence.
//!
//! Precedence is strictly explicit — never "more specific wins":
//!   Managed Deny > Local Deny > Hard Protected Guard > Managed Ask > Local Ask
//!   > Managed Allow > Local Allow > Mode Baseline.

use super::match_rule::{find_match, InspectContext};
use super::types::{
    Decision, Effect, PermissionMode, PermissionRule, PolicyFile, RuleScope, TrustStore,
};
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub use super::match_rule::rule_matches;

const DEFAULT_MANAGED_POLICY_PATH: &str = "/etc/jade/policy.json";

#[derive(Debug, Clone)]
pub struct LoadedPolicies {
    pub rules: Vec<PermissionRule>,
    pub default_mode: Option<PermissionMode>,
    pub extra_rw: Vec<String>,
    pub managed_error: Option<String>,
}

pub fn user_policy_path(data_dir: &Path) -> PathBuf {
    data_dir.join("permissions.json")
}

pub fn project_policy_path(cwd: &Path) -> PathBuf {
    cwd.join(".jade").join("permissions.json")
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

fn managed_policy_path(managed_override: Option<&Path>) -> PathBuf {
    if let Some(p) = managed_override {
        return p.to_path_buf();
    }
    #[cfg(test)]
    if let Ok(env_path) = std::env::var("JADE_MANAGED_POLICY") {
        if !env_path.trim().is_empty() {
            return PathBuf::from(env_path);
        }
    }
    PathBuf::from(DEFAULT_MANAGED_POLICY_PATH)
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
    let mut managed_error = None;

    let managed_path = managed_policy_path(managed_override);
    match read_json_strict::<PolicyFile>(&managed_path) {
        Ok(Some(p)) => {
            if p.default_mode.is_some() {
                default_mode = p.default_mode;
            }
            extra_rw.extend(p.extra_rw.iter().cloned());
            push_policy_rules(RuleScope::Managed, &p, &mut rules);
        }
        Ok(None) => {}
        Err(err) => {
            managed_error = Some(err);
        }
    }

    if let Some(p) = read_json::<PolicyFile>(&user_policy_path(data_dir)) {
        if default_mode.is_none() {
            default_mode = p.default_mode;
        }
        extra_rw.extend(p.extra_rw.iter().cloned());
        push_policy_rules(RuleScope::User, &p, &mut rules);
    }

    // Project policy is only loaded once the user has explicitly trusted `cwd`.
    // Repository-controlled policy files may never widen sandbox `extra_rw` roots.
    if trusted {
        if let Some(p) = read_json::<PolicyFile>(&project_policy_path(cwd)) {
            push_policy_rules(RuleScope::Project, &p, &mut rules);
        }
    }

    // Enabled plugins may tighten policy (`deny`/`ask`) within their scope,
    // but never auto-apply `allow` rules or widen `extra_rw`.
    for plugin in crate::plugins::enabled_plugins(data_dir, cwd) {
        if plugin.scope == crate::plugins::PluginScope::Project && !trusted {
            continue;
        }
        let Some(path) = plugin.permissions_file.as_deref() else {
            continue;
        };
        let Some(p) = read_json::<PolicyFile>(path) else {
            continue;
        };
        let scope = match plugin.scope {
            crate::plugins::PluginScope::Managed => RuleScope::Managed,
            crate::plugins::PluginScope::User => RuleScope::User,
            crate::plugins::PluginScope::Project => RuleScope::Project,
        };
        push_plugin_policy_rules(scope, &p, &mut rules);
    }

    let extra_rw = super::paths::sanitize_extra_rw(cwd, &extra_rw);
    rules.extend(session_rules.iter().cloned());
    LoadedPolicies {
        rules,
        default_mode,
        extra_rw,
        managed_error,
    }
}

pub fn save_allow_rule(data_dir: &Path, cwd: &Path, scope: RuleScope, pattern: &str) -> Result<()> {
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
                anyhow::bail!(
                    "refusing to persist allow `{pattern}` because an identical deny rule exists"
                );
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

pub fn save_default_mode(data_dir: &Path, mode: PermissionMode) -> Result<()> {
    let path = user_policy_path(data_dir);
    let mut policy: PolicyFile = read_json(&path).unwrap_or_default();
    policy.default_mode = Some(mode);
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

fn push_plugin_policy_rules(scope: RuleScope, file: &PolicyFile, out: &mut Vec<PermissionRule>) {
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
}

#[derive(Debug, Clone, Copy)]
pub struct EvalOptions<'a> {
    pub mcp_identity: Option<(&'a str, &'a str)>,
    pub sandbox_enforces_workspace: bool,
}

pub fn evaluate_tool_call(
    data_dir: &Path,
    cwd: &Path,
    mode: PermissionMode,
    policies: &LoadedPolicies,
    tool: &str,
    args: &Value,
) -> Decision {
    evaluate_tool_call_with_options(
        data_dir,
        cwd,
        mode,
        policies,
        tool,
        args,
        EvalOptions {
            mcp_identity: None,
            sandbox_enforces_workspace: sandbox_backend_isolated(),
        },
    )
}

pub fn evaluate_tool_call_with_options(
    data_dir: &Path,
    cwd: &Path,
    mode: PermissionMode,
    policies: &LoadedPolicies,
    tool: &str,
    args: &Value,
    opts: EvalOptions<'_>,
) -> Decision {
    let ctx = InspectContext::build(data_dir, cwd, tool, args, opts.mcp_identity);

    if let Some(err) = &policies.managed_error {
        return deny(
            format!("managed policy load failed (fail-closed): {err}"),
            &ctx,
        );
    }

    // 1. Managed Deny -> 2. Local Deny (never overridden by any allow).
    if let Some(rule) = find_match(
        &policies.rules,
        Effect::Deny,
        Some(RuleScope::Managed),
        &ctx,
    ) {
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
                    return deny(
                        "plan mode is read-only (only .jade/plans/*.md may be written)",
                        &ctx,
                    );
                }
            }
            "exec" => {
                if !ctx.shell_readonly {
                    return deny("plan mode forbids non-readonly shell commands", &ctx);
                }
                if matches!(
                    jade_core::sandbox::probe_backend(),
                    jade_core::sandbox::SandboxBackend::None
                ) {
                    return deny(
                        "plan mode forbids shell execution without a read-only OS sandbox",
                        &ctx,
                    );
                }
            }
            "job" if args["action"].as_str() == Some("cancel") => {
                return deny("plan mode forbids cancelling jobs", &ctx);
            }
            _ => {}
        }
    }

    // 3b. Relaxed modes stop prompting for everything the OS sandbox already
    // contains: `turbo` always, `auto` once an enforcing workspace sandbox is
    // actually active (not overridden to `off`/`danger-full-access`), the call
    // stays inside the workspace (or temp dir) for mutating/exec tools, and
    // does not request non-allowlisted outbound network access.
    // Protected paths still fall through to the guard below, explicit deny
    // rules above still win, and a managed-policy load error still fails closed.
    let auto_unallowlisted_net = mode == PermissionMode::Auto
        && matches!(tool, "exec" | "webfetch")
        && ctx.needs_net
        && !ctx.net_allowlisted;
    let auto_outside_workspace = mode == PermissionMode::Auto
        && ctx.outside_workspace
        && !ctx.is_temp_target
        && matches!(tool, "write" | "edit" | "exec");
    let relaxed = mode == PermissionMode::Turbo
        || (mode == PermissionMode::Auto
            && opts.sandbox_enforces_workspace
            && !auto_unallowlisted_net
            && !auto_outside_workspace
            && !ctx.uses_sudo);
    if relaxed && !ctx.protected_hit {
        return allow(
            if mode == PermissionMode::Turbo {
                "turbo mode allows every tool call"
            } else {
                "auto mode with an enforcing sandbox"
            },
            &ctx,
        );
    }

    // 4. Protected-path & symlink-escape guard (cannot be silently auto-allowed by mode).
    if ctx.protected_hit
        && matches!(
            tool,
            "write" | "edit" | "read" | "exec" | "ls" | "glob" | "grep"
        )
        && !ctx.is_plan_target
    {
        // Explicit Managed/Local Ask overrides below will also catch; ensure at least Ask.
        if let Some(rule) = find_match(&policies.rules, Effect::Ask, Some(RuleScope::Managed), &ctx)
        {
            return ask(format!("managed ask rule `{}`", rule.pattern), &ctx);
        }
        return ask(
            "targets a protected path (.git, .ssh, .env, .jade, .codex, .agents, .aws)",
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
    if let Some(rule) = find_match(
        &policies.rules,
        Effect::Allow,
        Some(RuleScope::Managed),
        &ctx,
    ) {
        return allow(format!("managed allow rule `{}`", rule.pattern), &ctx);
    }
    if let Some(rule) = find_match(&policies.rules, Effect::Allow, None, &ctx) {
        return allow(format!("allow rule `{}`", rule.pattern), &ctx);
    }

    // 9. Mode baseline
    mode_baseline(mode, tool, &ctx, opts.sandbox_enforces_workspace)
}

fn sandbox_backend_isolated() -> bool {
    jade_core::sandbox::capabilities().enforces_workspace_boundary()
}

fn mode_baseline(
    mode: PermissionMode,
    tool: &str,
    ctx: &InspectContext,
    sandbox_enforces_workspace: bool,
) -> Decision {
    match tool {
        "say" | "ask" | "todo" | "skill" | "delegate" | "monitor" | "mode" => {
            allow("interactive control tool", ctx)
        }
        "read" | "ls" | "glob" | "grep" | "job" => {
            if ctx.outside_workspace {
                match mode {
                    PermissionMode::Auto | PermissionMode::Turbo => {
                        allow("relaxed mode read outside workspace", ctx)
                    }
                    _ => ask("read/search outside workspace", ctx),
                }
            } else {
                allow("workspace read/search tool", ctx)
            }
        }
        "write" | "edit" => {
            if matches!(mode, PermissionMode::Auto | PermissionMode::Turbo)
                && (!ctx.outside_workspace || ctx.is_temp_target)
            {
                allow("relaxed mode workspace edit", ctx)
            } else if ctx.outside_workspace {
                ask("write outside workspace", ctx)
            } else if mode == PermissionMode::AcceptEdits
                || (mode == PermissionMode::Plan && ctx.is_plan_target)
            {
                allow(format!("{} mode workspace edit", mode.label()), ctx)
            } else {
                ask("workspace file modification", ctx)
            }
        }
        "webfetch" => match mode {
            PermissionMode::Turbo => allow("turbo mode web fetch", ctx),
            _ if ctx.net_allowlisted => allow("allowlisted web fetch", ctx),
            _ => ask("external web fetch", ctx),
        },
        "exec" => {
            // `turbo`, and `auto` with an enforcing sandbox, already returned
            // Allow above. What is left is conservatively ask-first.
            if ctx.destructive {
                return ask("destructive shell command", ctx);
            }
            if ctx.outside_workspace {
                return ask("shell command references path outside workspace", ctx);
            }
            if ctx.needs_net && !ctx.net_allowlisted {
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
                PermissionMode::Auto | PermissionMode::Default | PermissionMode::AcceptEdits => {
                    // Only reached without an enforcing sandbox for `auto`, and
                    // always for `default` / `accept-edits`.
                    if ctx.shell_readonly
                        || (sandbox_enforces_workspace && mode != PermissionMode::Auto)
                    {
                        allow(
                            if ctx.shell_readonly {
                                "read-only shell command inside workspace"
                            } else {
                                "sandboxed workspace command"
                            },
                            ctx,
                        )
                    } else {
                        ask("shell command execution", ctx)
                    }
                }
                PermissionMode::Turbo => allow("turbo mode allows every tool call", ctx),
            }
        }
        "mcp_search" => allow("read-only MCP tool catalog search", ctx),
        "mcp_resource" => match mode {
            PermissionMode::Plan => deny("plan mode blocks external MCP resource reads", ctx),
            PermissionMode::Auto | PermissionMode::Turbo => {
                allow("relaxed mode MCP resource read", ctx)
            }
            PermissionMode::Default | PermissionMode::AcceptEdits => {
                ask("MCP resource access", ctx)
            }
        },
        mcp if mcp.starts_with("mcp__") => match mode {
            PermissionMode::Plan => deny("plan mode blocks external MCP tool calls", ctx),
            PermissionMode::Auto | PermissionMode::Turbo => allow("relaxed mode MCP tool", ctx),
            PermissionMode::Default | PermissionMode::AcceptEdits => ask("MCP tool execution", ctx),
        },
        _ => ask("unrecognized tool", ctx),
    }
}

pub fn is_tool_denied(policies: &LoadedPolicies, tool: &str) -> bool {
    if policies.managed_error.is_some() {
        return true;
    }
    let keys = vec![tool.to_string()];
    policies
        .rules
        .iter()
        .any(|r| r.effect == Effect::Deny && rule_matches(&r.pattern, &keys))
}

fn allow(reason: impl Into<String>, ctx: &InspectContext) -> Decision {
    make_decision(Effect::Allow, reason, ctx)
}

fn ask(reason: impl Into<String>, ctx: &InspectContext) -> Decision {
    make_decision(Effect::Ask, reason, ctx)
}

fn deny(reason: impl Into<String>, ctx: &InspectContext) -> Decision {
    make_decision(Effect::Deny, reason, ctx)
}

fn make_decision(effect: Effect, reason: impl Into<String>, ctx: &InspectContext) -> Decision {
    Decision {
        effect,
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

fn read_json_strict<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> std::result::Result<Option<T>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{}: invalid JSON ({e})", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: read error ({e})", path.display())),
    }
}

fn write_json<T: serde::Serialize>(path: &Path, val: &T) -> Result<()> {
    let body = serde_json::to_string_pretty(val)?;
    jade_core::fs::write_atomic_sync(path, body.as_bytes(), 0o600)?;
    Ok(())
}
