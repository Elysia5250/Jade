//! `envoy mcp serve`: Model Context Protocol server over stdio.
//!
//! The MCP surface is the *interaction* layer: `submit` returns immediately,
//! `run` is a convenience that follows until a deadline, and `job_logs` /
//! `job_cancel` work on jobs that outlived the call that started them.

use crate::daemon;
use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use clap::Subcommand;
use envoy_core::{parse_mode, AgentConfigStore};
use envoy_proto::{Job, JobState, SubmitRequest, TaskSpec, DEFAULT_GROUP, READ_LIMIT};
use envoy_service::{dialer_for, AgentDialer, Remote};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Per-stream cap for tool output (keeps one call from flooding the model).
const CAP: usize = 256 * 1024;
/// Default wall-clock budget for `run` before it hands back a job id.
const DEFAULT_WAIT_S: u64 = 120;

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

fn machine_schema() -> Value {
    json!({
        "type": "string",
        "description": "Target machine: `local` or a conduit device name/id.",
    })
}

fn tool_defs() -> Value {
    let machine = machine_schema();
    json!([
        {
            "name": "status",
            "description": "Show this device's agent policy, daemon state and job counts.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "run",
            "description": "Submit a shell command and follow it for up to `wait_s` seconds. If it is still running at the deadline, returns the job id so you can poll `job_logs` instead of blocking.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "command": { "type": "string", "description": "Shell command to run." },
                    "cwd": { "type": "string", "description": "Working directory on the target." },
                    "timeout_s": { "type": "integer", "description": "Kill the job after this many seconds (default 600)." },
                    "wait_s": { "type": "integer", "description": "How long this call follows before returning a job id (default 120)." },
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
            "name": "submit",
            "description": "Queue a shell command and return immediately with a job id. Use `job_logs` to read output later.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "timeout_s": { "type": "integer" },
                    "label": { "type": "string" },
                    "group": { "type": "string", "description": "Concurrency group (default `default`)." },
                    "env": {
                        "type": "object",
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["machine", "command"]
            }
        },
        {
            "name": "jobs",
            "description": "List jobs on a machine, oldest first.",
            "inputSchema": {
                "type": "object",
                "properties": { "machine": machine },
                "required": ["machine"]
            }
        },
        {
            "name": "job_status",
            "description": "Fetch one job's state, pid, log size and exit code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "id": { "type": "integer" }
                },
                "required": ["machine", "id"]
            }
        },
        {
            "name": "job_logs",
            "description": "Read a job's output from a byte cursor. Returns the next cursor so reads can be paged.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "id": { "type": "integer" },
                    "offset": { "type": "integer", "description": "Cursor from a previous call (default 0)." }
                },
                "required": ["machine", "id"]
            }
        },
        {
            "name": "job_cancel",
            "description": "Cancel a queued or running job (kills its process group).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "id": { "type": "integer" }
                },
                "required": ["machine", "id"]
            }
        },
        {
            "name": "read",
            "description": "Read a file as text (binary falls back to base64). Relative paths resolve against the target's `$HOME`.",
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
            "description": "Copy one file between machines, streaming in 4 MiB pages.",
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
        "status" => status(data_dir).await,
        "run" => run(data_dir, args).await,
        "submit" => submit(data_dir, args).await,
        "jobs" => jobs(data_dir, args).await,
        "job_status" => job_status(data_dir, args).await,
        "job_logs" => job_logs(data_dir, args).await,
        "job_cancel" => job_cancel(data_dir, args).await,
        "read" => read(data_dir, args).await,
        "write" => write(data_dir, args).await,
        "ls" => ls(data_dir, args).await,
        "copy" => copy(data_dir, args).await,
        other => Ok((format!("unknown tool: {other}"), true)),
    }
}

// ------------------------------------------------------------------ helpers

type AnyDialer = Box<dyn AgentDialer>;

async fn remote_for(data_dir: &Path, machine: &str) -> Result<Remote<AnyDialer>> {
    if machine == "local" || machine.is_empty() {
        daemon::ensure(data_dir).await?;
    }
    Ok(Remote::new(dialer_for(data_dir, machine)))
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

fn job_id(args: &Value) -> Result<u64> {
    args["id"]
        .as_u64()
        .context("missing or non-numeric `id`")
}

fn env_arg(args: &Value) -> Vec<(String, String)> {
    args["env"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn submit_request(args: &Value) -> Result<SubmitRequest> {
    let command = args["command"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .context("missing `command`")?;
    Ok(SubmitRequest {
        spec: TaskSpec::Shell {
            command: command.to_string(),
        },
        cwd: args["cwd"].as_str().map(str::to_string),
        env: env_arg(args),
        label: args["label"].as_str().map(str::to_string),
        group: args["group"]
            .as_str()
            .filter(|g| !g.is_empty())
            .unwrap_or(DEFAULT_GROUP)
            .to_string(),
        timeout_s: args["timeout_s"].as_u64().unwrap_or(0),
    })
}

fn mode_arg(args: &Value) -> Result<Option<u32>> {
    match args["mode"].as_str() {
        None => Ok(None),
        Some(m) => Ok(Some(parse_mode(m)?)),
    }
}

/// Bounded output buffer that keeps stdout and stderr separable.
struct Sink {
    out: Vec<u8>,
    err: Vec<u8>,
    truncated: bool,
}

impl Sink {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            err: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, fd: u8, chunk: &[u8]) {
        let used = self.out.len() + self.err.len();
        if used >= CAP {
            self.truncated = true;
            return;
        }
        let room = CAP - used;
        let bucket = if fd == 1 { &mut self.err } else { &mut self.out };
        if chunk.len() > room {
            bucket.extend_from_slice(&chunk[..room]);
            self.truncated = true;
        } else {
            bucket.extend_from_slice(chunk);
        }
    }

    fn push_records(&mut self, records: &[envoy_service::LogRecord]) {
        for r in records {
            self.push(r.fd, &r.data);
        }
    }

    fn text(&self) -> String {
        let mut s = String::from_utf8_lossy(&self.out).to_string();
        if !self.err.is_empty() {
            s.push_str("\n[stderr]\n");
            s.push_str(&String::from_utf8_lossy(&self.err));
        }
        if self.truncated {
            s.push_str("\n[truncated]");
        }
        s
    }
}

fn fmt_job(job: &Job) -> String {
    let state = match &job.state {
        JobState::Queued { .. } => "queued".to_string(),
        JobState::Running { .. } => match job.pid {
            Some(pid) => format!("running pid={pid}"),
            None => "running".to_string(),
        },
        JobState::Done { result, .. } => format!("done: {}", result.describe()),
    };
    format!(
        "#{} {:<9} group={} owner={} label={} log={}B  {}",
        job.id,
        state,
        job.group,
        job.owner,
        job.label.clone().unwrap_or_else(|| "-".into()),
        job.log_bytes,
        job.command()
    )
}

// -------------------------------------------------------------------- tools

async fn status(data_dir: &Path) -> Result<(String, bool)> {
    let store = AgentConfigStore::open(data_dir.to_path_buf())?;
    let running = daemon::is_running(data_dir).await;
    let mut jobs_line = json!(null);
    let mut running_jobs = 0;
    let mut queued_jobs = 0;
    if running {
        if let Ok(remote) = remote_for(data_dir, "local").await {
            if let Ok(jobs) = remote.jobs("local").await {
                running_jobs = jobs
                    .iter()
                    .filter(|j| matches!(j.state, JobState::Running { .. }))
                    .count();
                queued_jobs = jobs
                    .iter()
                    .filter(|j| matches!(j.state, JobState::Queued { .. }))
                    .count();
                jobs_line = json!(jobs.len());
            }
        }
    }
    let text = serde_json::to_string_pretty(&json!({
        "data_dir": store.root,
        "enabled": store.config.enabled,
        "allow": store.config.allow,
        "max_jobs": store.config.max_jobs,
        "daemon": if running { "running" } else { "stopped" },
        "jobs": jobs_line,
        "jobs_running": running_jobs,
        "jobs_queued": queued_jobs,
        "bridge": if daemon::bridge_socket_path(data_dir).exists() { "socket present" } else { "not running" },
    }))?;
    Ok((text, false))
}

async fn run(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let machine = machine_arg(args);
    let req = submit_request(args)?;
    let remote = remote_for(data_dir, &machine).await?;
    let job = remote.submit(&machine, req).await?;
    let wait_s = args["wait_s"].as_u64().unwrap_or(DEFAULT_WAIT_S).min(600);

    let mut sink = Sink::new();
    let followed = tokio::time::timeout(
        Duration::from_secs(wait_s),
        remote.logs(&machine, job.id, 0, true, |records| {
            sink.push_records(records)
        }),
    )
    .await;

    match followed {
        Ok(Ok((_cursor, finished))) => {
            let mut text = sink.text();
            text.push_str(&format!(
                "\njob {} exit={}",
                job.id,
                finished.unwrap_or(1)
            ));
            Ok((text, !matches!(finished, Some(0))))
        }
        Ok(Err(e)) => Ok((format!("{e:#}"), true)),
        Err(_) => {
            let mut text = sink.text();
            text.push_str(&format!(
                "\n[job {} still running after {}s; poll `job_logs` with id {}]",
                job.id, wait_s, job.id
            ));
            Ok((text, false))
        }
    }
}

async fn submit(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let machine = machine_arg(args);
    let req = submit_request(args)?;
    let remote = remote_for(data_dir, &machine).await?;
    let job = remote.submit(&machine, req).await?;
    Ok((format!("job {} accepted: {}", job.id, fmt_job(&job)), false))
}

async fn jobs(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let machine = machine_arg(args);
    let remote = remote_for(data_dir, &machine).await?;
    let jobs = remote.jobs(&machine).await?;
    if jobs.is_empty() {
        return Ok((format!("no jobs on {machine}"), false));
    }
    let mut text = String::new();
    for job in jobs {
        text.push_str(&fmt_job(&job));
        text.push('\n');
    }
    Ok((text, false))
}

async fn job_status(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let machine = machine_arg(args);
    let id = job_id(args)?;
    let remote = remote_for(data_dir, &machine).await?;
    let job = remote.status(&machine, id).await?;
    let text = serde_json::to_string_pretty(&json!({
        "id": job.id,
        "state": job.state.label(),
        "result": job.state.result().map(|r| r.describe()),
        "exit": job.state.exit_code(),
        "pid": job.pid,
        "group": job.group,
        "owner": job.owner,
        "command": job.command(),
        "cwd": job.cwd,
        "log_bytes": job.log_bytes,
    }))?;
    Ok((text, false))
}

async fn job_logs(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let machine = machine_arg(args);
    let id = job_id(args)?;
    let offset = args["offset"].as_u64().unwrap_or(0);
    let remote = remote_for(data_dir, &machine).await?;
    let mut sink = Sink::new();
    let (cursor, finished) = remote
        .logs(&machine, id, offset, false, |records| sink.push_records(records))
        .await?;
    let mut text = sink.text();
    text.push_str(&format!("\n\nnext={cursor}"));
    match finished {
        Some(code) => text.push_str(&format!(" exit={code}")),
        None => text.push_str(" (still running)"),
    }
    Ok((text, false))
}

async fn job_cancel(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let machine = machine_arg(args);
    let id = job_id(args)?;
    let remote = remote_for(data_dir, &machine).await?;
    remote.cancel(&machine, id).await?;
    Ok((format!("cancel requested for job {id} on {machine}"), false))
}

async fn read(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let machine = machine_arg(args);
    let path = args["path"].as_str().unwrap_or("");
    if path.is_empty() {
        return Ok(("missing `path`".into(), true));
    }
    let remote = remote_for(data_dir, &machine).await?;
    let (_size, _mode, data) = remote
        .read(
            &machine,
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
    let machine = machine_arg(args);
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
    let remote = remote_for(data_dir, &machine).await?;
    let bytes = remote.write(&machine, path, content.as_bytes(), mode).await?;
    Ok((format!("wrote {bytes} bytes to {path}"), false))
}

async fn ls(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let machine = machine_arg(args);
    let path = args["path"].as_str().unwrap_or(".");
    let remote = remote_for(data_dir, &machine).await?;
    let entries = remote.list(&machine, path).await?;
    let mut text = String::from("KIND      SIZE  MODE  NAME\n");
    for e in entries {
        let kind = match e.kind {
            envoy_proto::EntryKind::File => "file",
            envoy_proto::EntryKind::Dir => "dir",
            envoy_proto::EntryKind::Symlink => "symlink",
            envoy_proto::EntryKind::Other => "other",
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

async fn copy(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let from = endpoint(&args["from"]);
    let to = endpoint(&args["to"]);
    let (Some(from_path), Some(to_path)) = (args["from_path"].as_str(), args["to_path"].as_str())
    else {
        return Ok(("missing `from_path` or `to_path`".into(), true));
    };
    let mode = match mode_arg(args) {
        Ok(m) => m,
        Err(e) => return Ok((format!("{e:#}"), true)),
    };
    let src = remote_for(data_dir, &from).await?;
    let dst = remote_for(data_dir, &to).await?;

    let (_, src_mode, first) = src.read(&from, from_path, 0, READ_LIMIT).await?;
    let mut writer = dst
        .open_write(&to, to_path, mode.or(Some(src_mode)))
        .await?;
    let mut total = 0u64;
    let mut offset = 0u64;
    let mut page = first;
    loop {
        total += page.len() as u64;
        writer.send(&page).await?;
        offset += page.len() as u64;
        if (page.len() as u64) < READ_LIMIT {
            break;
        }
        let (_, _, next) = src.read(&from, from_path, offset, READ_LIMIT).await?;
        if next.is_empty() {
            break;
        }
        page = next;
    }
    writer.finish().await?;
    Ok((
        format!("copied {total} bytes: {from}:{from_path} -> {to}:{to_path}"),
        false,
    ))
}

