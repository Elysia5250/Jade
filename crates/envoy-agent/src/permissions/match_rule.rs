//! Permission inspection context builder, rule matching, domain parsing,
//! and safe rule suggestion helpers.

use super::paths::{
    expand_tilde, is_in_workspace, is_plan_file, is_protected, is_temp_path, resolve_path_pair,
};
use super::shell::{analyze_shell, ShellAnalysis};
use super::types::{Effect, PermissionRule, RuleScope};
use serde_json::Value;
use std::path::Path;

pub(super) struct InspectContext {
    pub(super) match_keys: Vec<String>,
    pub(super) segment_keys: Vec<Vec<String>>,
    pub(super) suggested_rule: Option<String>,
    pub(super) protected_hit: bool,
    pub(super) symlink_escape: bool,
    pub(super) outside_workspace: bool,
    pub(super) is_temp_target: bool,
    pub(super) is_plan_target: bool,
    pub(super) parse_failed: bool,
    pub(super) uses_sudo: bool,
    pub(super) shell_readonly: bool,
    pub(super) needs_net: bool,
    pub(super) net_allowlisted: bool,
    pub(super) destructive: bool,
    pub(super) catastrophic: bool,
    pub(super) extra_rw: Vec<String>,
}

impl InspectContext {
    pub(super) fn build(
        data_dir: &Path,
        cwd: &Path,
        tool: &str,
        args: &Value,
        mcp_identity: Option<(&str, &str)>,
    ) -> Self {
        let mut ctx = Self {
            match_keys: vec![tool.to_string()],
            segment_keys: Vec::new(),
            suggested_rule: None,
            protected_hit: false,
            symlink_escape: false,
            outside_workspace: false,
            is_temp_target: false,
            is_plan_target: false,
            parse_failed: false,
            uses_sudo: false,
            shell_readonly: false,
            needs_net: false,
            net_allowlisted: false,
            destructive: false,
            catastrophic: false,
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
                if let Ok(rel) = resolved
                    .canonical
                    .strip_prefix(super::paths::canonicalize_with_missing_tail(cwd))
                {
                    ctx.match_keys
                        .push(format!("{rule_tool}({})", rel.display()));
                }
                ctx.suggested_rule = Some(format!("{rule_tool}({canon_s})"));
                ctx.protected_hit = is_protected(&resolved);
                ctx.is_temp_target = is_temp_path(&resolved);
                ctx.symlink_escape = resolved.escaped_via_symlink;
                ctx.outside_workspace = !is_in_workspace(cwd, &resolved);
                ctx.is_plan_target = is_plan_file(data_dir, cwd, &resolved);
            }
            "webfetch" => {
                let url = args["url"].as_str().unwrap_or("");
                let domain = extract_domain(url);
                ctx.match_keys.push(format!("WebFetch(domain:{domain})"));
                ctx.match_keys.push(format!("WebFetch({url})"));
                if !domain.is_empty() {
                    ctx.suggested_rule = Some(format!("WebFetch(domain:{domain})"));
                }
                ctx.needs_net = true;
                ctx.net_allowlisted = super::net_allowlist::webfetch_url_allowlisted(args);
            }
            "exec" => {
                let cmd = args["command"].as_str().unwrap_or("").trim();
                ctx.match_keys.push(format!("Bash({cmd})"));
                if let Some(job_cwd) = args["cwd"].as_str() {
                    let r = resolve_path_pair(cwd, job_cwd);
                    if is_protected(&r) {
                        ctx.protected_hit = true;
                    }
                    if r.escaped_via_symlink {
                        ctx.symlink_escape = true;
                    }
                    if !is_in_workspace(cwd, &r) && !is_temp_path(&r) {
                        ctx.outside_workspace = true;
                        ctx.extra_rw.push(r.canonical.display().to_string());
                    }
                }
                match analyze_shell(cmd) {
                    Ok(a) => {
                        ctx.suggested_rule = suggest_bash_rule(cmd, Some(&a));
                        ctx.needs_net = a.needs_net;
                        ctx.net_allowlisted =
                            super::net_allowlist::exec_segments_all_allowlisted(&a);
                        ctx.destructive = a.destructive;
                        ctx.catastrophic = a.catastrophic;
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
                            if !is_in_workspace(cwd, &r) && !is_temp_path(&r) {
                                ctx.outside_workspace = true;
                                if a.write_paths.contains(p) {
                                    ctx.extra_rw.push(r.canonical.display().to_string());
                                }
                            }
                        }
                    }
                    Err(_) => {
                        ctx.parse_failed = true;
                        ctx.suggested_rule = None;
                    }
                }
            }
            mcp if mcp.starts_with("mcp__") => {
                if let Some((srv, tool_name)) = mcp_identity {
                    ctx.match_keys.push(format!("Mcp({srv}:{tool_name})"));
                    let san_srv = crate::mcp::config::sanitize_ident(srv);
                    let san_tool = crate::mcp::config::sanitize_ident(tool_name);
                    if san_srv != srv || san_tool != tool_name {
                        ctx.match_keys.push(format!("Mcp({san_srv}:{san_tool})"));
                    }
                    ctx.suggested_rule = Some(format!("Mcp({srv}:{tool_name})"));
                } else if let Some(rest) = mcp.strip_prefix("mcp__") {
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

fn suggest_bash_rule(cmd: &str, analysis: Option<&ShellAnalysis>) -> Option<String> {
    let a = analysis?;
    if a.segments.len() != 1
        || a.destructive
        || a.catastrophic
        || a.segments[0].uses_sudo
        || a.segments[0].uses_xargs
    {
        return None;
    }
    let raw_first = cmd.split_whitespace().next()?;
    let raw_bin = raw_first.rsplit('/').next().unwrap_or(raw_first);
    let unwrapped_bin = a.segments[0]
        .argv
        .first()
        .map(|s| s.rsplit('/').next().unwrap_or(s.as_str()))?;
    if is_unsafe_suggest_bin(raw_bin) || is_unsafe_suggest_bin(unwrapped_bin) {
        return None;
    }
    let prefix = cmd.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    (!prefix.is_empty()).then(|| format!("Bash({prefix}*)"))
}

fn is_unsafe_suggest_bin(bin: &str) -> bool {
    matches!(
        bin,
        "sh" | "bash"
            | "zsh"
            | "dash"
            | "fish"
            | "ksh"
            | "csh"
            | "tcsh"
            | "eval"
            | "exec"
            | "source"
            | "."
            | "env"
            | "nohup"
            | "time"
            | "command"
            | "nice"
            | "ionice"
            | "stdbuf"
            | "timeout"
            | "sudo"
            | "doas"
            | "su"
            | "xargs"
            | "python"
            | "python3"
            | "python2"
            | "node"
            | "nodejs"
            | "deno"
            | "bun"
            | "perl"
            | "ruby"
            | "php"
            | "lua"
            | "awk"
            | "gawk"
            | "mawk"
            | "nawk"
    )
}

pub(super) fn find_match<'a>(
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

/// Match a permission pattern against candidate rule keys.
///
/// Unlike workspace file globbing, permission rules NEVER fall back to
/// matching only a trailing filename after `/` (preventing `Bash(cargo test*)`
/// from matching `Bash(curl http://evil/cargo test)`).
pub fn rule_matches(pattern: &str, keys: &[String]) -> bool {
    let pat = expand_tilde(pattern.trim());
    keys.iter().any(|k| {
        if pat == *k || pat == "*" {
            return true;
        }
        if let (Some((p_tool, p_arg)), Some((k_tool, k_arg))) = (split_rule(&pat), split_rule(k)) {
            if !p_tool.eq_ignore_ascii_case(k_tool) {
                return false;
            }
            if matches!(
                p_tool.to_ascii_lowercase().as_str(),
                "read" | "write" | "edit"
            ) {
                let p_expanded = expand_tilde(p_arg);
                return match_path_pattern(&p_expanded, k_arg);
            }
            return wildcard_match(p_arg, k_arg);
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

fn match_path_pattern(pattern: &str, path: &str) -> bool {
    for alt in expand_braces(pattern) {
        let pat = alt.trim_start_matches("./");
        let p = path.trim_start_matches("./");
        let pat_segs: Vec<&str> = pat.split('/').collect();
        let path_segs: Vec<&str> = p.split('/').collect();
        if match_segments(&pat_segs, &path_segs) {
            return true;
        }
    }
    false
}

fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(start) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let Some(rel_end) = pattern[start + 1..].find('}') else {
        return vec![pattern.to_string()];
    };
    let end = start + 1 + rel_end;
    let prefix = &pattern[..start];
    let suffix = &pattern[end + 1..];
    let mut out = Vec::new();
    for item in pattern[start + 1..end].split(',') {
        let combined = format!("{prefix}{item}{suffix}");
        out.extend(expand_braces(&combined));
    }
    out
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    if pat.is_empty() {
        return path.is_empty();
    }
    if pat[0] == "**" {
        if match_segments(&pat[1..], path) {
            return true;
        }
        return !path.is_empty() && match_segments(pat, &path[1..]);
    }
    if path.is_empty() {
        return false;
    }
    wildcard_match(pat[0], path[0]) && match_segments(&pat[1..], &path[1..])
}

pub(super) fn wildcard_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut match_t) = (None::<usize>, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = Some(pi);
            match_t = ti;
            pi += 1;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            match_t += 1;
            ti = match_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub(super) fn extract_domain(raw: &str) -> String {
    let candidate = raw.trim();
    let parsed = reqwest::Url::parse(candidate)
        .or_else(|_| reqwest::Url::parse(&format!("https://{candidate}")));
    parsed
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase()
}
