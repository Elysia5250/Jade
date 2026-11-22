//! MCP client server configuration discovery with workspace-trust gating
//! and `Managed > User > Project` precedence.

use crate::permissions::is_workspace_trusted;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_MANAGED_MCP_PATH: &str = "/etc/jade/mcp.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum McpScope {
    Managed,
    User,
    Project,
}

impl McpScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransportConfig {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub scope: McpScope,
    pub transport: McpTransportConfig,
    pub timeout_s: u64,
    pub disabled: bool,
}

#[derive(Debug, Clone, Default)]
pub struct McpConfigReport {
    pub servers: Vec<McpServerConfig>,
    pub skipped_untrusted: Vec<PathBuf>,
    pub managed_error: Option<String>,
    pub fingerprint: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct RawMcpFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, RawServerEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RawServerEntry {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    timeout_s: Option<u64>,
}

fn managed_mcp_path(override_path: Option<&Path>) -> PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    #[cfg(test)]
    if let Ok(env_path) = std::env::var("JADE_MANAGED_MCP") {
        if !env_path.trim().is_empty() {
            return PathBuf::from(env_path);
        }
    }
    PathBuf::from(DEFAULT_MANAGED_MCP_PATH)
}

fn read_mcp_file_strict(path: &Path) -> Result<Option<RawMcpFile>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<RawMcpFile>(&text)
            .map(Some)
            .map_err(|e| format!("{}: invalid JSON ({e})", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: read error ({e})", path.display())),
    }
}

pub fn load_mcp_configs(data_dir: &Path, cwd: &Path) -> McpConfigReport {
    load_mcp_configs_with_managed(data_dir, cwd, None)
}

pub fn load_mcp_configs_with_managed(
    data_dir: &Path,
    cwd: &Path,
    managed_override: Option<&Path>,
) -> McpConfigReport {
    let extra: Vec<(McpScope, PathBuf)> = crate::plugins::enabled_plugins(data_dir, cwd)
        .into_iter()
        .filter_map(|p| {
            let file = p.mcp_file?;
            let scope = match p.scope {
                crate::plugins::PluginScope::Managed => McpScope::Managed,
                crate::plugins::PluginScope::User => McpScope::User,
                crate::plugins::PluginScope::Project => McpScope::Project,
            };
            Some((scope, file))
        })
        .collect();
    load_mcp_configs_with_extra(data_dir, cwd, managed_override, &extra)
}

/// Load MCP server configs in strict `Managed > User > Project` precedence,
/// optionally folding in extra `(scope, path)` sources (used by plugins).
pub(crate) fn load_mcp_configs_with_extra(
    data_dir: &Path,
    cwd: &Path,
    managed_override: Option<&Path>,
    extra_sources: &[(McpScope, PathBuf)],
) -> McpConfigReport {
    let mut report = McpConfigReport::default();
    let mut by_name: BTreeMap<String, McpServerConfig> = BTreeMap::new();

    let managed_path = managed_mcp_path(managed_override);
    match read_mcp_file_strict(&managed_path) {
        Ok(Some(parsed)) => apply_parsed(parsed, McpScope::Managed, &managed_path, cwd, true, &mut by_name),
        Ok(None) => {}
        Err(err) => {
            report.managed_error = Some(err);
            report.fingerprint = compute_fingerprint(&report);
            return report;
        }
    }
    for (scope, path) in extra_sources.iter().filter(|(s, _)| *s == McpScope::Managed) {
        load_file(path, *scope, cwd, false, &mut by_name);
    }

    load_file(
        &data_dir.join("mcp.json"),
        McpScope::User,
        cwd,
        false,
        &mut by_name,
    );
    for (scope, path) in extra_sources.iter().filter(|(s, _)| *s == McpScope::User) {
        load_file(path, *scope, cwd, false, &mut by_name);
    }

    let project_candidates = [cwd.join(".mcp.json"), cwd.join(".jade").join("mcp.json")];
    if is_workspace_trusted(data_dir, cwd) {
        for path in project_candidates {
            load_file(&path, McpScope::Project, cwd, false, &mut by_name);
        }
        for (scope, path) in extra_sources.iter().filter(|(s, _)| *s == McpScope::Project) {
            load_file(path, *scope, cwd, false, &mut by_name);
        }
    } else {
        for path in project_candidates {
            if path.is_file() {
                report.skipped_untrusted.push(path);
            }
        }
        for (_, path) in extra_sources.iter().filter(|(s, _)| *s == McpScope::Project) {
            if path.is_file() {
                report.skipped_untrusted.push(path.clone());
            }
        }
    }

    report.servers = by_name.into_values().collect();
    report.fingerprint = compute_fingerprint(&report);
    report
}

fn load_file(
    path: &Path,
    scope: McpScope,
    workspace_cwd: &Path,
    allow_overwrite: bool,
    out: &mut BTreeMap<String, McpServerConfig>,
) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(parsed) = serde_json::from_str::<RawMcpFile>(&text) else {
        return;
    };
    apply_parsed(parsed, scope, path, workspace_cwd, allow_overwrite, out);
}

fn apply_parsed(
    parsed: RawMcpFile,
    scope: McpScope,
    path: &Path,
    workspace_cwd: &Path,
    allow_overwrite: bool,
    out: &mut BTreeMap<String, McpServerConfig>,
) {
    for (name, raw) in parsed.mcp_servers {
        if !allow_overwrite && out.contains_key(&name) {
            tracing::warn!(
                server = %name,
                scope = scope.label(),
                path = %path.display(),
                "ignoring MCP server overriding higher-scope config"
            );
            continue;
        }
        let kind = raw
            .kind
            .as_deref()
            .or(raw.transport.as_deref())
            .unwrap_or("")
            .to_ascii_lowercase();
        let transport = if kind == "http"
            || kind == "streamable-http"
            || (raw.url.is_some() && raw.command.is_none())
        {
            let Some(url) = raw.url.filter(|u| !u.trim().is_empty()) else {
                continue;
            };
            McpTransportConfig::Http {
                url,
                headers: raw.headers,
            }
        } else {
            let Some(command) = raw.command.filter(|c| !c.trim().is_empty()) else {
                continue;
            };
            let cwd = raw.cwd.map(|c| {
                let p = PathBuf::from(c);
                if p.is_absolute() {
                    p
                } else {
                    workspace_cwd.join(p)
                }
            });
            McpTransportConfig::Stdio {
                command,
                args: raw.args,
                env: raw.env,
                cwd,
            }
        };
        out.insert(
            name.clone(),
            McpServerConfig {
                name,
                scope,
                transport,
                timeout_s: raw.timeout_s.unwrap_or(20).clamp(1, 300),
                disabled: raw.disabled,
            },
        );
    }
}

fn compute_fingerprint(report: &McpConfigReport) -> String {
    let mut hasher = Sha256::new();
    if let Some(err) = &report.managed_error {
        hasher.update(b"managed_error:");
        hasher.update(err.as_bytes());
        hasher.update([0u8]);
    }
    for p in &report.skipped_untrusted {
        hasher.update(b"skip:");
        hasher.update(p.to_string_lossy().as_bytes());
        hasher.update([0u8]);
    }
    for s in &report.servers {
        hasher.update(s.name.as_bytes());
        hasher.update([0u8]);
        hasher.update(s.scope.label().as_bytes());
        hasher.update([0u8]);
        hasher.update(s.timeout_s.to_le_bytes());
        hasher.update([u8::from(s.disabled)]);
        match &s.transport {
            McpTransportConfig::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                hasher.update(b"stdio");
                hasher.update(command.as_bytes());
                hasher.update([0u8]);
                for a in args {
                    hasher.update(a.as_bytes());
                    hasher.update([0u8]);
                }
                for (k, v) in env {
                    hasher.update(k.as_bytes());
                    hasher.update([b'=']);
                    hasher.update(v.as_bytes());
                    hasher.update([0u8]);
                }
                if let Some(dir) = cwd {
                    hasher.update(dir.to_string_lossy().as_bytes());
                }
            }
            McpTransportConfig::Http { url, headers } => {
                hasher.update(b"http");
                hasher.update(url.as_bytes());
                hasher.update([0u8]);
                for (k, v) in headers {
                    hasher.update(k.as_bytes());
                    hasher.update([b':']);
                    hasher.update(v.as_bytes());
                    hasher.update([0u8]);
                }
            }
        }
        hasher.update([0xffu8]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

pub fn sanitize_ident(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("item");
    }
    out
}

pub fn qualify_tool_name(server: &str, tool: &str) -> String {
    let full = format!("mcp__{}__{}", sanitize_ident(server), sanitize_ident(tool));
    if full.len() <= 64 {
        return full;
    }
    let mut hasher = Sha256::new();
    hasher.update(full.as_bytes());
    let digest = hasher.finalize();
    let suffix = format!("_{:02x}{:02x}{:02x}", digest[0], digest[1], digest[2]);
    let keep = 64usize.saturating_sub(suffix.len());
    format!("{}{}", &full[..keep], suffix)
}

pub fn safe_stdio_env(
    declared: &BTreeMap<String, String>,
    scope: McpScope,
) -> BTreeMap<String, String> {
    const SAFE_HOST_VARS: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "LANG",
        "LC_ALL",
        "TERM",
        "TMPDIR",
        "XDG_RUNTIME_DIR",
    ];
    let mut out = BTreeMap::new();
    for key in SAFE_HOST_VARS {
        if let Ok(val) = std::env::var(key) {
            out.insert((*key).to_string(), val);
        }
    }
    for (k, v) in declared {
        if scope == McpScope::Project && jade_core::sandbox::is_dangerous_env_var(k) {
            continue;
        }
        out.insert(k.clone(), expand_env_vars(v, scope, SAFE_HOST_VARS));
    }
    out
}

fn expand_env_vars(input: &str, scope: McpScope, safe_vars: &[&str]) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find('}') {
            let var_name = &after[..end];
            let allowed = scope != McpScope::Project || safe_vars.contains(&var_name);
            if allowed {
                if let Ok(val) = std::env::var(var_name) {
                    out.push_str(&val);
                }
            }
            rest = &after[end + 1..];
        } else {
            out.push_str(&rest[start..]);
            return out;
        }
    }
    out.push_str(rest);
    out
}
