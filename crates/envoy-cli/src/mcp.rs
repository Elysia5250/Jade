//! `envoy mcp serve`: Model Context Protocol server over stdio.
//!
//! The tool surface is deliberately small — six orthogonal primitives, in the
//! spirit of a minimal agent harness:
//!
//! * `exec`  run a command (optionally follow it for a while)
//! * `read`  read bytes
//! * `write` write a whole file atomically
//! * `edit`  replace one exact snippet atomically
//! * `ls`    list one directory level
//! * `job`   list / status / logs / cancel
//!
//! Everything heavier (copying, policy, devices, dashboards) is composed by the
//! caller from these, or lives in the CLI. There is no plan mode, no sub-agents,
//! and no hidden permission layer here: the daemon's policy gate decides.

use crate::daemon;
use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use clap::Subcommand;
use envoy_core::parse_mode;
use envoy_proto::{Job, JobState, LogRecord, SubmitRequest, TaskSpec, DEFAULT_GROUP};
use envoy_service::{dialer_for, AgentDialer, Remote};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Per-call output cap (keeps one tool result from flooding the model).
const CAP: usize = 256 * 1024;
/// Default wall-clock budget for `exec` before it hands back a job id.
const DEFAULT_WAIT_S: u64 = 120;
/// Record header size in a job log: `fd` + `u32` length.
const RECORD_HEADER: u64 = 5;

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

// ---------------------------------------------------------------- tool defs

fn machine_prop() -> Value {
    json!({
        "type": "string",
        "default": "local",
        "description": "Target machine: `local` or a conduit device name/id.",
    })
}

fn tool_defs() -> Value {
    let machine = machine_prop();
    json!([
        {
            "name": "exec",
            "description": "Run a shell command and return its output and exit code. If it outlives `wait_s`, returns a job id instead of blocking; read it later with `job`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "command": { "type": "string", "description": "Shell command (run through `sh -c`)." },
                    "cwd": { "type": "string", "description": "Working directory (default: the target's $HOME)." },
                    "env": {
                        "type": "object",
                        "description": "Extra environment variables.",
                        "additionalProperties": { "type": "string" }
                    },
                    "timeout_s": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 600,
                        "description": "Kill the process group after this many seconds (0 = agent default)."
                    },
                    "wait_s": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 120,
                        "description": "Follow for at most this long. 0 queues and returns immediately."
                    },
                    "label": { "type": "string", "description": "Free-form label for listings." },
                    "group": { "type": "string", "default": "default", "description": "Concurrency group." }
                },
                "required": ["command"]
            }
        },
        {
            "name": "read",
            "description": "Read a file as text (binary falls back to base64). Relative paths resolve against the target's $HOME.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 0, "default": 0, "description": "Byte offset." },
                    "limit": { "type": "integer", "minimum": 0, "default": 0, "description": "Max bytes (default and cap: 4 MiB)." }
                },
                "required": ["path"]
            }
        },
        {
            "name": "write",
            "description": "Write a whole file atomically (sibling .part + rename). Use `edit` for a small change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "mode": { "type": "string", "description": "Octal permission bits, e.g. \"644\"." }
                },
                "required": ["path", "content"]
            }
        },
        {
            "name": "edit",
            "description": "Replace one exact snippet in a text file, atomically. Fails if the snippet is absent or appears more than once (unless `replace_all`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "path": { "type": "string" },
                    "old": { "type": "string", "description": "Exact text to replace." },
                    "new": { "type": "string", "description": "Replacement text." },
                    "replace_all": { "type": "boolean", "default": false }
                },
                "required": ["path", "old", "new"]
            }
        },
        {
            "name": "ls",
            "description": "List one directory level, sorted, with sizes and modes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "path": { "type": "string", "description": "Directory (default: the target's $HOME)." }
                }
            }
        },
        {
            "name": "job",
            "description": "Inspect or control jobs. `list` needs no id; `status`, `logs` and `cancel` take one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "action": {
                        "type": "string",
                        "enum": ["list", "status", "logs", "cancel"]
                    },
                    "id": { "type": "integer", "minimum": 1, "description": "Required unless `action` is `list`." },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0,
                        "description": "For `logs`: the cursor returned by a previous call."
                    }
                },
                "required": ["action"]
            }
        }
    ])
}

async fn call(data_dir: &Path, params: &Value) -> Result<(String, bool)> {
    let name = params["name"].as_str().unwrap_or("");
    let args = &params["arguments"];
    match name {
        "exec" => exec(data_dir, args).await,
        "read" => read(data_dir, args).await,
        "write" => write(data_dir, args).await,
        "edit" => edit(data_dir, args).await,
        "ls" => ls(data_dir, args).await,
        "job" => job(data_dir, args).await,
        other => Ok((
            format!("unknown tool `{other}`; available: exec, read, write, edit, ls, job"),
            true,
        )),
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

fn job_id(args: &Value) -> Result<u64> {
    args["id"]
        .as_u64()
        .filter(|id| *id > 0)
        .context("missing `id` (required for this action)")
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

fn exec_request(args: &Value) -> Result<SubmitRequest> {
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

/// Bounded output buffer. Keeps stdout/stderr separable and tracks how far the
/// caller has actually consumed the job log, so a truncated result still hands
/// back a correct resume cursor.
struct Sink {
    out: Vec<u8>,
    err: Vec<u8>,
    truncated: bool,
    cursor: u64,
}

impl Sink {
    fn new(offset: u64) -> Self {
        Self {
            out: Vec::new(),
            err: Vec::new(),
            truncated: false,
            cursor: offset,
        }
    }

    fn room(&self) -> usize {
        CAP.saturating_sub(self.out.len() + self.err.len())
    }

    fn push_bytes(&mut self, fd: u8, chunk: &[u8], fully: bool) {
        let bucket = if fd == 1 { &mut self.err } else { &mut self.out };
        bucket.extend_from_slice(chunk);
        if !fully {
            self.truncated = true;
        }
    }

    fn push_record(&mut self, record: &LogRecord) {
        let end = record.seq + RECORD_HEADER + record.data.len() as u64;
        let room = self.room();
        if room == 0 {
            self.truncated = true;
            return;
        }
        if record.data.len() <= room {
            self.push_bytes(record.fd, &record.data, true);
            self.cursor = end;
        } else {
            self.push_bytes(record.fd, &record.data[..room], false);
        }
    }

    fn push_records(&mut self, records: &[LogRecord]) {
        for record in records {
            if self.truncated {
                return;
            }
            self.push_record(record);
        }
    }

    fn text(&self) -> String {
        let mut s = String::from_utf8_lossy(&self.out).to_string();
        if !self.err.is_empty() {
            s.push_str("\n[stderr]\n");
            s.push_str(&String::from_utf8_lossy(&self.err));
        }
        s
    }
}

fn truncation_note(sink: &Sink) -> String {
    if sink.truncated {
        format!(
            "\n[truncated at {} KiB; resume with job action=logs offset={}]",
            CAP / 1024,
            sink.cursor
        )
    } else {
        String::new()
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
        "#{} {:<16} group={} owner={} log={}B  {}",
        job.id,
        state,
        job.group,
        job.owner,
        job.log_bytes,
        job.command()
    )
}

// -------------------------------------------------------------------- tools

async fn exec(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let machine = machine_arg(args);
    let req = exec_request(args)?;
    let remote = remote_for(data_dir, &machine).await?;
    let job = remote.submit(&machine, req).await?;
    let wait_s = args["wait_s"].as_u64().unwrap_or(DEFAULT_WAIT_S).min(3600);

    if wait_s == 0 {
        return Ok((
            format!(
                "job {} queued on {machine}: {}\n[job action=logs id={} to read it]",
                job.id,
                job.command(),
                job.id
            ),
            false,
        ));
    }

    let mut sink = Sink::new(0);
    let followed = tokio::time::timeout(
        Duration::from_secs(wait_s),
        remote.logs(&machine, job.id, 0, true, |records| sink.push_records(records)),
    )
    .await;

    match followed {
        Ok(Ok((_cursor, finished))) => {
            let mut text = sink.text();
            text.push_str(&truncation_note(&sink));
            text.push_str(&format!(
                "\n[job {} exit={}]",
                job.id,
                finished.unwrap_or(1)
            ));
            Ok((text, !matches!(finished, Some(0))))
        }
        Ok(Err(e)) => Ok((format!("{e:#}"), true)),
        Err(_) => {
            let mut text = sink.text();
            text.push_str(&truncation_note(&sink));
            text.push_str(&format!(
                "\n[job {} still running after {wait_s}s; job action=logs id={}]",
                job.id, job.id
            ));
            Ok((text, false))
        }
    }
}

async fn read(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let machine = machine_arg(args);
    let path = args["path"].as_str().unwrap_or("");
    if path.is_empty() {
        return Ok(("missing `path`".into(), true));
    }
    let remote = remote_for(data_dir, &machine).await?;
    let (size, _mode, data) = remote
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
    // A full-page read almost always means there is more.
    let more = args["limit"].as_u64().unwrap_or(0) == 0 && size > envoy_proto::READ_LIMIT;
    Ok((
        if more {
            format!(
                "{body}\n\n[{} bytes total; read with offset={}]",
                size,
                envoy_proto::READ_LIMIT
            )
        } else {
            body
        },
        false,
    ))
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
    let bytes = remote
        .write(&machine, path, content.as_bytes(), mode)
        .await?;
    Ok((format!("wrote {bytes} bytes to {path} on {machine}"), false))
}

async fn edit(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let machine = machine_arg(args);
    let path = args["path"].as_str().unwrap_or("");
    let Some(old) = args["old"].as_str() else {
        return Ok(("missing `old`".into(), true));
    };
    let Some(new) = args["new"].as_str() else {
        return Ok(("missing `new`".into(), true));
    };
    let replace_all = args["replace_all"].as_bool().unwrap_or(false);
    let remote = remote_for(data_dir, &machine).await?;
    match remote
        .edit(&machine, path, old, new, replace_all)
        .await
    {
        Ok(n) => Ok((
            format!("edited {path} on {machine}: {n} replacement(s)"),
            false,
        )),
        Err(e) => Ok((format!("{e:#}"), true)),
    }
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

async fn job(data_dir: &Path, args: &Value) -> Result<(String, bool)> {
    let machine = machine_arg(args);
    let action = args["action"].as_str().unwrap_or("");
    let remote = remote_for(data_dir, &machine).await?;
    match action {
        "list" => {
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
        "status" => {
            let id = job_id(args)?;
            let job = remote.status(&machine, id).await?;
            Ok((
                serde_json::to_string_pretty(&json!({
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
                }))?,
                false,
            ))
        }
        "logs" => {
            let id = job_id(args)?;
            let offset = args["offset"].as_u64().unwrap_or(0);
            let mut sink = Sink::new(offset);
            let (_server_cursor, finished) = remote
                .logs(&machine, id, offset, false, |records| sink.push_records(records))
                .await?;
            let mut text = sink.text();
            text.push_str(&truncation_note(&sink));
            text.push_str(&format!("\n\nnext={}", sink.cursor));
            match finished {
                Some(code) => text.push_str(&format!(" exit={code}")),
                None => text.push_str(" (running)"),
            }
            Ok((text, false))
        }
        "cancel" => {
            let id = job_id(args)?;
            remote.cancel(&machine, id).await?;
            Ok((format!("cancel requested for job {id} on {machine}"), false))
        }
        other => Ok((
            format!("unknown action `{other}`; use list, status, logs or cancel"),
            true,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_primitives_exactly() {
        let tools = tool_defs();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["exec", "read", "write", "edit", "ls", "job"]);
    }

    #[test]
    fn sink_reports_the_last_fully_consumed_cursor() {
        let mut sink = Sink::new(0);
        sink.push_records(&[
            LogRecord {
                seq: 0,
                fd: 0,
                data: b"hello".to_vec(),
            },
            LogRecord {
                seq: 10,
                fd: 1,
                data: b"err".to_vec(),
            },
        ]);
        assert_eq!(sink.cursor, 10 + RECORD_HEADER + 3);
        assert!(!sink.truncated);
        assert!(sink.text().contains("[stderr]"));

        // Resume from that cursor yields nothing new.
        let mut resume = Sink::new(sink.cursor);
        resume.push_records(&[]);
        assert_eq!(resume.cursor, sink.cursor);
    }
}
