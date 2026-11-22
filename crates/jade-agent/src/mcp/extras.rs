//! MCP resources, prompts, formatting helpers, and deferred MCP tool search.

use super::client::McpManager;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

/// Above this count, MCP tool schemas switch to on-demand activation via `mcp_search`
/// so large server ecosystems do not crowd out the conversation context window.
pub const DEFERRED_MCP_TOOL_THRESHOLD: usize = 24;

#[derive(Debug, Clone)]
pub struct McpResourceInfo {
    pub server: String,
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct McpPromptArg {
    pub name: String,
    pub description: String,
    pub required: bool,
}

#[derive(Debug, Clone)]
pub struct McpPromptInfo {
    pub server: String,
    pub name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArg>,
}

/// Per-turn view over a shared [`McpManager`] that tracks deferred tool activations.
pub struct McpTurnView {
    manager: Arc<McpManager>,
    threshold: usize,
    activated: RwLock<BTreeSet<String>>,
}

impl McpTurnView {
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self::with_threshold(manager, DEFERRED_MCP_TOOL_THRESHOLD)
    }

    pub fn with_threshold(manager: Arc<McpManager>, threshold: usize) -> Self {
        Self {
            manager,
            threshold,
            activated: RwLock::new(BTreeSet::new()),
        }
    }

    pub fn manager(&self) -> &Arc<McpManager> {
        &self.manager
    }

    pub fn is_deferred(&self) -> bool {
        self.manager.tool_count() > self.threshold
    }

    pub fn extra_tool_schemas(&self) -> Vec<Value> {
        let mut schemas = if !self.is_deferred() {
            self.manager.extra_tool_schemas()
        } else {
            let mut out = vec![mcp_search_schema(self.manager.tool_count())];
            let active = self.activated.read().unwrap();
            for name in active.iter() {
                if let Some(s) = self.manager.schema_for(name) {
                    out.push(s);
                }
            }
            out
        };
        if self.manager.has_resource_servers() {
            schemas.push(mcp_resource_schema());
        }
        schemas
    }

    pub fn search_and_activate(&self, args: &Value) -> Result<String> {
        let query = args["query"].as_str().unwrap_or("").trim().to_ascii_lowercase();
        let limit = args["limit"].as_u64().unwrap_or(8).clamp(1, 16) as usize;
        let tokens: Vec<&str> = query.split_whitespace().collect();
        let all = self.manager.tools();
        let mut matches: Vec<_> = all
            .into_iter()
            .filter(|t| {
                if tokens.is_empty() {
                    return true;
                }
                let hay = format!(
                    "{} {} {} {}",
                    t.qualified_name, t.server, t.raw_name, t.description
                )
                .to_ascii_lowercase();
                tokens.iter().all(|tok| hay.contains(tok))
            })
            .collect();
        matches.truncate(limit);
        if matches.is_empty() {
            return Ok(format!(
                "No MCP tools matched `{query}` (total available: {}).",
                self.manager.tool_count()
            ));
        }
        let mut active = self.activated.write().unwrap();
        let mut lines = vec![format!(
            "Activated {} MCP tool(s) for this turn (call them directly on your next step):",
            matches.len()
        )];
        for t in matches {
            active.insert(t.qualified_name.clone());
            lines.push(format!(
                "- `{}` [{}:{}]: {}",
                t.qualified_name, t.server, t.raw_name, t.description
            ));
        }
        Ok(lines.join("\n"))
    }

    pub async fn call_resource_tool(&self, args: &Value) -> Result<String> {
        let action = args["action"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("missing `action` (`list` or `read`)")?;
        match action {
            "list" => {
                let server = args["server"].as_str().filter(|s| !s.trim().is_empty());
                let items = self.manager.list_resources(server).await?;
                if items.is_empty() {
                    return Ok("No MCP resources available.".into());
                }
                let lines: Vec<String> = items
                    .into_iter()
                    .map(|r| {
                        if r.description.is_empty() {
                            format!("- [{}] {} ({})", r.server, r.uri, r.name)
                        } else {
                            format!("- [{}] {} ({}) — {}", r.server, r.uri, r.name, r.description)
                        }
                    })
                    .collect();
                Ok(lines.join("\n"))
            }
            "read" => {
                let server = args["server"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("missing `server` for `mcp_resource` read")?;
                let uri = args["uri"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("missing `uri` for `mcp_resource` read")?;
                self.manager.read_resource(server, uri).await
            }
            other => bail!("unsupported `mcp_resource` action `{other}` (expected `list` or `read`)"),
        }
    }
}

pub fn mcp_search_schema(total_tools: usize) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "mcp_search",
            "description": format!("Search and activate from {total_tools} connected MCP tools by keyword. Activated tools become callable on the next step."),
            "parameters": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Keywords matching server, tool name, or description (empty lists first page)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 16, "default": 8 }
                },
                "required": ["query"]
            }
        }
    })
}

pub fn mcp_resource_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "mcp_resource",
            "description": "List or read resources exposed by connected MCP servers.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "read"] },
                    "server": { "type": "string", "description": "MCP server name (optional for list, required for read)." },
                    "uri": { "type": "string", "description": "Resource URI (required for read)." }
                },
                "required": ["action"]
            }
        }
    })
}

pub(super) fn parse_resources_page(server: &str, res: &Value) -> Vec<McpResourceInfo> {
    let Some(arr) = res["resources"].as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let uri = item["uri"].as_str()?.to_string();
            let name = item["name"].as_str().unwrap_or(&uri).to_string();
            let description = item["description"].as_str().unwrap_or("").to_string();
            let mime_type = item["mimeType"].as_str().map(str::to_string);
            Some(McpResourceInfo {
                server: server.to_string(),
                uri,
                name,
                description,
                mime_type,
            })
        })
        .collect()
}

pub(super) fn parse_prompts_page(server: &str, res: &Value) -> Vec<McpPromptInfo> {
    let Some(arr) = res["prompts"].as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let name = item["name"].as_str()?.to_string();
            let description = item["description"].as_str().unwrap_or("").to_string();
            let arguments = item["arguments"]
                .as_array()
                .map(|args| {
                    args.iter()
                        .filter_map(|a| {
                            let aname = a["name"].as_str()?.to_string();
                            Some(McpPromptArg {
                                name: aname,
                                description: a["description"].as_str().unwrap_or("").to_string(),
                                required: a["required"].as_bool().unwrap_or(false),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(McpPromptInfo {
                server: server.to_string(),
                name,
                description,
                arguments,
            })
        })
        .collect()
}

pub(super) fn format_mcp_call_result(result: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(arr) = result["content"].as_array() {
        for item in arr {
            match item["type"].as_str() {
                Some("text") => {
                    if let Some(t) = item["text"].as_str() {
                        parts.push(t.to_string());
                    }
                }
                Some("image") => {
                    let mime = item["mimeType"].as_str().unwrap_or("image/*");
                    parts.push(format!("[image: {mime}]"));
                }
                Some("resource") => {
                    let uri = item["resource"]["uri"].as_str().unwrap_or("");
                    if let Some(text) = item["resource"]["text"].as_str() {
                        parts.push(format!("[resource {uri}]\n{text}"));
                    } else {
                        parts.push(format!("[resource {uri}]"));
                    }
                }
                Some(other) => parts.push(format!("[{other}] {}", item)),
                None => {}
            }
        }
    }
    if parts.is_empty() {
        if let Some(structured) = result.get("structuredContent") {
            return serde_json::to_string_pretty(structured)
                .unwrap_or_else(|_| structured.to_string());
        }
        return "(empty MCP tool output)".into();
    }
    parts.join("\n")
}

pub(super) fn format_mcp_resource_result(result: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(arr) = result["contents"].as_array() {
        for item in arr {
            let uri = item["uri"].as_str().unwrap_or("");
            if let Some(text) = item["text"].as_str() {
                parts.push(format!("[resource {uri}]\n{text}"));
            } else if let Some(mime) = item["mimeType"].as_str() {
                parts.push(format!("[resource {uri} ({mime}, binary)]"));
            } else {
                parts.push(format!("[resource {uri}]"));
            }
        }
    }
    if parts.is_empty() {
        "(empty MCP resource)".into()
    } else {
        parts.join("\n\n")
    }
}

pub(super) fn format_mcp_prompt_result(result: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(desc) = result["description"].as_str().filter(|s| !s.is_empty()) {
        parts.push(desc.to_string());
    }
    if let Some(arr) = result["messages"].as_array() {
        for msg in arr {
            let role = msg["role"].as_str().unwrap_or("user");
            if let Some(text) = msg["content"]["text"].as_str() {
                parts.push(format!("[{role}]\n{text}"));
            } else if let Some(text) = msg["content"].as_str() {
                parts.push(format!("[{role}]\n{text}"));
            }
        }
    }
    if parts.is_empty() {
        "(empty MCP prompt)".into()
    } else {
        parts.join("\n\n")
    }
}
