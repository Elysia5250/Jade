//! `envoy mcp serve`: standalone Model Context Protocol server over stdio.
//!
//! Works directly in-process via [`envoy_core::Runner`] and [`envoy_core::AgentConfigStore`]
//! for `local` targets. Remote machines require attaching a transport layer.

use envoy_core::{mode_of, resolve, set_mode, AgentConfigStore, Runner};
use envoy_proto::{AgentEvent, EntryKind};
use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use clap::Subcommand;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Per-stream cap for `run` output (keeps one call from flooding the model).
const CAP: usize = 256 * 1024;

#[derive(Subcommand, Debug)]
pub enum McpCmd {
    /// Speak MCP on stdin/stdout (for an MCP client's server config).
    Serve,
}

pub async fn cmd_mcp(data_dir: PathBuf, cmd: McpCmd) -> Result<()> {
    match cmd {
        McpCmd::Serve => serve(data_dir).await,
    }
}

async fn serve(data_dir: PathBuf) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                reply(&mut out, error(&Value::Null, -32700, &format!("parse error: {e}"))).await?;
                continue;
            }
        };
        let Some(resp) = handle_rpc(&data_dir, &msg).await? else {
            continue;
        };
        reply(&mut out, resp).await?;
    }
    Ok(())
}

async fn handle_rpc(data_dir: &Path, msg: &Value) -> Result<Option<Value>> {
    let method = msg["method"].as_str().unwrap_or("");
    if method.starts_with("notifications/") {
        return Ok(None);
    }
    let Some(id) = msg.get("id").cloned() else {
        return Ok(None);
    };
    let resp = match method {
        "initialize" => response(&id, initialize(&msg["params"])),
        "ping" => response(&id, json!({})),
        "tools/list" => response(&id, json!({ "tools": tool_defs() })),
        "tools/call" => match call(data_dir, &msg["params"]).await {
            Ok((text, is_error)) => response(
                &id,
                json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": is_error,
                }),
            ),
            Err(e) => error(&id, -32603, &format!("{e:#}")),
        },
        other => error(&id, -32601, &format!("method not found: {other}")),
    };
    Ok(Some(resp))
}

async fn reply(out: &mut tokio::io::Stdout, value: Value) -> Result<()> {
    let mut text = serde_json::to_vec(&value)?;
    text.push(b'\n');
    out.write_all(&text).await?;
    out.flush().await?;
    Ok(())
}

fn response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn initialize(params: &Value) -> Value {
    let version = params["protocolVersion"]
        .as_str()
        .unwrap_or("2024-11-05");
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "envoy",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn tool_defs() -> Value {
    let machine = json!({
        "type": "string",
        "description": "Target machine (`local` in standalone mode).",
    });
    json!([
        {
            "name": "status",
            "description": "Show this standalone agent's policy configuration (enabled state, allowlist, max_jobs).",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "run",
            "description": "Run a shell command (wrapped in `sh -c`). Returns stdout, stderr and exit code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "command": { "type": "string", "description": "Shell command to run." },
                    "cwd": { "type": "string", "description": "Working directory on the target." },
                    "timeout_s": { "type": "integer", "description": "Kill the process group after this many seconds (default 600)." },
                    "env": {
                        "type": "object",
                        "description": "Extra environment variables.",
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["machine", "command"]
            }
        },
        {
            "name": "read",
            "description": "Read a file as text (binary falls back to base64). Relative paths resolve against `$HOME`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "Byte offset (default 0)." },
                    "limit": { "type": "integer", "description": "Max bytes (default and cap: 4 MiB)." }
                },
                "required": ["machine", "path"]
            }
        },
        {
            "name": "write",
            "description": "Write a text file atomically (temp file + rename).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "mode": { "type": "string", "description": "Octal permission bits, e.g. \"644\"." }
                },
                "required": ["machine", "path", "content"]
            }
        },
        {
            "name": "ls",
            "description": "List one directory level.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "path": { "type": "string", "description": "Directory (default: `$HOME`)." }
                },
                "required": ["machine"]
            }
        },
        {
            "name": "copy",
            "description": "Copy one file locally.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": machine,
                    "from_path": { "type": "string" },
                    "to": machine,
                    "to_path": { "type": "string" },
                    "mode": { "type": "string", "description": "Octal permission bits for the destination." }
                },
                "required": ["from", "from_path", "to", "to_path"]
            }
        }
    ])
}

async fn call(data_dir: &Path, params: &Value) -> Result<(String, bool)> {
    let name = params["name"].as_str().unwrap_or("");
    let args = &params["arguments"];
    match name {
        "status" => status(data_dir),
        "run" => run(data_dir, args).await,
        "read" => read(data_dir, args).await,
        "write" => write(data_dir, args).await,
        "ls" => ls(data_dir, args).await,
        "copy" => copy(args).await,
        other => Ok((format!("unknown tool: {other}"), true)),
    }
}

fn machine_arg(args: &Value) -> String {
    args["machine"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("local")
        .to_string()
}

fn endpoint(v: &Value) -> String {
    v.as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("local")
        .to_string()
}

fn check_local(target: &str) -> Option<(String, bool)> {
    if target == "local" {
        None
    } else {
        Some((
            format!(
                "machine `{target}` unavailable: standalone `envoy` has no remote transport attached"
            ),
            true,
        ))
    }
}

fn mode_arg(args: &Value) -> Result<Option<u32>> {
    match args["mode"].as_str() {
        None => Ok(None),
        Some(m) => {
            let digits = m.strip_prefix("0o").unwrap_or(m);
            Ok(Some(
                u32::from_str_radix(digits, 8)
                    .with_context(|| format!("mode `{m}` is not octal (e.g. 644)"))?,
            ))
        }
    }
}

fn status(data_dir: &Path) -> Result<(String, bool)> {
    let store = AgentConfigStore::open(data_dir.to_path_buf())?;
    let text = serde_json::to_string_pretty(&json!({
        "data_dir": store.root,
        "enabled": store.config.enabled,
        "allow": store.config.allow,
        "max_jobs": store.config.max_jobs,
    }))?;
    Ok((text, false))
}

struct Sink {
    data: Vec<u8>,
    truncated: bool,
}

impl Sink {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if self.data.len() >= CAP {
            self.truncated = true;
            return;
        }
        let room = CAP - self.data.len();
        if chunk.len() > room {
            self.data.extend_from_slice(&chunk[..room]);
            self.truncated = true;
        } else {
            self.data.extend_from_slice(chunk);
        }
    }

    fn text(&self) -> String {
        let mut s = String::from_utf8_lossy(&self.data).to_string();
        if self.truncated {
            s.push_str("\n[truncated]");
        }
        s
    }
}

async fn run(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let target = machine_arg(args);
    if let Some(err) = check_local(&target) {
        return Ok(err);
    }
    let command = match args["command"].as_str() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => return Ok(("missing `command`".into(), true)),
    };
    let cwd = args["cwd"].as_str().map(str::to_string);
    let timeout_s = args["timeout_s"].as_u64().unwrap_or(0);
    let env: Vec<(String, String)> = args["env"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let runner = Runner::new(data_dir.to_path_buf());
    let task = tokio::spawn(async move {
        let cancel = tokio::sync::Notify::new();
        let argv = vec!["sh".to_string(), "-c".to_string(), command];
        let _ = runner
            .exec_into(&argv, cwd, &env, timeout_s, &tx, &cancel)
            .await;
    });

    let mut out = Sink::new();
    let mut err = Sink::new();
    let res = loop {
        match rx.recv().await {
            Some(AgentEvent::Out { fd, data }) => {
                if fd == 1 {
                    err.push(&data);
                } else {
                    out.push(&data);
                }
            }
            Some(AgentEvent::Done { code, ms }) => {
                let mut text = out.text();
                if !err.data.is_empty() {
                    text.push_str("\n[stderr]\n");
                    text.push_str(&err.text());
                }
                text.push_str(&format!("\nexit={code} ms={ms}"));
                break Ok((text, false));
            }
            Some(AgentEvent::Failed { error }) => break Ok((error, true)),
            Some(AgentEvent::Denied { reason }) => break Ok((format!("denied: {reason}"), true)),
            Some(_) => {}
            None => break Ok(("the agent stream ended without a result".into(), true)),
        }
    };
    let _ = task.await;
    res
}

async fn read(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let target = machine_arg(args);
    if let Some(err) = check_local(&target) {
        return Ok(err);
    }
    let path = args["path"].as_str().unwrap_or("");
    if path.is_empty() {
        return Ok(("missing `path`".into(), true));
    }
    let (_size, _mode, data) = Runner::new(data_dir.to_path_buf())
        .read_range(
            path,
            args["offset"].as_u64().unwrap_or(0),
            args["limit"].as_u64().unwrap_or(0),
        )
        .await?;
    let body = match String::from_utf8(data) {
        Ok(text) => text,
        Err(e) => format!("[base64]\n{}", B64.encode(e.into_bytes())),
    };
    Ok((body, false))
}

async fn write(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let target = machine_arg(args);
    if let Some(err) = check_local(&target) {
        return Ok(err);
    }
    let path = args["path"].as_str().unwrap_or("");
    if path.is_empty() {
        return Ok(("missing `path`".into(), true));
    }
    let Some(content) = args["content"].as_str() else {
        return Ok(("missing `content`".into(), true));
    };
    let mode = match mode_arg(args) {
        Ok(m) => m,
        Err(e) => return Ok((format!("{e:#}"), true)),
    };
    let bytes = Runner::new(data_dir.to_path_buf())
        .write_file(path, content.as_bytes(), mode)
        .await?;
    Ok((format!("wrote {bytes} bytes to {path}"), false))
}

async fn ls(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let target = machine_arg(args);
    if let Some(err) = check_local(&target) {
        return Ok(err);
    }
    let path = args["path"].as_str().unwrap_or(".");
    let entries = Runner::new(data_dir.to_path_buf()).list_dir(path).await?;
    let mut text = String::from("KIND      SIZE  MODE  NAME\n");
    for e in entries {
        let kind = match e.kind {
            EntryKind::File => "file",
            EntryKind::Dir => "dir",
            EntryKind::Symlink => "symlink",
            EntryKind::Other => "other",
        };
        text.push_str(&format!(
            "{:<8} {:>8}  {:<5o} {}\n",
            kind,
            e.size,
            e.mode & 0o777,
            e.name
        ));
    }
    Ok((text, false))
}

async fn copy(args: &Value) -> Result<(String, bool)> {
    let from = endpoint(&args["from"]);
    if let Some(err) = check_local(&from) {
        return Ok(err);
    }
    let to = endpoint(&args["to"]);
    if let Some(err) = check_local(&to) {
        return Ok(err);
    }
    let (Some(from_path), Some(to_path)) = (args["from_path"].as_str(), args["to_path"].as_str())
    else {
        return Ok(("missing `from_path` or `to_path`".into(), true));
    };
    let mode = match mode_arg(args) {
        Ok(m) => m,
        Err(e) => return Ok((format!("{e:#}"), true)),
    };
    let src = resolve(from_path);
    let dst = resolve(to_path);
    let meta = tokio::fs::metadata(&src)
        .await
        .with_context(|| format!("stat {}", src.display()))?;
    let bytes = tokio::fs::copy(&src, &dst)
        .await
        .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    set_mode(&dst, mode.unwrap_or_else(|| mode_of(&meta)));
    Ok((format!("copied {bytes} bytes to {to_path}"), false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mcp_initialize_list_and_local_run() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("agent-mcp-{n}"));
        std::fs::create_dir_all(&dir).unwrap();

        let init = handle_rpc(
            &dir,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "envoy");

        let list = handle_rpc(
            &dir,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(list["result"]["tools"].as_array().unwrap().len() >= 5);

        let run_resp = handle_rpc(
            &dir,
            &json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "run",
                    "arguments": { "machine": "local", "command": "echo mcp-ok" }
                }
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let text = run_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("mcp-ok"), "{text}");
        assert_eq!(run_resp["result"]["isError"], false);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
