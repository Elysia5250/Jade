//! MCP client server configuration discovery with workspace-trust gating.

use crate::permissions::is_workspace_trusted;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpScope {
    User,
    Project,
}

impl McpScope {
    pub fn label(self) -> &'static str {
        match self {
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

pub fn load_mcp_configs(data_dir: &Path, cwd: &Path) -> McpConfigReport {
    let mut report = McpConfigReport::default();
    let mut by_name: BTreeMap<String, McpServerConfig> = BTreeMap::new();

    load_file(
        &data_dir.join("mcp.json"),
        McpScope::User,
        cwd,
        &mut by_name,
    );

    let project_candidates = [cwd.join(".mcp.json"), cwd.join(".envoy").join("mcp.json")];
    if is_workspace_trusted(data_dir, cwd) {
        for path in project_candidates {
            load_file(&path, McpScope::Project, cwd, &mut by_name);
        }
    } else {
        for path in project_candidates {
            if path.is_file() {
                report.skipped_untrusted.push(path);
            }
        }
    }

    report.servers = by_name.into_values().collect();
    report
}

fn load_file(
    path: &Path,
    scope: McpScope,
    workspace_cwd: &Path,
    out: &mut BTreeMap<String, McpServerConfig>,
) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(parsed) = serde_json::from_str::<RawMcpFile>(&text) else {
        return;
    };
    for (name, raw) in parsed.mcp_servers {
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

pub fn safe_stdio_env(declared: &BTreeMap<String, String>) -> BTreeMap<String, String> {
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
        out.insert(k.clone(), expand_env_vars(v));
    }
    out
}

fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find('}') {
            let var_name = &after[..end];
            if let Ok(val) = std::env::var(var_name) {
                out.push_str(&val);
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
