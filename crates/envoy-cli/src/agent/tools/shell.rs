//! Shell and detached-job tools.
use super::resolve_path;
use crate::agent::events::AgentEvent;
use anyhow::{bail, Context, Result};
use envoy_proto::{LogRecord, SubmitRequest, TaskSpec, DEFAULT_GROUP};
use envoy_service::{AgentDialer, Remote};
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub(crate) async fn exec_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    call_id: &str,
    args: &Value,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<(String, Option<i32>)> {
    let command = args["command"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .context("missing `command`")?
        .to_string();
    let job_cwd = args["cwd"]
        .as_str()
        .map(|s| resolve_path(cwd, s))
        .unwrap_or_else(|| cwd.display().to_string());
    let timeout_s = args["timeout_s"].as_u64().unwrap_or(120).min(3600);

    let req = SubmitRequest {
        spec: TaskSpec::Shell { command },
        cwd: Some(job_cwd),
        env: Vec::new(),
        label: None,
        group: DEFAULT_GROUP.to_string(),
        timeout_s,
    };
    let job = remote.submit(target, req).await?;
    let _ = tx.send(AgentEvent::ToolJob {
        call_id: call_id.to_string(),
        job_id: job.id,
    });

    let mut out_all = Vec::new();
    let mut err_all = Vec::new();
    let mut cursor = 0u64;
    let deadline = Instant::now() + Duration::from_secs(timeout_s.max(5));

    loop {
        let mut batch_out = String::new();
        let mut batch_err = String::new();
        let (next, finished) = remote
            .logs(target, job.id, cursor, false, |records: &[LogRecord]| {
                for r in records {
                    let s = String::from_utf8_lossy(&r.data);
                    if r.fd == 1 {
                        err_all.extend_from_slice(&r.data);
                        batch_err.push_str(&s);
                    } else {
                        out_all.extend_from_slice(&r.data);
                        batch_out.push_str(&s);
                    }
                }
            })
            .await?;
        cursor = next;
        if !batch_out.is_empty() {
            let _ = tx.send(AgentEvent::ToolOutput {
                call_id: call_id.to_string(),
                text: batch_out,
                err: false,
            });
        }
        if !batch_err.is_empty() {
            let _ = tx.send(AgentEvent::ToolOutput {
                call_id: call_id.to_string(),
                text: batch_err,
                err: true,
            });
        }
        if let Some(code) = finished {
            let mut text = String::from_utf8_lossy(&out_all).to_string();
            if !err_all.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str("[stderr]\n");
                text.push_str(&String::from_utf8_lossy(&err_all));
            }
            text.push_str(&format!("\n[job #{} exit={code}]", job.id));
            return Ok((text, Some(code)));
        }
        if Instant::now() >= deadline {
            let text = format!(
                "{}\n[job #{} still running after {}s; use `job` tool with action=logs id={}]",
                String::from_utf8_lossy(&out_all),
                job.id,
                timeout_s,
                job.id
            );
            return Ok((text, None));
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

pub(crate) async fn job_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    args: &Value,
) -> Result<String> {
    let action = args["action"].as_str().context("missing `action`")?;
    match action {
        "list" => {
            let jobs = remote.jobs(target).await?;
            if jobs.is_empty() {
                return Ok("no jobs".into());
            }
            let mut out = String::new();
            for j in jobs {
                out.push_str(&format!(
                    "#{} {} {}\n",
                    j.id,
                    j.state.label(),
                    j.command()
                ));
            }
            Ok(out)
        }
        "status" => {
            let id = args["id"].as_u64().context("missing `id`")?;
            let j = remote.status(target, id).await?;
            Ok(format!(
                "#{} {} cwd={} cmd={}",
                j.id,
                j.state.label(),
                j.cwd,
                j.command()
            ))
        }
        "logs" => {
            let id = args["id"].as_u64().context("missing `id`")?;
            let offset = args["offset"].as_u64().unwrap_or(0);
            let mut text = String::new();
            let (next, finished) = remote
                .logs(target, id, offset, false, |recs| {
                    for r in recs {
                        text.push_str(&String::from_utf8_lossy(&r.data));
                    }
                })
                .await?;
            text.push_str(&format!("\n[next={next} finished={finished:?}]"));
            Ok(text)
        }
        "wait" => {
            let id = args["id"].as_u64().context("missing `id`")?;
            let timeout_s = args["timeout_s"].as_u64().unwrap_or(300).clamp(5, 3600);
            let deadline = Instant::now() + Duration::from_secs(timeout_s);
            let mut offset = 0u64;
            let mut text = String::new();
            loop {
                let (next, finished) = remote
                    .logs(target, id, offset, false, |recs| {
                        for r in recs {
                            text.push_str(&String::from_utf8_lossy(&r.data));
                        }
                    })
                    .await?;
                offset = next;
                if let Some(code) = finished {
                    return Ok(format!("{text}\n[job #{id} exit {code}]"));
                }
                if Instant::now() >= deadline {
                    return Ok(format!(
                        "{text}\n[job #{id} still running after {timeout_s}s; use job wait again]"
                    ));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        "cancel" => {
            let id = args["id"].as_u64().context("missing `id`")?;
            remote.cancel(target, id).await?;
            Ok(format!("cancel requested for #{id}"))
        }
        other => bail!("unknown job action `{other}`"),
    }
}
