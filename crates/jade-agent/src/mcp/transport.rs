//! MCP client transports: stdio (JSON-RPC lines) and Streamable HTTP (`POST` JSON + SSE).

use super::config::{safe_stdio_env, McpScope, McpTransportConfig};
use crate::agent::tools::web::{validate_http_url, SafeDnsResolver};
use crate::permissions::paths::is_path_under_workspace;
use anyhow::{bail, Context, Result};
use jade_core::ScratchGuard;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const MAX_MCP_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_MCP_HTTP_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    pub tools: bool,
    pub resources: bool,
    pub prompts: bool,
}

#[allow(clippy::large_enum_variant)]
pub enum McpTransport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

pub struct StdioTransport {
    pid: Option<u32>,
    _child: Child,
    _scratch: ScratchGuard,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: AtomicU64,
    tools_dirty: AtomicBool,
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            jade_core::exec::kill_group(pid);
        }
    }
}

pub struct HttpTransport {
    http: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session_id: Option<String>,
    next_id: AtomicU64,
}

impl McpTransport {
    pub async fn connect(
        cfg: &McpTransportConfig,
        scope: McpScope,
        workspace_cwd: &Path,
        timeout_s: u64,
    ) -> Result<(Self, ServerCapabilities)> {
        let mut conn = match cfg {
            McpTransportConfig::Stdio {
                command,
                args,
                env,
                cwd,
            } => Self::Stdio(StdioTransport::spawn(
                command,
                args,
                env,
                cwd.as_ref(),
                scope,
                workspace_cwd,
            )?),
            McpTransportConfig::Http { url, headers } => Self::Http(HttpTransport::new(
                url.clone(),
                headers.clone(),
                scope,
                timeout_s,
            )?),
        };

        let timeout = Duration::from_secs(timeout_s);
        let init_params = json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "jade",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        let init_res = conn.request("initialize", init_params, timeout).await?;
        let caps = parse_capabilities(&init_res);
        let _ = conn
            .notify("notifications/initialized", json!({}), timeout)
            .await;
        Ok((conn, caps))
    }

    pub fn take_tools_dirty(&self) -> bool {
        match self {
            Self::Stdio(s) => s.tools_dirty.swap(false, Ordering::AcqRel),
            Self::Http(_) => false,
        }
    }

    pub fn pid(&self) -> Option<u32> {
        match self {
            Self::Stdio(s) => s.pid,
            Self::Http(_) => None,
        }
    }

    pub fn transport_label(&self) -> &'static str {
        match self {
            Self::Stdio(_) => "stdio",
            Self::Http(_) => "http",
        }
    }

    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        match self {
            Self::Stdio(s) => s.request(method, params, timeout).await,
            Self::Http(h) => h.request(method, params, timeout).await,
        }
    }

    pub async fn notify(&mut self, method: &str, params: Value, timeout: Duration) -> Result<()> {
        match self {
            Self::Stdio(s) => s.notify(method, params).await,
            Self::Http(h) => h.notify(method, params, timeout).await,
        }
    }
}

fn parse_capabilities(init_result: &Value) -> ServerCapabilities {
    let Some(caps) = init_result.get("capabilities").and_then(Value::as_object) else {
        return ServerCapabilities {
            tools: true,
            resources: false,
            prompts: false,
        };
    };
    ServerCapabilities {
        tools: caps.contains_key("tools") || caps.is_empty(),
        resources: caps.contains_key("resources"),
        prompts: caps.contains_key("prompts"),
    }
}

impl StdioTransport {
    fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: Option<&PathBuf>,
        scope: McpScope,
        workspace_cwd: &Path,
    ) -> Result<Self> {
        let env_map = safe_stdio_env(env, scope);
        let mut scratch = ScratchGuard(None);
        let mut cmd = if scope == McpScope::Project {
            let eff_cwd = match cwd {
                Some(dir) => {
                    if !is_path_under_workspace(workspace_cwd, dir) {
                        bail!(
                            "project MCP server cwd `{}` escapes workspace",
                            dir.display()
                        );
                    }
                    dir.as_path()
                }
                None => workspace_cwd,
            };
            let mut raw_argv = Vec::with_capacity(1 + args.len());
            raw_argv.push(command.to_string());
            raw_argv.extend(args.iter().cloned());
            let env_vec: Vec<(String, String)> = env_map
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let prepared = jade_core::prepare_command_with_env(
                &raw_argv,
                &eff_cwd.display().to_string(),
                &jade_proto::SandboxPolicy::WorkspaceWrite {
                    cwd: workspace_cwd.display().to_string(),
                    extra_rw: Vec::new(),
                    allow_net: false,
                },
                &env_vec,
            )
            .with_context(|| format!("sandbox setup for project MCP server `{command}`"))?;
            scratch = ScratchGuard(prepared.scratch_dir.clone());
            let mut c = Command::new(&prepared.argv[0]);
            c.args(&prepared.argv[1..]).env_clear();
            if prepared.apply_env_in_child {
                c.envs(&env_map);
                if prepared.landlock.is_some() {
                    if let Some(ref dir) = prepared.scratch_dir {
                        let s = dir.display().to_string();
                        c.env("TMPDIR", &s).env("TMP", &s).env("TEMP", &s);
                    }
                }
            } else {
                for key in [
                    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TERM", "TMPDIR",
                ] {
                    if let Some(val) = env_map.get(key) {
                        c.env(key, val);
                    }
                }
            }
            if let Some(override_dir) = prepared.cwd_override {
                c.current_dir(override_dir);
            } else if eff_cwd.exists() {
                c.current_dir(eff_cwd);
            }
            #[cfg(unix)]
            if let Some(plan) = prepared.landlock {
                unsafe {
                    c.pre_exec(move || jade_core::sandbox::apply_landlock_in_child(&plan));
                }
            }
            c
        } else {
            let mut c = Command::new(command);
            c.args(args).env_clear().envs(&env_map);
            if let Some(dir) = cwd {
                c.current_dir(dir);
            }
            c
        };
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server `{command}`"))?;
        let pid = child.id();
        let stdin = child.stdin.take().context("MCP child stdin missing")?;
        let stdout = BufReader::new(child.stdout.take().context("MCP child stdout missing")?);
        Ok(Self {
            pid,
            _child: child,
            _scratch: scratch,
            stdin,
            stdout,
            next_id: AtomicU64::new(1),
            tools_dirty: AtomicBool::new(false),
        })
    }

    async fn write_json(&mut self, msg: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(msg)?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_json(&payload).await
    }

    async fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        tokio::time::timeout(timeout, async {
            self.write_json(&payload).await?;
            loop {
                let Some(line) = read_mcp_line(&mut self.stdout, MAX_MCP_LINE_BYTES).await? else {
                    bail!("MCP stdio server closed stdout");
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                if msg.get("method").and_then(Value::as_str)
                    == Some("notifications/tools/list_changed")
                {
                    self.tools_dirty.store(true, Ordering::Release);
                }
                if msg.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                return extract_rpc_result(&msg);
            }
        })
        .await
        .context("MCP stdio request timed out")?
    }
}

async fn read_mcp_line(reader: &mut BufReader<ChildStdout>, cap: usize) -> Result<Option<String>> {
    let mut buf = Vec::new();
    let n = reader
        .take((cap + 1) as u64)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > cap {
        bail!("MCP stdio response line exceeded {cap} byte limit");
    }
    Ok(Some(String::from_utf8_lossy(&buf).to_string()))
}

impl HttpTransport {
    fn new(
        url: String,
        headers: BTreeMap<String, String>,
        scope: McpScope,
        timeout_s: u64,
    ) -> Result<Self> {
        let parsed = reqwest::Url::parse(&url).context("invalid MCP HTTP URL")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("unsupported MCP HTTP URL scheme `{}`", parsed.scheme());
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            bail!("MCP HTTP URLs with embedded credentials are not allowed");
        }
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_s))
            .redirect(reqwest::redirect::Policy::none());
        if scope == McpScope::Project {
            validate_http_url(&parsed, true)?;
            builder = builder
                .no_proxy()
                .dns_resolver(Arc::new(SafeDnsResolver::new(true)));
        }
        let http = builder.build()?;
        Ok(Self {
            http,
            url,
            headers,
            session_id: None,
            next_id: AtomicU64::new(1),
        })
    }

    async fn notify(&mut self, method: &str, params: Value, timeout: Duration) -> Result<()> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut req = self
            .http
            .post(&self.url)
            .timeout(timeout)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&payload);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        let _ = req.send().await?;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut req = self
            .http
            .post(&self.url)
            .timeout(timeout)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&payload);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        let resp = req.send().await?;
        if let Some(sid) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
        {
            self.session_id = Some(sid.to_string());
        }
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let body = read_capped_body(resp, MAX_MCP_HTTP_BYTES).await?;
        if !status.is_success() {
            bail!("MCP HTTP {status}: {body}");
        }
        if content_type.contains("text/event-stream") {
            let msg = parse_sse_rpc_response(&body, id)?;
            extract_rpc_result(&msg)
        } else {
            let msg: Value = serde_json::from_str(&body)
                .with_context(|| format!("invalid JSON from MCP HTTP server: {body}"))?;
            extract_rpc_result(&msg)
        }
    }
}

async fn read_capped_body(mut resp: reqwest::Response, cap: usize) -> Result<String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if bytes.len() + chunk.len() > cap {
            bail!("MCP HTTP response exceeded {cap} byte limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn extract_rpc_result(msg: &Value) -> Result<Value> {
    if let Some(err) = msg.get("error") {
        let code = err["code"].as_i64().unwrap_or(-32000);
        let text = err["message"].as_str().unwrap_or("unknown MCP error");
        bail!("MCP error ({code}): {text}");
    }
    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
}

pub(crate) fn parse_sse_rpc_response(body: &str, expected_id: u64) -> Result<Value> {
    let mut data_buf = Vec::new();
    let mut fallback = None;

    for raw_line in body.lines().chain(std::iter::once("")) {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            if !data_buf.is_empty() {
                let joined = data_buf.join("\n");
                data_buf.clear();
                if let Ok(val) = serde_json::from_str::<Value>(&joined) {
                    if val.get("id").and_then(Value::as_u64) == Some(expected_id) {
                        return Ok(val);
                    }
                    if val.get("result").is_some() || val.get("error").is_some() {
                        fallback = Some(val);
                    }
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_buf.push(rest.trim_start().to_string());
        }
    }

    fallback.context("MCP SSE stream did not contain a JSON-RPC response")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn pid_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[tokio::test]
    async fn stdio_drop_kills_grandchild_process_group() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jade-mcp-pgid-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > {}; wait", pid_file.display());

        let transport = StdioTransport::spawn(
            "sh",
            &["-c".to_string(), script],
            &BTreeMap::new(),
            None,
            McpScope::User,
            &dir,
        )
        .expect("spawn stdio MCP mock");

        let mut gpid = None;
        for _ in 0..40 {
            if let Ok(text) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    gpid = Some(pid);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let gpid = gpid.expect("grandchild pid should be recorded");
        assert!(pid_alive(gpid), "grandchild {gpid} should be running");

        drop(transport);

        let mut dead = false;
        for _ in 0..40 {
            if !pid_alive(gpid) {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(dead, "grandchild {gpid} leaked after StdioTransport drop");
    }
}
