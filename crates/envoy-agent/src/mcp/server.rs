//! `envoy mcp serve`: Model Context Protocol server over stdio.
//!
//! Exposes `exec`, `read`, `write`, `edit`, `ls`, `glob`, `grep`, `job` with
//! MCP annotations + `structuredContent`, plus `resources/list` + `resources/read`
//! for job logs (`envoy://jobs`, `envoy://jobs/<id>/log`).
//!
//! Security boundary:
//! - Enforces `PermissionEngine` rules (`deny` blocks, `ask` refuses cleanly
//!   over non-interactive stdio unless allowed by rules/mode).
//! - `exec` defaults to `SandboxPolicy::WorkspaceWrite { allow_net: false }` on
//!   every target. The peer applies that policy with its own backend.

use super::server_tools::{self, machine_arg, tool_defs, Sink, ToolReply};
use crate::agent::AskBroker;
use crate::daemon;
use crate::permissions::{PermissionEngine, PermissionMode};
use crate::tools_core;
use anyhow::{bail, Result};
use envoy_proto::SandboxPolicy;
use envoy_service::{dialer_for, AgentDialer, Remote};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(super) struct Mcp {
    pub(super) data_dir: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) conduit: Option<PathBuf>,
    pub(super) permissions: Arc<PermissionEngine>,
}

impl Mcp {
    async fn remote(&self, machine: &str) -> Result<Remote<Box<dyn AgentDialer>>> {
        if machine == "local" || machine.is_empty() {
            daemon::ensure(&self.data_dir).await?;
        }
        Ok(Remote::new(dialer_for(
            &self.data_dir,
            machine,
            self.conduit.clone(),
        )))
    }
}

pub async fn serve(
    data_dir: PathBuf,
    conduit: Option<PathBuf>,
    mode: PermissionMode,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| data_dir.clone());
    let permissions = PermissionEngine::new(
        data_dir.clone(),
        cwd.clone(),
        format!("mcp-{}", std::process::id()),
        "local".into(),
        mode,
    );
    let mcp = Mcp {
        data_dir,
        cwd,
        conduit,
        permissions,
    };

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
                reply(
                    &mut out,
                    error(&Value::Null, -32700, &format!("parse error: {e}")),
                )
                .await?;
                continue;
            }
        };
        let Some(resp) = handle_rpc(&mcp, &msg).await? else {
            continue;
        };
        reply(&mut out, resp).await?;
    }
    Ok(())
}

async fn handle_rpc(mcp: &Mcp, msg: &Value) -> Result<Option<Value>> {
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
        "tools/call" => match call_tool(mcp, &msg["params"]).await {
            Ok((text, structured, is_error)) => {
                let mut payload = json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": is_error,
                });
                if let Some(sc) = structured {
                    payload["structuredContent"] = sc;
                }
                response(&id, payload)
            }
            Err(e) => error(&id, -32603, &format!("{e:#}")),
        },
        "resources/list" => match list_resources(mcp).await {
            Ok(res) => response(&id, res),
            Err(e) => error(&id, -32603, &format!("{e:#}")),
        },
        "resources/read" => match read_resource(mcp, &msg["params"]).await {
            Ok(res) => response(&id, res),
            Err(e) => error(&id, -32602, &format!("{e:#}")),
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
    let requested = params["protocolVersion"].as_str().unwrap_or("2025-03-26");
    let supported = [
        "2024-11-05",
        "2025-03-26",
        "2025-06-18",
        "2025-11-25",
        "2026-07-28",
    ];
    let version = if supported.contains(&requested) {
        requested
    } else {
        "2025-03-26"
    };
    json!({
        "protocolVersion": version,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false }
        },
        "serverInfo": {
            "name": "envoy",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn normalize_local_args(name: &str, cwd: &Path, args: &Value) -> Value {
    if machine_arg(args) != "local" {
        return args.clone();
    }
    let mut out = if args.is_object() {
        args.clone()
    } else {
        json!({})
    };
    match name {
        "read" | "write" | "edit" | "ls" => {
            if let Some(raw) = out.get("path").and_then(Value::as_str).map(str::to_string) {
                out["path"] = Value::String(tools_core::resolve_path(cwd, &raw));
            } else if name == "ls" {
                out["path"] = Value::String(cwd.display().to_string());
            }
        }
        "glob" | "grep" => {
            let raw = out
                .get("path")
                .or_else(|| out.get("root"))
                .and_then(Value::as_str)
                .unwrap_or(".");
            out["path"] = Value::String(tools_core::resolve_path(cwd, raw));
        }
        "exec" => {
            let raw_cwd = out.get("cwd").and_then(Value::as_str).unwrap_or(".");
            out["cwd"] = Value::String(tools_core::resolve_path(cwd, raw_cwd));
        }
        _ => {}
    }
    out
}

async fn authorize_mcp_tool(
    mcp: &Mcp,
    name: &str,
    args: &Value,
) -> std::result::Result<SandboxPolicy, String> {
    let asks = AskBroker::new_headless();
    let (sink_tx, _sink_rx) = tokio::sync::mpsc::unbounded_channel();
    match mcp
        .permissions
        .authorize_tool_call(name, args, &asks, &sink_tx)
        .await
    {
        Ok(auth) => Ok(match auth.sandbox {
            SandboxPolicy::WorkspaceWrite { cwd, extra_rw, .. } => SandboxPolicy::WorkspaceWrite {
                cwd,
                extra_rw,
                allow_net: false,
            },
            SandboxPolicy::ReadOnly { cwd, .. } => SandboxPolicy::ReadOnly {
                cwd,
                allow_net: false,
            },
            other => other,
        }),
        Err(err) => {
            let msg = format!("{err:#}");
            if msg.contains("requires interactive approval") {
                Err(format!(
                    "permission requires approval over non-interactive MCP stdio ({msg})"
                ))
            } else {
                Err(msg)
            }
        }
    }
}

pub(super) async fn call_tool(mcp: &Mcp, params: &Value) -> Result<ToolReply> {
    let name = params["name"].as_str().unwrap_or("");
    let args = &params["arguments"];
    if !matches!(
        name,
        "exec" | "read" | "write" | "edit" | "ls" | "glob" | "grep" | "job"
    ) {
        return Ok((
            format!(
                "unknown tool `{name}`; available: exec, read, write, edit, ls, glob, grep, job"
            ),
            None,
            true,
        ));
    }

    let machine = machine_arg(args);
    if machine != "local" {
        let mode = mcp.permissions.mode();
        if mode == PermissionMode::Plan
            && !matches!(name, "read" | "ls" | "glob" | "grep")
            && !(name == "job" && args["action"].as_str() != Some("cancel"))
        {
            return Ok((
                "permission denied: plan mode forbids remote mutations".into(),
                None,
                true,
            ));
        }
        if !matches!(mode, PermissionMode::Auto | PermissionMode::Turbo) {
            return Ok((
                format!(
                    "permission requires approval over non-interactive MCP stdio (remote machine target `{machine}`)"
                ),
                None,
                true,
            ));
        }
    }

    let norm_args = normalize_local_args(name, &mcp.cwd, args);
    if name == "exec" {
        if let Err(e) = tools_core::validate_env_arg(&norm_args) {
            return Ok((format!("{e:#}"), None, true));
        }
    }

    let sandbox = match authorize_mcp_tool(mcp, name, &norm_args).await {
        Ok(s) => s,
        Err(msg) => return Ok((msg, None, true)),
    };

    let remote = mcp.remote(&machine).await?;
    match name {
        "exec" => server_tools::exec(remote, &machine, &mcp.cwd, sandbox, &norm_args).await,
        "read" => server_tools::read(remote, &machine, &norm_args).await,
        "write" => server_tools::write(remote, &machine, &norm_args).await,
        "edit" => server_tools::edit(remote, &machine, &norm_args).await,
        "ls" => server_tools::ls(remote, &machine, &norm_args).await,
        "glob" => server_tools::glob(remote, &machine, &norm_args).await,
        "grep" => server_tools::grep(remote, &machine, &norm_args).await,
        "job" => server_tools::job(remote, &machine, &mcp.cwd, &norm_args).await,
        _ => unreachable!(),
    }
}

async fn list_resources(mcp: &Mcp) -> Result<Value> {
    if let Err(msg) = authorize_mcp_tool(mcp, "job", &json!({ "action": "list" })).await {
        bail!("{msg}");
    }
    let remote = mcp.remote("local").await?;
    let jobs = tools_core::job_list_scoped(&remote, "local", &mcp.cwd)
        .await
        .unwrap_or_default();
    let mut resources = vec![json!({
        "uri": "envoy://jobs",
        "name": "Local Jobs",
        "description": "List of detached jobs managed by the local envoy daemon.",
        "mimeType": "application/json",
    })];
    for j in jobs.into_iter().take(50) {
        resources.push(json!({
            "uri": format!("envoy://jobs/{}/log", j.id),
            "name": format!("Job #{} Log", j.id),
            "description": j.command(),
            "mimeType": "text/plain",
        }));
    }
    Ok(json!({ "resources": resources }))
}

async fn read_resource(mcp: &Mcp, params: &Value) -> Result<Value> {
    let uri = params["uri"].as_str().unwrap_or("");
    let action = if uri == "envoy://jobs" {
        "list"
    } else {
        "logs"
    };
    if let Err(msg) = authorize_mcp_tool(mcp, "job", &json!({ "action": action })).await {
        bail!("{msg}");
    }
    let remote = mcp.remote("local").await?;
    if uri == "envoy://jobs" {
        let jobs = tools_core::job_list_scoped(&remote, "local", &mcp.cwd).await?;
        let json_list: Vec<Value> = jobs
            .into_iter()
            .map(|j| {
                json!({
                    "id": j.id,
                    "state": j.state.label(),
                    "command": j.command(),
                    "cwd": j.cwd,
                    "log_bytes": j.log_bytes,
                })
            })
            .collect();
        return Ok(json!({
            "contents": [{
                "uri": uri,
                "mimeType": "application/json",
                "text": serde_json::to_string_pretty(&json_list)?,
            }]
        }));
    }
    if let Some(rest) = uri.strip_prefix("envoy://jobs/") {
        if let Some(id_str) = rest.strip_suffix("/log") {
            let id: u64 = id_str.parse()?;
            let _ = tools_core::job_status_scoped(&remote, "local", &mcp.cwd, id).await?;
            let mut sink = Sink::new(0);
            let _ = remote
                .logs("local", id, 0, false, |recs| sink.push_records(recs))
                .await?;
            return Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": sink.text(),
                }]
            }));
        }
    }
    bail!("unknown MCP resource URI `{uri}`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use envoy_proto::LogRecord;

    #[test]
    fn eight_tools_have_annotations_and_glob_grep() {
        let tools = tool_defs();
        let arr = tools.as_array().unwrap();
        let names: Vec<&str> = arr.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec!["exec", "read", "write", "edit", "ls", "glob", "grep", "job"]
        );
        for t in arr {
            assert!(
                t["annotations"].is_object(),
                "missing annotations on {}",
                t["name"]
            );
        }
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
        assert_eq!(sink.cursor, 10 + server_tools::RECORD_HEADER + 3);
        assert!(!sink.truncated);
        assert!(sink.text().contains("[stderr]"));
    }

    #[tokio::test]
    async fn initialize_negotiates_version_and_permission_gate_blocks_unapproved_write() {
        let init = initialize(&json!({ "protocolVersion": "2026-07-28" }));
        assert_eq!(init["protocolVersion"], "2026-07-28");
        assert!(init["capabilities"]["resources"].is_object());

        let dir = std::env::temp_dir().join(format!("envoy-mcp-srv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mcp = Mcp {
            data_dir: dir.clone(),
            cwd: dir.clone(),
            conduit: None,
            permissions: PermissionEngine::new(
                dir.clone(),
                dir.clone(),
                "mcp-test".into(),
                "local".into(),
                PermissionMode::Default,
            ),
        };
        let (text, _sc, is_err) = call_tool(
            &mcp,
            &json!({ "name": "write", "arguments": { "path": "foo.txt", "content": "hi" } }),
        )
        .await
        .unwrap();
        assert!(is_err);
        assert!(text.contains("permission requires approval"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
