//! Connected MCP client pool: tool discovery, resources/prompts, reconnection,
//! OpenAI schema projection, and tool invocation.

use super::config::{
    load_mcp_configs, qualify_tool_name, McpConfigReport, McpScope, McpServerConfig,
    McpTransportConfig,
};
use super::extras::{
    format_mcp_call_result, parse_prompts_page, parse_resources_page, McpPromptInfo,
    McpResourceInfo,
};
use super::transport::{McpTransport, ServerCapabilities};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
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
    pub has_resources: bool,
    pub has_prompts: bool,
    pub error: Option<String>,
}

pub(super) struct ConnectedServer {
    pub(super) cfg: McpServerConfig,
    pub(super) workspace_cwd: PathBuf,
    pub(super) transport: Mutex<McpTransport>,
    pub(super) capabilities: RwLock<ServerCapabilities>,
    pub(super) timeout: Duration,
}

impl ConnectedServer {
    pub(super) async fn reconnect(&self) -> Result<()> {
        let (new_transport, caps) = McpTransport::connect(
            &self.cfg.transport,
            self.cfg.scope,
            &self.workspace_cwd,
            self.cfg.timeout_s,
        )
        .await?;
        *self.transport.lock().await = new_transport;
        *self.capabilities.write().unwrap() = caps;
        Ok(())
    }

    pub(super) fn capabilities(&self) -> ServerCapabilities {
        *self.capabilities.read().unwrap()
    }
}

pub struct McpManager {
    pub(super) servers: BTreeMap<String, ConnectedServer>,
    tools: RwLock<BTreeMap<String, McpToolInfo>>,
    statuses: RwLock<Vec<McpServerStatus>>,
    skipped_untrusted: Vec<PathBuf>,
    managed_error: Option<String>,
}

impl McpManager {
    pub fn empty() -> Self {
        Self {
            servers: BTreeMap::new(),
            tools: RwLock::new(BTreeMap::new()),
            statuses: RwLock::new(Vec::new()),
            skipped_untrusted: Vec::new(),
            managed_error: None,
        }
    }

    pub async fn connect_configured(data_dir: &Path, cwd: &Path) -> Self {
        let report = load_mcp_configs(data_dir, cwd);
        Self::connect_report(report, cwd).await
    }

    pub async fn connect_report(report: McpConfigReport, cwd: &Path) -> Self {
        if let Some(err) = report.managed_error {
            return Self {
                servers: BTreeMap::new(),
                tools: RwLock::new(BTreeMap::new()),
                statuses: RwLock::new(vec![McpServerStatus {
                    name: "managed:load_error".into(),
                    scope: McpScope::Managed,
                    transport_label: "config",
                    connected: false,
                    tool_count: 0,
                    has_resources: false,
                    has_prompts: false,
                    error: Some(format!("managed MCP config failed (fail-closed): {err}")),
                }]),
                skipped_untrusted: report.skipped_untrusted,
                managed_error: Some(err),
            };
        }

        let mut servers = BTreeMap::new();
        let mut tools = BTreeMap::new();
        let mut statuses = Vec::new();

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
                servers.insert(status.name.clone(), conn);
            }
            insert_server_tools(&mut tools, discovered);
            statuses.push(status);
        }
        statuses.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            servers,
            tools: RwLock::new(tools),
            statuses: RwLock::new(statuses),
            skipped_untrusted: report.skipped_untrusted,
            managed_error: None,
        }
    }

    pub fn statuses(&self) -> Vec<McpServerStatus> {
        self.statuses.read().unwrap().clone()
    }

    pub fn managed_error(&self) -> Option<&str> {
        self.managed_error.as_deref()
    }

    pub fn skipped_untrusted(&self) -> &[PathBuf] {
        &self.skipped_untrusted
    }

    pub fn tools(&self) -> Vec<McpToolInfo> {
        self.tools.read().unwrap().values().cloned().collect()
    }

    pub fn tool_count(&self) -> usize {
        self.tools.read().unwrap().len()
    }

    pub fn tool_identity(&self, qualified_name: &str) -> Option<(String, String)> {
        self.tools
            .read()
            .unwrap()
            .get(qualified_name)
            .map(|t| (t.server.clone(), t.raw_name.clone()))
    }

    pub fn is_read_only(&self, qualified_name: &str) -> bool {
        self.tools
            .read()
            .unwrap()
            .get(qualified_name)
            .map(|t| t.read_only_hint)
            .unwrap_or(false)
    }

    pub fn has_resource_servers(&self) -> bool {
        self.servers.values().any(|s| s.capabilities().resources)
    }

    pub fn has_prompt_servers(&self) -> bool {
        self.servers.values().any(|s| s.capabilities().prompts)
    }

    pub fn extra_tool_schemas(&self) -> Vec<Value> {
        self.tools
            .read()
            .unwrap()
            .values()
            .map(tool_to_openai_schema)
            .collect()
    }

    pub fn schema_for(&self, qualified_name: &str) -> Option<Value> {
        self.tools
            .read()
            .unwrap()
            .get(qualified_name)
            .map(tool_to_openai_schema)
    }

    /// Re-query `tools/list` on any server that emitted `notifications/tools/list_changed`.
    pub async fn refresh_dirty_tools(&self) -> usize {
        let mut refreshed = 0usize;
        for (name, srv) in &self.servers {
            let dirty = {
                let transport = srv.transport.lock().await;
                transport.take_tools_dirty()
            };
            if !dirty {
                continue;
            }
            if self.refresh_server_tools(name, srv).await.is_ok() {
                refreshed += 1;
            }
        }
        refreshed
    }

    async fn refresh_server_tools(&self, server_name: &str, srv: &ConnectedServer) -> Result<()> {
        let discovered = {
            let mut transport = srv.transport.lock().await;
            fetch_tools_list(&mut transport, server_name, srv.timeout).await?
        };
        let count = discovered.len();
        {
            let mut map = self.tools.write().unwrap();
            map.retain(|_, info| info.server != server_name);
            insert_server_tools(&mut map, discovered);
        }
        if let Some(st) = self
            .statuses
            .write()
            .unwrap()
            .iter_mut()
            .find(|s| s.name == server_name)
        {
            st.tool_count = count;
        }
        Ok(())
    }

    pub async fn call_tool(&self, qualified_name: &str, args: Value) -> Result<String> {
        let info = self
            .tools
            .read()
            .unwrap()
            .get(qualified_name)
            .cloned()
            .with_context(|| format!("unknown MCP tool `{qualified_name}`"))?;
        let srv = self
            .servers
            .get(&info.server)
            .with_context(|| format!("MCP server `{}` is not connected", info.server))?;

        let call_res = {
            let mut transport = srv.transport.lock().await;
            transport
                .request(
                    "tools/call",
                    json!({
                        "name": info.raw_name,
                        "arguments": if args.is_null() { json!({}) } else { args },
                    }),
                    srv.timeout,
                )
                .await
        };

        let result = match call_res {
            Ok(val) => val,
            Err(err) => {
                if is_broken_transport_error(&err) && srv.reconnect().await.is_ok() {
                    let _ = self.refresh_server_tools(&info.server, srv).await;
                    bail!("MCP server `{}` disconnected during `{}` (reconnected for subsequent calls): {err:#}", info.server, info.raw_name);
                }
                return Err(err);
            }
        };

        let is_error = result["isError"].as_bool().unwrap_or(false);
        let text = format_mcp_call_result(&result);
        if is_error {
            bail!("{text}");
        }
        Ok(text)
    }

    pub async fn list_resources(&self, server_filter: Option<&str>) -> Result<Vec<McpResourceInfo>> {
        let mut out = Vec::new();
        for (name, srv) in &self.servers {
            if let Some(want) = server_filter {
                if name != want {
                    continue;
                }
            }
            if !srv.capabilities().resources {
                continue;
            }
            let res = request_with_read_reconnect(srv, "resources/list", json!({})).await?;
            out.extend(parse_resources_page(name, &res));
        }
        Ok(out)
    }

    pub async fn read_resource(&self, server: &str, uri: &str) -> Result<String> {
        let srv = self
            .servers
            .get(server)
            .with_context(|| format!("MCP server `{server}` is not connected"))?;
        let res =
            request_with_read_reconnect(srv, "resources/read", json!({ "uri": uri })).await?;
        Ok(super::extras::format_mcp_resource_result(&res))
    }

    pub async fn list_prompts(&self, server_filter: Option<&str>) -> Result<Vec<McpPromptInfo>> {
        let mut out = Vec::new();
        for (name, srv) in &self.servers {
            if let Some(want) = server_filter {
                if name != want {
                    continue;
                }
            }
            if !srv.capabilities().prompts {
                continue;
            }
            let res = request_with_read_reconnect(srv, "prompts/list", json!({})).await?;
            out.extend(parse_prompts_page(name, &res));
        }
        Ok(out)
    }

    pub async fn get_prompt(
        &self,
        server: &str,
        name: &str,
        arguments: BTreeMap<String, String>,
    ) -> Result<String> {
        let srv = self
            .servers
            .get(server)
            .with_context(|| format!("MCP server `{server}` is not connected"))?;
        let res = request_with_read_reconnect(
            srv,
            "prompts/get",
            json!({ "name": name, "arguments": arguments }),
        )
        .await?;
        Ok(super::extras::format_mcp_prompt_result(&res))
    }
}

async fn request_with_read_reconnect(
    srv: &ConnectedServer,
    method: &str,
    params: Value,
) -> Result<Value> {
    let first = {
        let mut transport = srv.transport.lock().await;
        transport.request(method, params.clone(), srv.timeout).await
    };
    match first {
        Ok(v) => Ok(v),
        Err(err) if is_broken_transport_error(&err) => {
            srv.reconnect().await?;
            let mut transport = srv.transport.lock().await;
            transport.request(method, params, srv.timeout).await
        }
        Err(err) => Err(err),
    }
}

fn is_broken_transport_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("closed stdout")
        || msg.contains("broken pipe")
        || msg.contains("connection reset")
        || msg.contains("eof")
}

pub(super) fn tool_to_openai_schema(t: &McpToolInfo) -> Value {
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
}

fn insert_server_tools(map: &mut BTreeMap<String, McpToolInfo>, discovered: Vec<McpToolInfo>) {
    for mut t in discovered {
        if map.contains_key(&t.qualified_name) {
            let candidate = disambiguate_tool_name(&t.qualified_name, &t.server, &t.raw_name);
            if map.contains_key(&candidate) {
                tracing::warn!(
                    server = %t.server,
                    tool = %t.raw_name,
                    "skipping colliding MCP tool name"
                );
                continue;
            }
            t.qualified_name = candidate;
        }
        map.insert(t.qualified_name.clone(), t);
    }
}

async fn fetch_tools_list(
    transport: &mut McpTransport,
    server_name: &str,
    timeout: Duration,
) -> Result<Vec<McpToolInfo>> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _page in 0..16 {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let res = transport.request("tools/list", params, timeout).await?;
        if let Some(arr) = res["tools"].as_array() {
            for item in arr {
                let Some(raw_name) = item["name"].as_str() else {
                    continue;
                };
                let qualified_name = qualify_tool_name(server_name, raw_name);
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
                    server: server_name.to_string(),
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
    Ok(tools)
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
                has_resources: false,
                has_prompts: false,
                error: Some("disabled".into()),
            },
            None,
            Vec::new(),
        );
    }

    let timeout = Duration::from_secs(cfg.timeout_s);
    let (mut transport, caps) =
        match McpTransport::connect(&cfg.transport, cfg.scope, &workspace_cwd, cfg.timeout_s).await
        {
            Ok(pair) => pair,
            Err(e) => {
                return (
                    McpServerStatus {
                        name: cfg.name,
                        scope: cfg.scope,
                        transport_label,
                        connected: false,
                        tool_count: 0,
                        has_resources: false,
                        has_prompts: false,
                        error: Some(format!("{e:#}")),
                    },
                    None,
                    Vec::new(),
                );
            }
        };

    let tools = if caps.tools {
        match fetch_tools_list(&mut transport, &cfg.name, timeout).await {
            Ok(t) => t,
            Err(e) => {
                return (
                    McpServerStatus {
                        name: cfg.name,
                        scope: cfg.scope,
                        transport_label,
                        connected: false,
                        tool_count: 0,
                        has_resources: caps.resources,
                        has_prompts: caps.prompts,
                        error: Some(format!("tools/list failed: {e:#}")),
                    },
                    None,
                    Vec::new(),
                );
            }
        }
    } else {
        Vec::new()
    };

    let status = McpServerStatus {
        name: cfg.name.clone(),
        scope: cfg.scope,
        transport_label,
        connected: true,
        tool_count: tools.len(),
        has_resources: caps.resources,
        has_prompts: caps.prompts,
        error: None,
    };
    let conn = ConnectedServer {
        cfg,
        workspace_cwd,
        transport: Mutex::new(transport),
        capabilities: RwLock::new(caps),
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
    let prefix = if base.len() > keep {
        &base[..keep]
    } else {
        base
    };
    format!("{prefix}{suffix}")
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
            "jade-mcp-client-{tag}-{:02x}{:02x}{:02x}{:02x}",
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
                        "result": { "protocolVersion": "2025-03-26", "capabilities": { "tools": {}, "resources": {}, "prompts": {} } }
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
                } else if method == "resources/list" {
                    let b = serde_json::to_string(&json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "resources": [{ "uri": "demo://readme", "name": "readme", "description": "Demo doc" }] }
                    }))
                    .unwrap();
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{b}", b.len())
                } else if method == "resources/read" {
                    let b = serde_json::to_string(&json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "contents": [{ "uri": "demo://readme", "text": "hello resource" }] }
                    }))
                    .unwrap();
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{b}", b.len())
                } else if method == "prompts/list" {
                    let b = serde_json::to_string(&json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "prompts": [{ "name": "summarize", "description": "Summarize code" }] }
                    }))
                    .unwrap();
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{b}", b.len())
                } else if method == "prompts/get" {
                    let b = serde_json::to_string(&json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "messages": [{ "role": "user", "content": { "type": "text", "text": "Please summarize." } }] }
                    }))
                    .unwrap();
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{b}", b.len())
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

        let mgr_untrusted = McpManager::connect_configured(&data, &cwd).await;
        assert!(mgr_untrusted.tools().is_empty());
        assert_eq!(mgr_untrusted.skipped_untrusted().len(), 1);

        set_workspace_trusted(&data, &cwd, true).unwrap();
        let mgr = McpManager::connect_configured(&data, &cwd).await;
        assert_eq!(mgr.statuses().len(), 2);
        assert_eq!(mgr.tools().len(), 2);
        assert!(mgr.is_read_only("mcp__httpmock__ping"));
        assert!(mgr.has_resource_servers());
        assert!(mgr.has_prompt_servers());

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

        let resources = mgr.list_resources(None).await.unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].uri, "demo://readme");
        let body = mgr.read_resource("httpmock", "demo://readme").await.unwrap();
        assert!(body.contains("hello resource"));

        let prompts = mgr.list_prompts(None).await.unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "summarize");
        let ptext = mgr
            .get_prompt("httpmock", "summarize", BTreeMap::new())
            .await
            .unwrap();
        assert!(ptext.contains("Please summarize."));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn stdio_tools_list_changed_notification_refreshes_tools() {
        let root = tmpdir("list-changed");
        let data = root.join("data");
        let cwd = root.join("repo");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        // First tools/list returns ["t1"]; calling t1 emits notifications/tools/list_changed before responding;
        // second tools/list returns ["t1", "t2"].
        let script = r#"
count=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{"listChanged":true}}}}\n' "$id"
      ;;
    *'"tools/list"'*)
      count=$((count+1))
      if [ "$count" -eq 1 ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"t1","description":"first","inputSchema":{"type":"object"}}]}}\n' "$id"
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"t1","description":"first","inputSchema":{"type":"object"}},{"name":"t2","description":"second","inputSchema":{"type":"object"}}]}}\n' "$id"
      fi
      ;;
    *'"tools/call"'*)
      printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"ok"}],"isError":false}}\n' "$id"
      ;;
  esac
done
"#;
        std::fs::write(
            data.join("mcp.json"),
            serde_json::to_string_pretty(&json!({
                "mcpServers": {
                    "dyn": { "command": "sh", "args": ["-c", script] }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let mgr = McpManager::connect_configured(&data, &cwd).await;
        assert_eq!(mgr.tools().len(), 1);
        let _ = mgr.call_tool("mcp__dyn__t1", json!({})).await.unwrap();
        let refreshed = mgr.refresh_dirty_tools().await;
        assert_eq!(refreshed, 1);
        assert_eq!(mgr.tools().len(), 2);
        assert!(mgr.tool_identity("mcp__dyn__t2").is_some());

        let _ = std::fs::remove_dir_all(&root);
    }
}
