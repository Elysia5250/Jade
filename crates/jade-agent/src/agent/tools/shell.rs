//! Shell and detached-job tools backed by `crate::tools_core`.
use crate::agent::events::AgentEvent;
use crate::tools_core::{self, BoundedOutput};
use anyhow::{bail, Context, Result};
use jade_proto::{LogRecord, SandboxPolicy};
use jade_service::{AgentDialer, Remote};
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
    sandbox: SandboxPolicy,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<(String, Option<i32>)> {
    let req = tools_core::build_exec_request(args, cwd, sandbox, 120, false)?;
    let timeout_s = req.timeout_s;
    let job = remote.submit(target, req).await?;
    let _ = tx.send(AgentEvent::ToolJob {
        call_id: call_id.to_string(),
        job_id: job.id,
    });

    let mut out_all = BoundedOutput::new();
    let mut err_all = BoundedOutput::new();
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
                        err_all.push(&r.data);
                        batch_err.push_str(&s);
                    } else {
                        out_all.push(&r.data);
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
            let mut text = out_all.render();
            let err = err_all.render();
            if !err.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str("[stderr]\n");
                text.push_str(&err);
            }
            if code != 0 && text.trim().is_empty() {
                if let Ok(done) = remote.status(target, job.id).await {
                    if let Some(result) = done.state.result() {
                        text.push_str(&result.describe());
                        text.push('\n');
                    }
                }
            }
            text.push_str(&format!("\n[job #{} exit={code}]", job.id));
            return Ok((text, Some(code)));
        }
        if Instant::now() >= deadline {
            let text = format!(
                "{}\n[job #{} still running after {}s; use `job` tool with action=logs id={}]",
                out_all.render(),
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
    cwd: &Path,
    args: &Value,
) -> Result<String> {
    let action = args["action"].as_str().context("missing `action`")?;
    match action {
        "list" => {
            let jobs = tools_core::job_list_scoped(remote, target, cwd).await?;
            if jobs.is_empty() {
                return Ok("no jobs".into());
            }
            let mut out = String::new();
            for j in jobs {
                out.push_str(&format!("#{} {} {}\n", j.id, j.state.label(), j.command()));
            }
            Ok(out)
        }
        "status" => {
            let id = args["id"].as_u64().context("missing `id`")?;
            let j = tools_core::job_status_scoped(remote, target, cwd, id).await?;
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
            let _ = tools_core::job_status_scoped(remote, target, cwd, id).await?;
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
            let _ = tools_core::job_status_scoped(remote, target, cwd, id).await?;
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
            let _ = tools_core::job_status_scoped(remote, target, cwd, id).await?;
            remote.cancel(target, id).await?;
            Ok(format!("cancel requested for #{id}"))
        }
        other => bail!("unknown job action `{other}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::BoundedOutput;
    use crate::tools_core::EXEC_OUTPUT_SIDE;

    #[test]
    fn bounded_output_keeps_head_and_tail() {
        let mut buf = BoundedOutput::new();
        buf.push(b"hello");
        assert_eq!(buf.render(), "hello");

        let mut big = BoundedOutput::new();
        big.push(&vec![b'a'; EXEC_OUTPUT_SIDE]);
        big.push(&vec![b'b'; EXEC_OUTPUT_SIDE]);
        big.push(b"TAIL");
        let text = big.render();
        assert!(text.starts_with("aaa"), "{text}");
        assert!(text.contains("bytes omitted"), "{text}");
        assert!(text.ends_with("TAIL"), "{text}");
        assert!(text.len() < EXEC_OUTPUT_SIDE * 3);

        let mut mid = BoundedOutput::new();
        mid.push(&vec![b'a'; EXEC_OUTPUT_SIDE]);
        mid.push(&vec![b'b'; 20_000]);
        let mid_text = mid.render();
        assert!(!mid_text.contains("omitted"), "{mid_text}");
        assert_eq!(mid_text.len(), EXEC_OUTPUT_SIDE + 20_000);
    }
}
