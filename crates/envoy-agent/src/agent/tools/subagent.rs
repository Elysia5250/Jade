//! `delegate` tool argument parsing and subagent invocation.

use crate::agent::events::AgentEvent;
use crate::agent::subagent::runner::{execute_delegate, SubagentHost, SubagentTaskInput};
use anyhow::{bail, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::sync::mpsc;

pub(crate) async fn delegate_tool(
    host: Option<&SubagentHost>,
    call_id: &str,
    args: &Value,
    hook_state: BTreeMap<String, Value>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<(String, Option<i32>, BTreeMap<String, Value>)> {
    let Some(host) = host else {
        bail!("nested subagent delegation is disabled (max depth 1)");
    };

    let tasks = parse_tasks(args)?;
    let (summary, all_ok, next_state) =
        execute_delegate(host, call_id, tasks, hook_state, tx).await?;
    let exit_code = if all_ok { None } else { Some(1) };
    Ok((summary, exit_code, next_state))
}

pub(crate) fn summarize_delegate(args: &Value) -> (String, String) {
    if let Some(arr) = args["tasks"].as_array() {
        let agents: Vec<&str> = arr
            .iter()
            .filter_map(|t| t["agent"].as_str())
            .take(3)
            .collect();
        let subtitle = if agents.is_empty() {
            format!("{} tasks", arr.len())
        } else if arr.len() > 3 {
            format!("{} tasks ({}, …)", arr.len(), agents.join(", "))
        } else {
            format!("{} tasks ({})", arr.len(), agents.join(", "))
        };
        return ("delegate batch".into(), subtitle);
    }

    let agent = args["agent"].as_str().unwrap_or("explore");
    let desc = args["description"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    (format!("delegate {agent}"), desc)
}

fn parse_tasks(args: &Value) -> Result<Vec<SubagentTaskInput>> {
    if let Some(arr) = args["tasks"].as_array() {
        let mut out = Vec::with_capacity(arr.len());
        for (i, item) in arr.iter().enumerate() {
            let agent = item["agent"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("tasks[{i}] is missing `agent`"))?;
            let prompt = item["prompt"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("tasks[{i}] is missing `prompt`"))?;
            out.push(SubagentTaskInput {
                agent: agent.to_string(),
                prompt: prompt.to_string(),
                description: item["description"].as_str().map(str::to_string),
                model_override: item["model"].as_str().map(str::to_string),
            });
        }
        return Ok(out);
    }

    let agent = args["agent"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing `agent` (or `tasks` array)"))?;
    let prompt = args["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing `prompt` (or `tasks` array)"))?;

    Ok(vec![SubagentTaskInput {
        agent: agent.to_string(),
        prompt: prompt.to_string(),
        description: args["description"].as_str().map(str::to_string),
        model_override: args["model"].as_str().map(str::to_string),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_single_and_batch_delegate_calls() {
        let single = parse_tasks(&json!({
            "agent": "explore",
            "prompt": "Find Auth",
            "description": "find auth"
        }))
        .unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].agent, "explore");

        let batch = parse_tasks(&json!({
            "tasks": [
                { "agent": "explore", "prompt": "A" },
                { "agent": "review", "prompt": "B", "model": "fast" }
            ]
        }))
        .unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[1].model_override.as_deref(), Some("fast"));

        let (title, sub) = summarize_delegate(&json!({
            "tasks": [
                { "agent": "explore", "prompt": "A" },
                { "agent": "review", "prompt": "B" }
            ]
        }));
        assert_eq!(title, "delegate batch");
        assert_eq!(sub, "2 tasks (explore, review)");
    }
}
