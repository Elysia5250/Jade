//! Interactive permission approval prompting via [`AskBroker`].

use super::types::Decision;
use crate::agent::{AgentEvent, AskBroker, AskOption};
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ApprovalChoice {
    OnceOffline,
    OnceWithNet,
    OnceExpanded,
    OnceOutsideSandbox,
    Session,
    Project,
    Deny,
}

pub(super) async fn prompt_user_approval(
    tool: &str,
    detail: &str,
    decision: &Decision,
    asks: &Arc<AskBroker>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<ApprovalChoice> {
    let mut options = Vec::new();
    if tool == "exec" && decision.needs_net {
        options.push(AskOption {
            label: "Allow once (with network)".into(),
            description: Some(
                "Run in workspace sandbox with network enabled for this command".into(),
            ),
            recommended: false,
        });
        options.push(AskOption {
            label: "Allow once (offline)".into(),
            description: Some("Run inside default network-disabled sandbox".into()),
            recommended: false,
        });
    } else if tool == "exec" && decision.outside_workspace {
        options.push(AskOption {
            label: "Allow once (with expanded path)".into(),
            description: Some("Run inside sandbox with the referenced path bound".into()),
            recommended: false,
        });
        options.push(AskOption {
            label: "Allow once (unsandboxed)".into(),
            description: Some("Run without OS sandbox restrictions for this command only".into()),
            recommended: false,
        });
    } else {
        options.push(AskOption {
            label: "Allow once".into(),
            description: Some("Approve this single tool invocation".into()),
            recommended: false,
        });
    }
    if let Some(rule) = &decision.suggested_rule {
        options.push(AskOption {
            label: "Allow for session".into(),
            description: Some(format!("Session allow: {rule}")),
            recommended: false,
        });
        options.push(AskOption {
            label: "Always allow (project)".into(),
            description: Some(format!("Save to .envoy/permissions.json: {rule}")),
            recommended: false,
        });
    }
    let deny_idx = options.len();
    options.push(AskOption {
        label: "Deny".into(),
        description: Some("Refuse this operation".into()),
        recommended: true,
    });

    let question = format!(
        "Permission required for `{tool}` ({}):\n  {detail}",
        decision.reason
    );
    let timeout_s = 120u64;
    let _gate = asks.acquire_prompt().await;
    let id = asks.next_id();
    let (send, recv) = oneshot::channel();
    asks.register(id, send);
    let _ = tx.send(AgentEvent::Ask {
        id,
        question,
        options: options.clone(),
        recommended: deny_idx,
        timeout_ms: timeout_s * 1000,
    });

    let answer = tokio::time::timeout(Duration::from_secs(timeout_s), recv).await;
    asks.forget(id);
    let chosen = match answer {
        Ok(Ok(label)) => label,
        _ => {
            let _ = tx.send(AgentEvent::AskClosed { id });
            return Ok(ApprovalChoice::Deny);
        }
    };

    Ok(match chosen.as_str() {
        "Allow once (with network)" => ApprovalChoice::OnceWithNet,
        "Allow once (with expanded path)" => ApprovalChoice::OnceExpanded,
        "Allow once (unsandboxed)" => ApprovalChoice::OnceOutsideSandbox,
        "Allow once" | "Allow once (offline)" => ApprovalChoice::OnceOffline,
        "Allow for session" => ApprovalChoice::Session,
        "Always allow (project)" => ApprovalChoice::Project,
        _ => ApprovalChoice::Deny,
    })
}

pub(super) fn summarize_call(tool: &str, args: &Value) -> String {
    match tool {
        "exec" => args["command"].as_str().unwrap_or("").to_string(),
        "read" | "write" | "edit" | "ls" => args["path"].as_str().unwrap_or(".").to_string(),
        "glob" | "grep" => format!(
            "{} ({})",
            args["pattern"].as_str().unwrap_or("*"),
            args["path"]
                .as_str()
                .or_else(|| args["root"].as_str())
                .unwrap_or(".")
        ),
        "webfetch" => args["url"].as_str().unwrap_or("").to_string(),
        "delegate" => args["agent"]
            .as_str()
            .map(|a| format!("delegate {a}"))
            .unwrap_or_else(|| "delegate batch".into()),
        "monitor" => format!("monitor {}", args["action"].as_str().unwrap_or("list")),
        other => other.to_string(),
    }
}
