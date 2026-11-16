//! Tool registry and dispatch.

use super::asks::AskBroker;
use super::events::AgentEvent;
use crate::session::ToolCall;
use anyhow::anyhow;
use envoy_service::{AgentDialer, Remote};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

mod ask;
mod fs;
mod schema;
mod shell;

pub(crate) use ask::ask_tool;
pub(crate) use schema::{anthropic_tools_json, openai_tools_json, responses_tools_json};
use fs::{edit_tool, ls_tool, read_tool, write_tool};
use shell::{exec_tool, job_tool};

const TOOL_OUTPUT_CAP: usize = 64 * 1024;

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
        "job" => (
            format!(
                "job {} {}",
                args["action"].as_str().unwrap_or(""),
                args["id"].as_u64().map(|id| format!("#{id}")).unwrap_or_default()
            ),
            String::new(),
        ),
        other => (other.to_string(), String::new()),
    }
}

pub(crate) async fn run_tool_call(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    tc: &ToolCall,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    asks: &Arc<AskBroker>,
) -> String {
    let args: Value = serde_json::from_str(&tc.arguments).unwrap_or_else(|_| json!({}));
    let (title, subtitle) = summarize_tool(&tc.name, &args);
    let _ = tx.send(AgentEvent::ToolStart {
        call_id: tc.id.clone(),
        name: tc.name.clone(),
        title,
        subtitle,
    });

    let start = Instant::now();
    let res = match tc.name.as_str() {
        "exec" => exec_tool(remote, target, cwd, &tc.id, &args, tx).await,
        "read" => read_tool(remote, target, cwd, &args).await.map(|s| (s, None)),
        "write" => write_tool(remote, target, cwd, &args).await.map(|s| (s, None)),
        "edit" => edit_tool(remote, target, cwd, &args).await.map(|s| (s, None)),
        "ls" => ls_tool(remote, target, cwd, &args).await.map(|s| (s, None)),
        "job" => job_tool(remote, target, &args).await.map(|s| (s, None)),
        "say" => {
            let text = args["text"].as_str().unwrap_or("").trim().to_string();
            let kind = args["kind"].as_str().unwrap_or("progress").to_string();
            if !text.is_empty() {
                let _ = tx.send(AgentEvent::Say { text, kind });
            }
            Ok(("ok".to_string(), None))
        }
        "ask" => ask_tool(asks, tx, &args).await.map(|s| (s, None)),
        other => Err(anyhow!("unknown tool `{other}`")),
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
            truncate_tool_output(&text)
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
            format!("error: {msg}")
        }
    }
}

fn truncate_tool_output(s: &str) -> String {
    if s.len() <= TOOL_OUTPUT_CAP {
        return s.to_string();
    }
    let mut end = TOOL_OUTPUT_CAP;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}\n[truncated]", &s[..end])
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
