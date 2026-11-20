//! Background log monitor tool (`start`, `list`, `stop`).

use crate::agent::asks::AskBroker;
use crate::agent::events::AgentEvent;
use crate::agent::monitors::{MonitorBroker, MonitorStartSpec};
use crate::permissions::PermissionEngine;
use crate::tools_core::resolve_path;
use anyhow::{bail, Context, Result};
use envoy_proto::{SubmitRequest, TaskSpec, DEFAULT_GROUP};
use envoy_service::{AgentDialer, Remote};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

pub(crate) fn summarize_monitor(args: &Value) -> (String, String) {
    let action = args["action"].as_str().unwrap_or("list");
    match action {
        "start" => {
            let label = args["label"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| args["command"].as_str())
                .map(|s| s.trim().chars().take(40).collect::<String>())
                .or_else(|| args["job_id"].as_u64().map(|id| format!("job #{id}")))
                .unwrap_or_else(|| "monitor".into());
            let pattern = args["pattern"].as_str().unwrap_or("*");
            (format!("monitor start {label}"), pattern.to_string())
        }
        "stop" => {
            let id = args["id"]
                .as_u64()
                .map(|n| format!("#{n}"))
                .unwrap_or_default();
            (format!("monitor stop {id}"), String::new())
        }
        _ => ("monitor list".into(), String::new()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn monitor_tool(
    broker: Option<&Arc<dyn MonitorBroker>>,
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
    asks: &Arc<AskBroker>,
    permissions: &Arc<PermissionEngine>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<String> {
    let broker = broker.context("background monitors are not available in this context")?;
    let action = args["action"].as_str().context("missing `action`")?;

    match action {
        "start" => {
            let pattern = args["pattern"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .context("missing non-empty `pattern`")?
                .to_string();

            let cmd_opt = args["command"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let job_opt = args["job_id"].as_u64();

            if cmd_opt.is_some() && job_opt.is_some() {
                bail!("provide either `command` or `job_id`, not both");
            }
            if cmd_opt.is_none() && job_opt.is_none() {
                bail!("`monitor` action=start requires either `command` or `job_id`");
            }

            let (job_id, command, owns_job) = if let Some(cmd) = cmd_opt {
                // Re-use full `exec` permission + OS sandbox evaluation for spawned shell monitors.
                let mut exec_args = json!({ "command": cmd });
                if let Some(raw_cwd) = args["cwd"].as_str() {
                    exec_args["cwd"] = Value::String(raw_cwd.to_string());
                }
                let exec_auth = permissions
                    .authorize_tool_call("exec", &exec_args, asks, tx)
                    .await?;
                let job_cwd = args["cwd"]
                    .as_str()
                    .map(|s| resolve_path(cwd, s))
                    .unwrap_or_else(|| cwd.display().to_string());
                let timeout_s = args["timeout_s"].as_u64().unwrap_or(3600).clamp(10, 86_400);
                let req = SubmitRequest {
                    spec: TaskSpec::Shell {
                        command: cmd.to_string(),
                    },
                    cwd: Some(job_cwd),
                    env: Vec::new(),
                    label: args["label"].as_str().map(|s| s.trim().to_string()),
                    group: DEFAULT_GROUP.to_string(),
                    timeout_s,
                    sandbox: exec_auth.sandbox,
                };
                let job = remote.submit(target, req).await?;
                (job.id, cmd.to_string(), true)
            } else {
                let id = job_opt.unwrap();
                let job = remote
                    .status(target, id)
                    .await
                    .with_context(|| format!("looking up job #{id}"))?;
                (id, job.command().to_string(), false)
            };

            let label = args["label"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .unwrap_or_else(|| {
                    let preview: String = command.chars().take(36).collect();
                    if command.chars().count() > 36 {
                        format!("{preview}…")
                    } else {
                        preview
                    }
                });
            let debounce_ms = args["debounce_ms"]
                .as_u64()
                .unwrap_or(500)
                .clamp(100, 30_000);
            let max_triggers = args["max_triggers"].as_u64().and_then(|n| {
                if n == 0 {
                    None
                } else {
                    Some(n.min(u32::MAX as u64) as u32)
                }
            });

            let info = match broker.start(MonitorStartSpec {
                label,
                job_id,
                command,
                pattern,
                debounce_ms,
                max_triggers,
                owns_job,
                target: target.to_string(),
            }) {
                Ok(info) => info,
                Err(e) => {
                    if owns_job {
                        let _ = remote.cancel(target, job_id).await;
                    }
                    return Err(e);
                }
            };

            Ok(format!(
                "started monitor #{} `{}` watching job #{} for `{}` (debounce={}ms, max_triggers={})",
                info.id,
                info.label,
                info.job_id,
                info.pattern,
                debounce_ms,
                info.max_triggers
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unlimited".into())
            ))
        }
        "list" => {
            let list = broker.list();
            if list.is_empty() {
                return Ok("no monitors in this session".into());
            }
            let mut out = String::new();
            for m in list {
                let state = if m.active { "active" } else { "stopped" };
                let cap = m.max_triggers.map(|n| format!("/{n}")).unwrap_or_default();
                out.push_str(&format!(
                    "#{} [{}] `{}` job=#{} pattern=`{}` triggers={}{} cmd=`{}`\n",
                    m.id, state, m.label, m.job_id, m.pattern, m.trigger_count, cap, m.command
                ));
            }
            Ok(out)
        }
        "stop" => {
            let id = args["id"].as_u64().context("missing `id`")?;
            match broker.stop(id).await? {
                Some(info) => Ok(format!(
                    "stopped monitor #{} `{}` (job #{}, triggers={})",
                    info.id, info.label, info.job_id, info.trigger_count
                )),
                None => bail!("no monitor #{id} found in this session"),
            }
        }
        other => bail!("unknown monitor action `{other}` (expected start, list, stop)"),
    }
}
