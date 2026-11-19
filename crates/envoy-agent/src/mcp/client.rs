//! Connected MCP client pool: tool discovery, OpenAI schema projection, and tool invocation.

use super::config::{
    load_mcp_configs, qualify_tool_name, McpScope, McpServerConfig, McpTransportConfig,
};
use super::transport::McpTransport;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub qualified_name: String,
    pub server: String,
    pub raw_name: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only_hint: bool,
}

#[derive(Debug, Clone)]
pub struct McpServerStatus {
    pub name: String,
    pub scope: McpScope,
    pub transport_label: &'static str,
    pub connected: bool,
    pub tool_count: usize,
    pub error: Option<String>,
}

struct ConnectedServer {
    transport: Mutex<McpTransport>,
    timeout: Duration,
}

pub struct McpManager {
    servers: BTreeMap<String, ConnectedServer>,
    tools: BTreeMap<String, McpToolInfo>,
    statuses: Vec<McpServerStatus>,
    skipped_untrusted: Vec<PathBuf>,
}

impl McpManager {
    pub fn empty() -> Self {
        Self {
            servers: BTreeMap::new(),
            tools: BTreeMap::new(),
            statuses: Vec::new(),
            skipped_untrusted: Vec::new(),
        }
    }

    pub async fn connect_configured(data_dir: &Path, cwd: &Path) -> Self {
        let report = load_mcp_configs(data_dir, cwd);
        let mut mgr = Self {
            servers: BTreeMap::new(),
            tools: BTreeMap::new(),
            statuses: Vec::new(),
            skipped_untrusted: report.skipped_untrusted,
        };

        let mut handles = Vec::new();
        for cfg in report.servers {
            let cwd_c = cwd.to_path_buf();
            handles.push(tokio::spawn(connect_one(cfg, cwd_c)));
        }

        for h in handles {
            let Ok((status, conn_opt, discovered)) = h.await else {
                continue;
            };
            if let Some(conn) = conn_opt {
                mgr.servers.insert(status.name.clone(), conn);
            }
            for mut t in discovered {
                if mgr.tools.contains_key(&t.qualified_name) {
                    let candidate = disambiguate_tool_name(&t.qualified_name, &t.server, &t.raw_name);
                    if mgr.tools.contains_key(&candidate) {
                        tracing::warn!(
                            server = %t.server,
                            tool = %t.raw_name,
                            "skipping colliding MCP tool name"
                        );
                        continue;
                    }
                    t.qualified_name = candidate;
                }
                mgr.tools.insert(t.qualified_name.clone(), t);
            }
            mgr.statuses.push(status);
        }
        mgr.statuses.sort_by(|a, b| a.name.cmp(&b.name));
        mgr
    }

    pub fn statuses(&self) -> &[McpServerStatus] {
        &self.statuses
    }

    pub fn skipped_untrusted(&self) -> &[PathBuf] {
        &self.skipped_untrusted
    }

    pub fn tools(&self) -> Vec<&McpToolInfo> {
        self.tools.values().collect()
    }

    pub fn tool_identity(&self, qualified_name: &str) -> Option<(&str, &str)> {
        self.tools
            .get(qualified_name)
            .map(|t| (t.server.as_str(), t.raw_name.as_str()))
    }

    pub fn is_read_only(&self, qualified_name: &str) -> bool {
        self.tools
            .get(qualified_name)
            .map(|t| t.read_only_hint)
            .unwrap_or(false)
    }

    pub fn extra_tool_schemas(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| {
                let mut schema = t.input_schema.clone();
                if !schema.is_object() {
                    schema = json!({ "type": "object", "properties": {} });
                }
                json!({
                    "type": "function",
                    "function": {
                        "name": t.qualified_name,
                        "description": format!("[MCP {}:{}] {}", t.server, t.raw_name, t.description),
                        "parameters": schema,
                    }
                })
            })
            .collect()
    }

    pub async fn call_tool(&self, qualified_name: &str, args: Value) -> Result<String> {
        let info = self
            .tools
            .get(qualified_name)
            .with_context(|| format!("unknown MCP tool `{qualified_name}`"))?;
        let srv = self
            .servers
            .get(&info.server)
            .with_context(|| format!("MCP server `{}` is not connected", info.server))?;

        let mut transport = srv.transport.lock().await;
        let result = transport
            .request(
                "tools/call",
                json!({
                    "name": info.raw_name,
                    "arguments": if args.is_null() { json!({}) } else { args },
                }),
                srv.timeout,
            )
            .await?;

        let is_error = result["isError"].as_bool().unwrap_or(false);
        let text = format_mcp_call_result(&result);
        if is_error {
            bail!("{text}");
        }
        Ok(text)
    }
}

async fn connect_one(
    cfg: McpServerConfig,
    workspace_cwd: PathBuf,
) -> (McpServerStatus, Option<ConnectedServer>, Vec<McpToolInfo>) {
    let transport_label = match cfg.transport {
        McpTransportConfig::Stdio { .. } => "stdio",
        McpTransportConfig::Http { .. } => "http",
    };
    if cfg.disabled {
        return (
            McpServerStatus {
                name: cfg.name,
                scope: cfg.scope,
                transport_label,
                connected: false,
                tool_count: 0,
                error: Some("disabled".into()),
            },
            None,
            Vec::new(),
        );
    }

    let timeout = Duration::from_secs(cfg.timeout_s);
    let mut transport =
        match McpTransport::connect(&cfg.transport, cfg.scope, &workspace_cwd, cfg.timeout_s).await
        {
        Ok(t) => t,
        Err(e) => {
            return (
                McpServerStatus {
                    name: cfg.name,
                    scope: cfg.scope,
                    transport_label,
                    connected: false,
                    tool_count: 0,
                    error: Some(format!("{e:#}")),
                },
                None,
                Vec::new(),
            );
        }
    };

    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _page in 0..16 {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let res = match transport.request("tools/list", params, timeout).await {
            Ok(r) => r,
            Err(e) => {
                return (
                    McpServerStatus {
                        name: cfg.name,
                        scope: cfg.scope,
                        transport_label,
                        connected: false,
                        tool_count: 0,
                        error: Some(format!("tools/list failed: {e:#}")),
                    },
                    None,
                    Vec::new(),
                );
            }
        };
        if let Some(arr) = res["tools"].as_array() {
            for item in arr {
                let Some(raw_name) = item["name"].as_str() else {
                    continue;
                };
                let qualified_name = qualify_tool_name(&cfg.name, raw_name);
                let description = item["description"].as_str().unwrap_or("").to_string();
                let input_schema = item
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                let read_only_hint = item["annotations"]["readOnlyHint"]
                    .as_bool()
                    .unwrap_or(false);
                tools.push(McpToolInfo {
                    qualified_name,
                    server: cfg.name.clone(),
                    raw_name: raw_name.to_string(),
                    description,
                    input_schema,
                    read_only_hint,
                });
            }
        }
        cursor = res["nextCursor"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }

    let status = McpServerStatus {
        name: cfg.name,
        scope: cfg.scope,
        transport_label,
        connected: true,
        tool_count: tools.len(),
        error: None,
    };
    let conn = ConnectedServer {
        transport: Mutex::new(transport),
        timeout,
    };
    (status, Some(conn), tools)
}

fn disambiguate_tool_name(base: &str, server: &str, raw_name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(server.as_bytes());
    hasher.update([0u8]);
    hasher.update(raw_name.as_bytes());
    let d = hasher.finalize();
    let suffix = format!("_{:02x}{:02x}{:02x}{:02x}", d[0], d[1], d[2], d[3]);
    let keep = 64usize.saturating_sub(suffix.len());
    let prefix = if base.len() > keep { &base[..keep] } else { base };
    format!("{prefix}{suffix}")
}

fn format_mcp_call_result(result: &Value) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::set_workspace_trusted;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 6];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "envoy-mcp-client-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn project_mcp_json_is_blocked_until_trusted_then_connects_stdio_and_http_sse() {
        let root = tmpdir("trust-and-rpc");
        let data = root.join("data");
        let cwd = root.join("repo");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        // Loopback Streamable HTTP server: initialize -> JSON + Mcp-Session-Id, tools/list -> SSE, tools/call -> SSE
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let raw = String::from_utf8_lossy(&buf[..n]);
                let body = raw.split("\r\n\r\n").nth(1).unwrap_or("{}");
                let req: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                let method = req["method"].as_str().unwrap_or("");
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                let resp = if method == "initialize" {
                    let b = serde_json::to_string(&json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "protocolVersion": "2025-03-26", "capabilities": { "tools": {} } }
                    }))
                    .unwrap();
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: sess-42\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{b}", b.len())
                } else if method == "notifications/initialized" {
                    "HTTP/1.1 202 Accepted\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                        .to_string()
                } else if method == "tools/list" {
                    assert!(
                        raw.contains("sess-42"),
                        "missing Mcp-Session-Id header:\n{raw}"
                    );
                    let evt = serde_json::to_string(&json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "ping",
                                "description": "Ping HTTP tool",
                                "annotations": { "readOnlyHint": true },
                                "inputSchema": { "type": "object", "properties": { "msg": { "type": "string" } } }
                            }]
                        }
                    }))
                    .unwrap();
                    let sse = format!("event: message\ndata: {evt}\n\n");
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}", sse.len())
                } else {
                    let msg = req["params"]["arguments"]["msg"].as_str().unwrap_or("");
                    let evt = serde_json::to_string(&json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": format!("pong:{msg}") }],
                            "isError": false
                        }
                    }))
                    .unwrap();
                    let sse = format!("data: {evt}\n\n");
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}", sse.len())
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        // Write a tiny sh stdio MCP server in repo/.mcp.json
        let script = r#"while IFS= read -r line; do case "$line" in *'"initialize"'*) printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}}}}\n';; *'"tools/list"'*) printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"Echo stdio","inputSchema":{"type":"object"}}]}}\n';; *'"tools/call"'*) printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"stdio-ok"}],"isError":false}}\n';; esac; done"#;
        std::fs::write(
            cwd.join(".mcp.json"),
            serde_json::to_string_pretty(&json!({
                "mcpServers": {
                    "localmock": { "command": "sh", "args": ["-c", script] },
                    "httpmock": { "type": "http", "url": format!("http://{addr}/mcp") }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        // Untrusted: project .mcp.json must be skipped!
        let mgr_untrusted = McpManager::connect_configured(&data, &cwd).await;
        assert!(mgr_untrusted.tools().is_empty());
        assert_eq!(mgr_untrusted.skipped_untrusted().len(), 1);

        // Trust workspace -> both stdio and HTTP SSE servers connect!
        set_workspace_trusted(&data, &cwd, true).unwrap();
        let mgr = McpManager::connect_configured(&data, &cwd).await;
        assert_eq!(mgr.statuses().len(), 2);
        assert_eq!(mgr.tools().len(), 2);
        assert!(mgr.is_read_only("mcp__httpmock__ping"));

        let out_http = mgr
            .call_tool("mcp__httpmock__ping", json!({ "msg": "hello" }))
            .await
            .unwrap();
        assert_eq!(out_http, "pong:hello");

        let out_stdio = mgr
            .call_tool("mcp__localmock__echo", json!({}))
            .await
            .unwrap();
        assert_eq!(out_stdio, "stdio-ok");

        let _ = std::fs::remove_dir_all(&root);
    }
}
