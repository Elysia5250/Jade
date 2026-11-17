//! Deterministic hook loading, workspace-trust gating, matching, and strict ordering.

use super::types::{
    HookContext, HookEvent, HookRule, HookScope, StageCapability, DEFAULT_HOOK_TIMEOUT_MS,
    MAX_HOOK_TIMEOUT_MS, SHUTDOWN_HOOK_TIMEOUT_MS,
};
use envoy_core::search::glob_match;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_MANAGED_HOOKS_PATH: &str = "/etc/envoy/hooks.json";

#[derive(Debug, Clone, Default, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: BTreeMap<String, Vec<HookEntry>>,
}

#[derive(Debug, Clone, Deserialize)]
struct HookEntry {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    matcher: Option<String>,
    command: String,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    fail_closed: Option<bool>,
    #[serde(default)]
    always_run: bool,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

pub fn user_hooks_path(data_dir: &Path) -> PathBuf {
    data_dir.join("hooks.json")
}

pub fn project_hooks_path(cwd: &Path) -> PathBuf {
    cwd.join(".envoy").join("hooks.json")
}

fn managed_hooks_path(override_path: Option<&Path>) -> PathBuf {
    override_path
        .map(PathBuf::from)
        .or_else(|| std::env::var("ENVOY_MANAGED_HOOKS").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MANAGED_HOOKS_PATH))
}

fn read_hooks_file(path: &Path) -> Option<HooksFile> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn effective_timeout_ms(event: HookEvent, raw: Option<u64>) -> u64 {
    if matches!(event, HookEvent::SessionEnd) {
        return raw
            .unwrap_or(SHUTDOWN_HOOK_TIMEOUT_MS)
            .clamp(100, SHUTDOWN_HOOK_TIMEOUT_MS);
    }
    raw.unwrap_or(DEFAULT_HOOK_TIMEOUT_MS)
        .clamp(100, MAX_HOOK_TIMEOUT_MS)
}

fn push_scope_rules(scope: HookScope, file: HooksFile, out: &mut Vec<HookRule>) {
    for (ev_name, entries) in file.hooks {
        let Some(event) = HookEvent::parse(&ev_name) else {
            continue;
        };
        for (decl_index, entry) in entries.into_iter().enumerate() {
            if !entry.enabled || entry.command.trim().is_empty() {
                continue;
            }
            let id = entry
                .id
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| format!("{}:{}:{}", scope.label(), event.label(), decl_index));
            let fail_closed = entry.fail_closed.unwrap_or_else(|| {
                scope == HookScope::Managed && !matches!(event.capability(), StageCapability::ObserveOnly)
            });
            let timeout_ms = effective_timeout_ms(event, entry.timeout_ms);
            out.push(HookRule {
                id,
                scope,
                priority: entry.priority,
                decl_index,
                event,
                matcher: entry.matcher.filter(|m| !m.trim().is_empty()),
                command: entry.command,
                timeout_ms,
                fail_closed,
                always_run: entry.always_run,
            });
        }
    }
}

/// Load and sort hooks strictly by `(HookScope, priority, decl_index)`.
/// Project hooks (`.envoy/hooks.json`) are loaded only when `trusted` is true.
pub fn load_hooks(
    data_dir: &Path,
    cwd: &Path,
    trusted: bool,
    managed_override: Option<&Path>,
) -> Vec<HookRule> {
    let mut rules = Vec::new();

    let managed_path = managed_hooks_path(managed_override);
    if let Some(file) = read_hooks_file(&managed_path) {
        push_scope_rules(HookScope::Managed, file, &mut rules);
    }

    if let Some(file) = read_hooks_file(&user_hooks_path(data_dir)) {
        push_scope_rules(HookScope::User, file, &mut rules);
    }

    if trusted {
        if let Some(file) = read_hooks_file(&project_hooks_path(cwd)) {
            push_scope_rules(HookScope::Project, file, &mut rules);
        }
    }

    rules.sort_by_key(HookRule::sort_key);
    rules
}

pub fn matches_rule(rule: &HookRule, ctx: &HookContext) -> bool {
    if rule.event != ctx.event {
        return false;
    }
    let Some(matcher) = rule.matcher.as_deref() else {
        return true;
    };
    let m = matcher.trim();
    if m.is_empty() || m == "*" {
        return true;
    }
    let subject = match ctx.event {
        HookEvent::PreToolUse | HookEvent::PostToolUse | HookEvent::PostToolUseFailure => {
            ctx.tool_name.as_deref().unwrap_or("")
        }
        HookEvent::SubagentStart | HookEvent::SubagentStop => {
            ctx.subagent_name.as_deref().unwrap_or("")
        }
        HookEvent::PreModelCall | HookEvent::PostModelCall => {
            ctx.model_id.as_deref().unwrap_or("")
        }
        _ => ctx.event.label(),
    };
    m.split('|')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .any(|pat| glob_match(pat, subject))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "envoy-hooks-cfg-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn loads_in_strict_scope_priority_and_declaration_order_with_trust_gate() {
        let dir = tmpdir("order");
        let data = dir.join("data");
        let cwd = dir.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(cwd.join(".envoy")).unwrap();
        let managed = dir.join("managed.json");

        std::fs::write(
            &managed,
            r#"{
              "hooks": {
                "PreToolUse": [
                  { "id": "m-late", "priority": 20, "command": "echo m2" },
                  { "id": "m-early", "priority": 5, "command": "echo m1" }
                ]
              }
            }"#,
        )
        .unwrap();
        std::fs::write(
            user_hooks_path(&data),
            r#"{
              "hooks": {
                "PreToolUse": [
                  { "id": "u-first", "command": "echo u1" },
                  { "id": "u-second", "command": "echo u2" },
                  { "id": "u-prio-neg", "priority": -10, "command": "echo u0" }
                ]
              }
            }"#,
        )
        .unwrap();
        std::fs::write(
            project_hooks_path(&cwd),
            r#"{
              "hooks": {
                "PreToolUse": [
                  { "id": "p-one", "priority": -999, "command": "echo p1" }
                ]
              }
            }"#,
        )
        .unwrap();

        let untrusted = load_hooks(&data, &cwd, false, Some(&managed));
        let ids_untrusted: Vec<_> = untrusted.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids_untrusted,
            vec!["m-early", "m-late", "u-prio-neg", "u-first", "u-second"]
        );
        // Managed non-observe defaults to fail_closed=true; User defaults to fail_closed=false.
        assert!(untrusted[0].fail_closed);
        assert!(!untrusted[2].fail_closed);

        let trusted = load_hooks(&data, &cwd, true, Some(&managed));
        let ids_trusted: Vec<_> = trusted.iter().map(|r| r.id.as_str()).collect();
        // Even with priority -999, Project still executes strictly AFTER Managed and User.
        assert_eq!(
            ids_trusted,
            vec!["m-early", "m-late", "u-prio-neg", "u-first", "u-second", "p-one"]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn matcher_supports_pipe_alternation_and_globs() {
        let mut ctx = HookContext::new(
            "s1",
            PathBuf::from("/tmp"),
            "local",
            "default",
            HookEvent::PreToolUse,
            BTreeMap::new(),
        );
        ctx.tool_name = Some("mcp__github__search".into());
        let r = HookRule {
            id: "r1".into(),
            scope: HookScope::User,
            priority: 0,
            decl_index: 0,
            event: HookEvent::PreToolUse,
            matcher: Some("write|edit|mcp__*".into()),
            command: "true".into(),
            timeout_ms: 1000,
            fail_closed: false,
            always_run: false,
        };
        assert!(matches_rule(&r, &ctx));
        ctx.tool_name = Some("read".into());
        assert!(!matches_rule(&r, &ctx));
    }
}
