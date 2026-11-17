//! Tool registry and dispatch.

use super::asks::AskBroker;
use super::events::AgentEvent;
use super::loop_hooks::PreparedTool;
use super::monitors::MonitorBroker;
use super::subagent::SubagentHost;
use crate::mcp::client::McpManager;
use crate::permissions::PermissionEngine;
use anyhow::anyhow;
use envoy_service::{AgentDialer, Remote};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

mod ask;
mod fs;
mod monitor;
mod schema;
mod search;
mod shell;
mod skill;
mod subagent;
pub(crate) mod todo;
mod web;

pub(crate) use ask::ask_tool;
#[allow(unused_imports)]
pub(crate) use schema::{
    anthropic_tools_json, anthropic_tools_json_filtered, anthropic_tools_json_for,
    openai_tools_json, openai_tools_json_filtered, openai_tools_json_for, responses_tools_json,
    responses_tools_json_filtered, responses_tools_json_for, ToolFilter,
};
use fs::{edit_tool, ls_tool, read_tool, write_tool};
use monitor::{monitor_tool, summarize_monitor};
use search::{glob_tool, grep_tool};
use shell::{exec_tool, job_tool};
use skill::skill_tool;
use subagent::{delegate_tool, summarize_delegate};
use todo::todo_tool;
use web::webfetch_tool;

const TOOL_OUTPUT_CAP: usize = 64 * 1024;

pub(crate) struct ToolExecOutcome {
    pub(crate) text: String,
    pub(crate) ok: bool,
    pub(crate) updated_hook_state: Option<BTreeMap<String, Value>>,
}

fn resolve_path(cwd: &Path, raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "." {
        return cwd.display().to_string();
    }
    if trimmed.starts_with('/') || trimmed.starts_with("~/") || trimmed == "~" {
        return trimmed.to_string();
    }
    cwd.join(trimmed).display().to_string()
}

fn summarize_tool(name: &str, args: &Value) -> (String, String) {
    match name {
        "exec" => {
            let cmd = args["command"].as_str().unwrap_or("");
            (format!("$ {cmd}"), String::new())
        }
        "read" => (
            format!("read {}", args["path"].as_str().unwrap_or("")),
            String::new(),
        ),
        "write" => (
            format!("write {}", args["path"].as_str().unwrap_or("")),
            format!("{}B", args["content"].as_str().map(|s| s.len()).unwrap_or(0)),
        ),
        "edit" => (
            format!("edit {}", args["path"].as_str().unwrap_or("")),
            String::new(),
        ),
        "ls" => (
            format!("ls {}", args["path"].as_str().unwrap_or(".")),
            String::new(),
        ),
        "glob" => (
            format!("glob {}", args["pattern"].as_str().unwrap_or("")),
            args["path"].as_str().unwrap_or(".").to_string(),
        ),
        "grep" => (
            format!("grep {}", args["pattern"].as_str().unwrap_or("")),
            args["include"].as_str().unwrap_or("").to_string(),
        ),
        "webfetch" => (
            format!("fetch {}", args["url"].as_str().unwrap_or("")),
            String::new(),
        ),
        "todo" => (
            format!("todo {}", args["action"].as_str().unwrap_or("read")),
            String::new(),
        ),
        "skill" => (
            format!("skill {}", args["name"].as_str().unwrap_or("")),
            String::new(),
        ),
        "job" => (
            format!(
                "job {} {}",
                args["action"].as_str().unwrap_or(""),
                args["id"].as_u64().map(|id| format!("#{id}")).unwrap_or_default()
            ),
            String::new(),
        ),
        "delegate" => summarize_delegate(args),
        "monitor" => summarize_monitor(args),
        mcp if mcp.starts_with("mcp__") => (
            mcp.trim_start_matches("mcp__").replace("__", ":"),
            String::new(),
        ),
        other => (other.to_string(), String::new()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_prepared_tool_call(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    prepared: &PreparedTool,
    hook_state_snapshot: Option<BTreeMap<String, Value>>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    asks: &Arc<AskBroker>,
    permissions: &Arc<PermissionEngine>,
    mcp: Option<&McpManager>,
    tool_filter: &ToolFilter,
    subagent_host: Option<&SubagentHost>,
    monitors: Option<&Arc<dyn MonitorBroker>>,
) -> ToolExecOutcome {
    let tc = &prepared.call;
    let args = &prepared.effective_args;
    let (title, subtitle) = summarize_tool(&tc.name, args);
    let _ = tx.send(AgentEvent::ToolStart {
        call_id: tc.id.clone(),
        name: tc.name.clone(),
        title,
        subtitle,
    });

    let start = Instant::now();
    let mut updated_hook_state = None;
    let res = if let Some(blocked) = &prepared.blocked_error {
        Err(anyhow!("{blocked}"))
    } else if !tool_filter.allows(&tc.name) {
        Err(anyhow!("tool `{}` is not enabled for this agent", tc.name))
    } else if tc.name == "ask" {
        ask_tool(asks, tx, args).await.map(|s| (s, None))
    } else {
        match permissions
            .authorize_tool_call(&tc.name, args, asks, tx)
            .await
        {
            Err(e) => Err(e),
            Ok(auth) => match tc.name.as_str() {
                "exec" => exec_tool(remote, target, cwd, &tc.id, args, auth.sandbox, tx).await,
                "read" => read_tool(remote, target, cwd, args).await.map(|s| (s, None)),
                "write" => match write_tool(remote, target, cwd, args).await {
                    Ok((msg, snap)) => {
                        if let Some(s) = snap {
                            let (added, removed) =
                                crate::session::compute_diff_counts(&s.before, &s.after);
                            let _ = tx.send(AgentEvent::FileChanged {
                                path: s.path,
                                before: s.before,
                                after: s.after,
                                added,
                                removed,
                                existed_before: s.existed_before,
                                restorable_before: s.restorable_before,
                            });
                        }
                        Ok((msg, None))
                    }
                    Err(e) => Err(e),
                },
                "edit" => match edit_tool(remote, target, cwd, args).await {
                    Ok((msg, snap)) => {
                        if let Some(s) = snap {
                            let (added, removed) =
                                crate::session::compute_diff_counts(&s.before, &s.after);
                            let _ = tx.send(AgentEvent::FileChanged {
                                path: s.path,
                                before: s.before,
                                after: s.after,
                                added,
                                removed,
                                existed_before: s.existed_before,
                                restorable_before: s.restorable_before,
                            });
                        }
                        Ok((msg, None))
                    }
                    Err(e) => Err(e),
                },
                "ls" => ls_tool(remote, target, cwd, args).await.map(|s| (s, None)),
                "glob" => glob_tool(remote, target, cwd, args).await.map(|s| (s, None)),
                "grep" => grep_tool(remote, target, cwd, args).await.map(|s| (s, None)),
                "webfetch" => webfetch_tool(args).await.map(|s| (s, None)),
                "todo" => todo_tool(permissions.data_dir(), permissions.session_id(), args)
                    .map(|s| (s, None)),
                "skill" => skill_tool(permissions.data_dir(), cwd, args).map(|s| (s, None)),
                "job" => job_tool(remote, target, args).await.map(|s| (s, None)),
                "delegate" => {
                    match delegate_tool(
                        subagent_host,
                        &tc.id,
                        args,
                        hook_state_snapshot.unwrap_or_default(),
                        tx,
                    )
                    .await
                    {
                        Ok((msg, exit_code, next_state)) => {
                            updated_hook_state = Some(next_state);
                            Ok((msg, exit_code))
                        }
                        Err(e) => Err(e),
                    }
                }
                "monitor" => {
                    monitor_tool(monitors, remote, target, cwd, args, asks, permissions, tx)
                        .await
                        .map(|s| (s, None))
                }
                "say" => {
                    let text = args["text"].as_str().unwrap_or("").trim().to_string();
                    let kind = args["kind"].as_str().unwrap_or("progress").to_string();
                    if !text.is_empty() {
                        let _ = tx.send(AgentEvent::Say { text, kind });
                    }
                    Ok(("ok".to_string(), None))
                }
                name if name.starts_with("mcp__") => {
                    if let Some(mgr) = mcp {
                        mgr.call_tool(name, args.clone()).await.map(|s| (s, None))
                    } else {
                        Err(anyhow!("MCP client manager not available for `{name}`"))
                    }
                }
                other => Err(anyhow!("unknown tool `{other}`")),
            },
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    match res {
        Ok((text, exit_code)) => {
            let ok = exit_code.map(|c| c == 0).unwrap_or(true);
            if tc.name != "exec" && !text.is_empty() {
                let preview = truncate_tool_preview(&text, 4096);
                let _ = tx.send(AgentEvent::ToolOutput {
                    call_id: tc.id.clone(),
                    text: preview,
                    err: false,
                });
            }
            let _ = tx.send(AgentEvent::ToolDone {
                call_id: tc.id.clone(),
                ok,
                duration_ms,
                exit_code,
            });
            ToolExecOutcome {
                text: truncate_tool_output(&text),
                ok,
                updated_hook_state,
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let _ = tx.send(AgentEvent::ToolOutput {
                call_id: tc.id.clone(),
                text: msg.clone(),
                err: true,
            });
            let _ = tx.send(AgentEvent::ToolDone {
                call_id: tc.id.clone(),
                ok: false,
                duration_ms,
                exit_code: Some(1),
            });
            ToolExecOutcome {
                text: format!("error: {msg}"),
                ok: false,
                updated_hook_state,
            }
        }
    }
}

fn truncate_tool_output(s: &str) -> String {
    super::context::bound_tool_output(s, TOOL_OUTPUT_CAP)
}

fn truncate_tool_preview(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}\n…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_uses_cwd_for_relative() {
        let p = resolve_path(Path::new("/tmp/proj"), "src/main.rs");
        assert_eq!(p, "/tmp/proj/src/main.rs");
        assert_eq!(resolve_path(Path::new("/tmp/proj"), "/etc/hosts"), "/etc/hosts");
    }

}
