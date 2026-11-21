//! Plan sidechat turn. It reads the worker's context and may write only a plan file.
use super::prompt::build_system_prompt;
use super::{execute_loop, LoopOptions, LoopOutcome, TurnConfig};
use crate::permissions::paths::{is_plan_file, resolve_path_pair};
use crate::session::{ChatMessage, ChatRole};
use anyhow::Result;
use std::path::Path;
use tokio::sync::mpsc;

use super::events::AgentEvent;

/// Copy a worker history into a prompt the side model can send.
///
/// A trailing assistant message that still has tool calls is dropped. Those
/// calls have no results yet, and a provider rejects that shape.
pub(crate) fn snapshot_for_side(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out = messages.to_vec();
    crate::session::strip_prior_turn_reasoning(&mut out);
    if let Some(last) = out.last() {
        if last.role == ChatRole::Assistant && !last.tool_calls.is_empty() {
            out.pop();
        }
    }
    out
}

pub(crate) fn is_side_plan_path(data_dir: &Path, cwd: &Path, path: &str) -> bool {
    let resolved = resolve_path_pair(cwd, path);
    is_plan_file(data_dir, cwd, &resolved)
}

pub(crate) fn side_tools() -> Vec<String> {
    [
        "exec", "read", "write", "edit", "ls", "glob", "grep", "webfetch", "skill", "say", "ask",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

pub(crate) fn side_system(target: &str, cwd: &Path) -> String {
    let mut prompt = build_system_prompt(target, cwd);
    prompt.push_str(
        "\n\n## Plan sidechat\n\
         You are the plan sidechat beside a worker that is still running.\n\
         The workspace may still change. Do not continue the worker's unfinished tool calls.\n\
         You are in plan mode: read, discuss, and write the plan only to `.jade/plans/*.md`.\n\
         Do not call `delegate`, `job`, `todo`, or `monitor`. \
         When the plan is ready, write that markdown file and stop.\n\
         User-visible replies use the Steps shell. Choices go only in `ask`.\n",
    );
    prompt
}

pub(crate) async fn run_side_turn(
    cfg: TurnConfig,
    tx: mpsc::UnboundedSender<AgentEvent>,
) -> Result<LoopOutcome> {
    let system = side_system(&cfg.target, &cfg.cwd);
    let opts = LoopOptions {
        system,
        max_steps: cfg.max_steps.unwrap_or(40).clamp(1, 40),
        depth: 0,
        allowed_tools: Some(side_tools()),
        connect_mcp: false,
        subagent_host: None,
        subagent_name: None,
    };
    execute_loop(cfg, opts, &tx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ToolCall;
    fn msg(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage::text(role, content)
    }

    #[test]
    fn snapshot_drops_an_unresolved_tool_call() {
        let mut pending = msg(ChatRole::Assistant, "");
        pending.tool_calls.push(ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        });
        let messages = vec![msg(ChatRole::User, "go"), pending];
        let snap = snapshot_for_side(&messages);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].content, "go");
    }

    #[test]
    fn snapshot_keeps_a_finished_reply() {
        let messages = vec![msg(ChatRole::User, "go"), msg(ChatRole::Assistant, "done")];
        assert_eq!(snapshot_for_side(&messages).len(), 2);
    }

    #[test]
    fn side_tools_leave_out_worker_only_tools() {
        let tools = side_tools();
        for banned in ["delegate", "job", "todo", "monitor"] {
            assert!(!tools.iter().any(|tool| tool == banned), "{banned}");
        }
    }

    #[test]
    fn plan_paths_are_only_plan_markdown() {
        let dir = std::env::temp_dir().join(format!("jade-side-plan-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join(".jade/plans"));
        assert!(is_side_plan_path(&dir, &dir, ".jade/plans/next.md"));
        assert!(!is_side_plan_path(&dir, &dir, "src/lib.rs"));
        assert!(!is_side_plan_path(&dir, &dir, ".jade/plans/notes.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
