//! MCP server tool definitions (with annotations + outputSchema) and handlers.

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use envoy_core::parse_mode;
use envoy_proto::{
    Job, JobState, LogRecord, SandboxPolicy, SubmitRequest, TaskSpec, DEFAULT_GROUP,
};
use envoy_service::{AgentDialer, Remote};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

pub(crate) const CAP: usize = 256 * 1024;
const DEFAULT_WAIT_S: u64 = 120;
pub(crate) const RECORD_HEADER: u64 = 5;

pub(crate) type ToolReply = (String, Option<Value>, bool);

fn machine_prop() -> Value {
    json!({
        "type": "string",
        "default": "local",
        "description": "Target machine: `local` or a conduit device name/id.",
    })
}

pub(crate) fn tool_defs() -> Value {
    let machine = machine_prop();
    json!([
        {
            "name": "exec",
            "description": "Run a shell command and return its output and exit code. Local runs inside a workspace-write offline sandbox by default.",
            "annotations": { "title": "Run Shell Command", "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "machine": { "type": "string" },
                    "job_id": { "type": "integer" },
                    "exit_code": { "type": ["integer", "null"] },
                    "running": { "type": "boolean" },
                    "output": { "type": "string" }
                }
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "command": { "type": "string", "description": "Shell command (run through `sh -c`)." },
                    "cwd": { "type": "string", "description": "Working directory (default: session cwd or target $HOME)." },
                    "env": { "type": "object", "additionalProperties": { "type": "string" } },
                    "timeout_s": { "type": "integer", "minimum": 0, "default": 600 },
                    "wait_s": { "type": "integer", "minimum": 0, "default": 120 },
                    "label": { "type": "string" },
                    "group": { "type": "string", "default": "default" }
                },
                "required": ["command"]
            }
        },
        {
            "name": "read",
            "description": "Read a file as text (binary falls back to base64).",
            "annotations": { "title": "Read File", "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "outputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" }, "size": { "type": "integer" }, "content": { "type": "string" } }
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 },
                    "limit": { "type": "integer", "minimum": 0, "default": 0 }
                },
                "required": ["path"]
            }
        },
        {
            "name": "write",
            "description": "Write a whole file atomically (sibling .part + rename).",
            "annotations": { "title": "Write File", "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false },
            "outputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" }, "bytes": { "type": "integer" } }
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "mode": { "type": "string" }
                },
                "required": ["path", "content"]
            }
        },
        {
            "name": "edit",
            "description": "Replace one exact snippet in a text file atomically.",
            "annotations": { "title": "Edit File", "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false },
            "outputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" }, "replacements": { "type": "integer" } }
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "path": { "type": "string" },
                    "old": { "type": "string" },
                    "new": { "type": "string" },
                    "replace_all": { "type": "boolean", "default": false }
                },
                "required": ["path", "old", "new"]
            }
        },
        {
            "name": "ls",
            "description": "List one directory level, sorted, with sizes and modes.",
            "annotations": { "title": "List Directory", "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "inputSchema": {
                "type": "object",
                "properties": { "machine": machine, "path": { "type": "string" } }
            }
        },
        {
            "name": "glob",
            "description": "Find files matching a glob pattern (for example `**/*.rs`), skipping build artifacts.",
            "annotations": { "title": "Glob Search", "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "outputSchema": {
                "type": "object",
                "properties": { "paths": { "type": "array", "items": { "type": "string" } } }
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "pattern": { "type": "string" },
                    "path": { "type": "string", "default": "." },
                    "max_results": { "type": "integer", "minimum": 1, "default": 200 }
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "grep",
            "description": "Search file contents by regex pattern on the target machine.",
            "annotations": { "title": "Grep Search", "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "outputSchema": {
                "type": "object",
                "properties": { "count": { "type": "integer" } }
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "pattern": { "type": "string" },
                    "path": { "type": "string", "default": "." },
                    "include": { "type": "string" },
                    "max_results": { "type": "integer", "minimum": 1, "default": 100 }
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "job",
            "description": "Inspect or control jobs (`list`, `status`, `logs`, `cancel`).",
            "annotations": { "title": "Manage Jobs", "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "machine": machine,
                    "action": { "type": "string", "enum": ["list", "status", "logs", "cancel"] },
                    "id": { "type": "integer", "minimum": 1 },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 }
                },
                "required": ["action"]
            }
        }
    ])
}

pub(crate) fn machine_arg(args: &Value) -> String {
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

pub(crate) fn exec_request(
    args: &Value,
    default_cwd: &Path,
    sandbox: SandboxPolicy,
) -> Result<SubmitRequest> {
    let command = args["command"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .context("missing `command`")?;
    let cwd = args["cwd"]
        .as_str()
        .map(str::to_string)
        .or_else(|| Some(default_cwd.display().to_string()));
    Ok(SubmitRequest {
        spec: TaskSpec::Shell {
            command: command.to_string(),
        },
        cwd,
        env: env_arg(args),
        label: args["label"].as_str().map(str::to_string),
        group: args["group"]
            .as_str()
            .filter(|g| !g.is_empty())
            .unwrap_or(DEFAULT_GROUP)
            .to_string(),
        timeout_s: args["timeout_s"].as_u64().unwrap_or(0),
        sandbox,
    })
}

pub(crate) struct Sink {
    out: Vec<u8>,
    err: Vec<u8>,
    pub(crate) truncated: bool,
    pub(crate) cursor: u64,
}

impl Sink {
    pub(crate) fn new(offset: u64) -> Self {
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
        let bucket = if fd == 1 {
            &mut self.err
        } else {
            &mut self.out
        };
        bucket.extend_from_slice(chunk);
        if !fully {
            self.truncated = true;
        }
    }

    pub(crate) fn push_records(&mut self, records: &[LogRecord]) {
        for record in records {
            if self.truncated {
                return;
            }
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
    }

    pub(crate) fn text(&self) -> String {
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

pub(crate) async fn exec(
    remote: Remote<Box<dyn AgentDialer>>,
    machine: &str,
    default_cwd: &Path,
    sandbox: SandboxPolicy,
    args: &Value,
) -> Result<ToolReply> {
    let req = exec_request(args, default_cwd, sandbox)?;
    let job = remote.submit(machine, req).await?;
    let wait_s = args["wait_s"].as_u64().unwrap_or(DEFAULT_WAIT_S).min(3600);

    if wait_s == 0 {
        let text = format!(
            "job {} queued on {machine}: {}\n[job action=logs id={} to read it]",
            job.id,
            job.command(),
            job.id
        );
        return Ok((
            text.clone(),
            Some(
                json!({ "machine": machine, "job_id": job.id, "exit_code": null, "running": true, "output": text }),
            ),
            false,
        ));
    }

    let mut sink = Sink::new(0);
    let followed = tokio::time::timeout(
        Duration::from_secs(wait_s),
        remote.logs(machine, job.id, 0, true, |records| {
            sink.push_records(records)
        }),
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
            Ok((
                text.clone(),
                Some(
                    json!({ "machine": machine, "job_id": job.id, "exit_code": finished, "running": false, "output": text }),
                ),
                !matches!(finished, Some(0)),
            ))
        }
        Ok(Err(e)) => Ok((format!("{e:#}"), None, true)),
        Err(_) => {
            let mut text = sink.text();
            text.push_str(&truncation_note(&sink));
            text.push_str(&format!(
                "\n[job {} still running after {wait_s}s; job action=logs id={}]",
                job.id, job.id
            ));
            Ok((
                text.clone(),
                Some(
                    json!({ "machine": machine, "job_id": job.id, "exit_code": null, "running": true, "output": text }),
                ),
                false,
            ))
        }
    }
}

pub(crate) async fn read(
    remote: Remote<Box<dyn AgentDialer>>,
    machine: &str,
    args: &Value,
) -> Result<ToolReply> {
    let path = args["path"].as_str().unwrap_or("");
    if path.is_empty() {
        return Ok(("missing `path`".into(), None, true));
    }
    let (size, _mode, data) = remote
        .read(
            machine,
            path,
            args["offset"].as_u64().unwrap_or(0),
            args["limit"].as_u64().unwrap_or(0),
        )
        .await?;
    let body = match String::from_utf8(data) {
        Ok(text) => text,
        Err(e) => format!("[base64]\n{}", B64.encode(e.into_bytes())),
    };
    let more = args["limit"].as_u64().unwrap_or(0) == 0 && size > envoy_proto::READ_LIMIT;
    let out = if more {
        format!(
            "{body}\n\n[{} bytes total; read with offset={}]",
            size,
            envoy_proto::READ_LIMIT
        )
    } else {
        body
    };
    Ok((
        out.clone(),
        Some(json!({ "path": path, "size": size, "content": out })),
        false,
    ))
}

pub(crate) async fn write(
    remote: Remote<Box<dyn AgentDialer>>,
    machine: &str,
    args: &Value,
) -> Result<ToolReply> {
    let path = args["path"].as_str().unwrap_or("");
    if path.is_empty() {
        return Ok(("missing `path`".into(), None, true));
    }
    let Some(content) = args["content"].as_str() else {
        return Ok(("missing `content`".into(), None, true));
    };
    let mode = match args["mode"].as_str() {
        None => None,
        Some(m) => match parse_mode(m) {
            Ok(v) => Some(v),
            Err(e) => return Ok((format!("{e:#}"), None, true)),
        },
    };
    let bytes = remote
        .write(machine, path, content.as_bytes(), mode)
        .await?;
    Ok((
        format!("wrote {bytes} bytes to {path} on {machine}"),
        Some(json!({ "path": path, "bytes": bytes })),
        false,
    ))
}

pub(crate) async fn edit(
    remote: Remote<Box<dyn AgentDialer>>,
    machine: &str,
    args: &Value,
) -> Result<ToolReply> {
    let path = args["path"].as_str().unwrap_or("");
    let Some(old) = args["old"].as_str() else {
        return Ok(("missing `old`".into(), None, true));
    };
    let Some(new) = args["new"].as_str() else {
        return Ok(("missing `new`".into(), None, true));
    };
    let replace_all = args["replace_all"].as_bool().unwrap_or(false);
    match remote.edit(machine, path, old, new, replace_all).await {
        Ok(n) => Ok((
            format!("edited {path} on {machine}: {n} replacement(s)"),
            Some(json!({ "path": path, "replacements": n })),
            false,
        )),
        Err(e) => Ok((format!("{e:#}"), None, true)),
    }
}

pub(crate) async fn ls(
    remote: Remote<Box<dyn AgentDialer>>,
    machine: &str,
    args: &Value,
) -> Result<ToolReply> {
    let path = args["path"].as_str().unwrap_or(".");
    let entries = remote.list(machine, path).await?;
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
    Ok((text, None, false))
}

pub(crate) async fn glob(
    remote: Remote<Box<dyn AgentDialer>>,
    machine: &str,
    args: &Value,
) -> Result<ToolReply> {
    let pattern = args["pattern"].as_str().unwrap_or("");
    if pattern.trim().is_empty() {
        return Ok(("missing `pattern`".into(), None, true));
    }
    let root = args["path"].as_str().unwrap_or(".");
    let max_results = args["max_results"].as_u64().unwrap_or(200).clamp(1, 2000) as u32;
    let paths = remote.glob(machine, root, pattern, max_results).await?;
    let text = if paths.is_empty() {
        "(no matching files)".into()
    } else {
        paths.join("\n")
    };
    Ok((text, Some(json!({ "paths": paths })), false))
}

pub(crate) async fn grep(
    remote: Remote<Box<dyn AgentDialer>>,
    machine: &str,
    args: &Value,
) -> Result<ToolReply> {
    let pattern = args["pattern"].as_str().unwrap_or("");
    if pattern.is_empty() {
        return Ok(("missing `pattern`".into(), None, true));
    }
    let root = args["path"].as_str().unwrap_or(".");
    let include = args["include"].as_str().filter(|s| !s.trim().is_empty());
    let max_results = args["max_results"].as_u64().unwrap_or(100).clamp(1, 1000) as u32;
    let hits = remote
        .grep(machine, root, pattern, include, max_results)
        .await?;
    let text = if hits.is_empty() {
        "(no matches)".into()
    } else {
        hits.iter()
            .map(|h| format!("{}:{}: {}", h.path, h.line, h.text))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok((text, Some(json!({ "count": hits.len() })), false))
}

pub(crate) async fn job(
    remote: Remote<Box<dyn AgentDialer>>,
    machine: &str,
    args: &Value,
) -> Result<ToolReply> {
    let action = args["action"].as_str().unwrap_or("");
    match action {
        "list" => {
            let jobs = remote.jobs(machine).await?;
            if jobs.is_empty() {
                return Ok((
                    format!("no jobs on {machine}"),
                    Some(json!({ "jobs": [] })),
                    false,
                ));
            }
            let text = jobs.iter().map(fmt_job).collect::<Vec<_>>().join("\n") + "\n";
            Ok((text, Some(json!({ "count": jobs.len() })), false))
        }
        "status" => {
            let id = job_id(args)?;
            let job = remote.status(machine, id).await?;
            let structured = json!({
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
            });
            Ok((
                serde_json::to_string_pretty(&structured)?,
                Some(structured),
                false,
            ))
        }
        "logs" => {
            let id = job_id(args)?;
            let offset = args["offset"].as_u64().unwrap_or(0);
            let mut sink = Sink::new(offset);
            let (_server_cursor, finished) = remote
                .logs(machine, id, offset, false, |records| {
                    sink.push_records(records)
                })
                .await?;
            let mut text = sink.text();
            text.push_str(&truncation_note(&sink));
            text.push_str(&format!("\n\nnext={}", sink.cursor));
            match finished {
                Some(code) => text.push_str(&format!(" exit={code}")),
                None => text.push_str(" (running)"),
            }
            Ok((
                text,
                Some(json!({ "id": id, "next": sink.cursor, "exit": finished })),
                false,
            ))
        }
        "cancel" => {
            let id = job_id(args)?;
            remote.cancel(machine, id).await?;
            Ok((
                format!("cancel requested for job {id} on {machine}"),
                Some(json!({ "id": id, "canceled": true })),
                false,
            ))
        }
        other => Ok((
            format!("unknown action `{other}`; use list, status, logs or cancel"),
            None,
            true,
        )),
    }
}
