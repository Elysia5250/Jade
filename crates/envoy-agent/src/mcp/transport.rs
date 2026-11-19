//! MCP client transports: stdio (JSON-RPC lines) and Streamable HTTP (`POST` JSON + SSE).

use super::config::{safe_stdio_env, McpTransportConfig};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[allow(clippy::large_enum_variant)]
pub enum McpTransport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

pub struct StdioTransport {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: AtomicU64,
}

pub struct HttpTransport {
    http: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session_id: Option<String>,
    next_id: AtomicU64,
}

impl McpTransport {
    pub async fn connect(cfg: &McpTransportConfig, timeout_s: u64) -> Result<Self> {
        let mut conn = match cfg {
            McpTransportConfig::Stdio {
                command,
                args,
                env,
                cwd,
            } => Self::Stdio(StdioTransport::spawn(command, args, env, cwd.as_ref())?),
            McpTransportConfig::Http { url, headers } => {
                Self::Http(HttpTransport::new(url.clone(), headers.clone(), timeout_s)?)
            }
        };

        let timeout = Duration::from_secs(timeout_s);
        let init_params = json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "envoy",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        let _ = conn.request("initialize", init_params, timeout).await?;
        let _ = conn
            .notify("notifications/initialized", json!({}), timeout)
            .await;
        Ok(conn)
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

impl StdioTransport {
    fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: Option<&PathBuf>,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .env_clear()
            .envs(safe_stdio_env(env))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server `{command}`"))?;
        let stdin = child.stdin.take().context("MCP child stdin missing")?;
        let stdout = BufReader::new(child.stdout.take().context("MCP child stdout missing")?);
        Ok(Self {
            _child: child,
            stdin,
            stdout,
            next_id: AtomicU64::new(1),
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
            let mut line = String::new();
            loop {
                line.clear();
                let n = self.stdout.read_line(&mut line).await?;
                if n == 0 {
                    bail!("MCP stdio server closed stdout");
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
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

impl HttpTransport {
    fn new(url: String, headers: BTreeMap<String, String>, timeout_s: u64) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_s))
            .build()?;
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
        let body = resp.text().await?;
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
