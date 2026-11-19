//! Shell and detached-job tools.
use super::resolve_path;
use crate::agent::events::AgentEvent;
use anyhow::{bail, Context, Result};
use envoy_proto::{LogRecord, SandboxPolicy, SubmitRequest, TaskSpec, DEFAULT_GROUP};
use envoy_service::{AgentDialer, Remote};
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Bytes of command output kept at each end of the tool result.
const OUTPUT_SIDE: usize = 128 * 1024;

/// Head-and-tail buffer so a long command cannot pin 16 MiB in the tool result.
struct BoundedOutput {
    head: Vec<u8>,
    tail: Vec<u8>,
    dropped: usize,
}

impl BoundedOutput {
    fn new() -> Self {
        Self {
            head: Vec::new(),
            tail: Vec::new(),
            dropped: 0,
        }
    }

    fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if self.head.len() >= OUTPUT_SIDE {
            self.push_tail(data);
            return;
        }
        let room = OUTPUT_SIDE - self.head.len();
        if data.len() <= room {
            self.head.extend_from_slice(data);
            return;
        }
        self.head.extend_from_slice(&data[..room]);
        self.push_tail(&data[room..]);
    }

    fn push_tail(&mut self, data: &[u8]) {
        self.tail.extend_from_slice(data);
        if self.tail.len() <= OUTPUT_SIDE {
            return;
        }
        let overflow = self.tail.len() - OUTPUT_SIDE;
        self.dropped += overflow;
        self.tail.drain(..overflow);
    }

    fn render(&self) -> String {
        if self.dropped == 0 && self.tail.is_empty() {
            return String::from_utf8_lossy(&self.head).into_owned();
        }
        format!(
            "{}\n[... {} bytes omitted ...]\n{}",
            String::from_utf8_lossy(&self.head),
            self.dropped,
            String::from_utf8_lossy(&self.tail)
        )
    }
}

pub(crate) async fn exec_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    call_id: &str,
    args: &Value,
    sandbox: SandboxPolicy,
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
        sandbox,
    };
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
    let is_local = target.is_empty() || target == "local";
    let in_scope = |job_cwd: &str| -> bool {
        !is_local
            || (!job_cwd.is_empty()
                && crate::permissions::paths::is_path_under_workspace(cwd, Path::new(job_cwd)))
    };
    let check_scope = |id: u64, job_cwd: &str| -> Result<()> {
        if !in_scope(job_cwd) {
            bail!("job #{id} belongs to another workspace ({job_cwd})");
        }
        Ok(())
    };

    let action = args["action"].as_str().context("missing `action`")?;
    match action {
        "list" => {
            let jobs: Vec<_> = remote
                .jobs(target)
                .await?
                .into_iter()
                .filter(|j| in_scope(&j.cwd))
                .collect();
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
            let j = remote.status(target, id).await?;
            check_scope(id, &j.cwd)?;
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
            let j = remote.status(target, id).await?;
            check_scope(id, &j.cwd)?;
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
            let j = remote.status(target, id).await?;
            check_scope(id, &j.cwd)?;
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
            let j = remote.status(target, id).await?;
            check_scope(id, &j.cwd)?;
            remote.cancel(target, id).await?;
            Ok(format!("cancel requested for #{id}"))
        }
        other => bail!("unknown job action `{other}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::BoundedOutput;

    #[test]
    fn bounded_output_keeps_head_and_tail() {
        let mut buf = BoundedOutput::new();
        buf.push(b"hello");
        assert_eq!(buf.render(), "hello");

        let mut big = BoundedOutput::new();
        big.push(&vec![b'a'; super::OUTPUT_SIDE]);
        big.push(&vec![b'b'; super::OUTPUT_SIDE]);
        big.push(b"TAIL");
        let text = big.render();
        assert!(text.starts_with("aaa"), "{text}");
        assert!(text.contains("bytes omitted"), "{text}");
        assert!(text.ends_with("TAIL"), "{text}");
        assert!(text.len() < super::OUTPUT_SIDE * 3);
    }
}
