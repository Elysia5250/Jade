//! Subagent execution runtime: isolated context, depth=1 guard, parallel read-only batches,
//! timeout + cancellation propagation, and concise progress forwarding.

use super::catalog::{resolve_agent, SubagentSpec};
use crate::agent::asks::AskBroker;
use crate::agent::events::AgentEvent;
use crate::auth::{ModelRef, StoredProvider};
use crate::hooks::{
    confirm_hook_ask, emit_hook_event, HookContext, HookDecision, HookEngine, HookEvent,
};
use crate::permissions::PermissionEngine;
use anyhow::{bail, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

mod forward;
mod isolate;
mod run;

const MAX_BATCH_TASKS: usize = 8;

#[derive(Clone)]
pub(crate) struct SubagentHost {
    pub(crate) data_dir: PathBuf,
    pub(crate) conduit: Option<PathBuf>,
    pub(crate) local_socket: Option<PathBuf>,
    pub(crate) local_server: Option<jade_service::AgentServer>,
    pub(crate) target: String,
    pub(crate) cwd: PathBuf,
    pub(crate) provider: StoredProvider,
    pub(crate) model: ModelRef,
    pub(crate) asks: Arc<AskBroker>,
    pub(crate) permissions: Arc<PermissionEngine>,
    pub(crate) mcp_pool: Option<Arc<crate::mcp::pool::McpPool>>,
    pub(crate) tools: Option<Arc<crate::agent::tools::ToolRegistry>>,
    pub(crate) providers: Option<Arc<crate::agent::providers::ProviderRegistry>>,
    pub(crate) hooks: Option<Arc<HookEngine>>,
    pub(crate) turn_seq: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentTaskInput {
    pub(crate) agent: String,
    pub(crate) prompt: String,
    pub(crate) description: Option<String>,
    pub(crate) model_override: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentTaskReport {
    pub(crate) index: usize,
    pub(crate) agent: String,
    pub(crate) description: String,
    pub(crate) model: String,
    pub(crate) ok: bool,
    pub(crate) duration_ms: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) summary: String,
}

struct PreparedTask {
    index: usize,
    input: SubagentTaskInput,
    spec: SubagentSpec,
    blocked_reason: Option<String>,
}

pub(crate) type DelegateFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<(String, bool, BTreeMap<String, Value>, u64, u64)>>
            + Send
            + 'a,
    >,
>;

pub(crate) fn execute_delegate<'a>(
    host: &'a SubagentHost,
    parent_call_id: &'a str,
    tasks: Vec<SubagentTaskInput>,
    mut hook_state: BTreeMap<String, Value>,
    parent_tx: &'a mpsc::UnboundedSender<AgentEvent>,
) -> DelegateFuture<'a> {
    Box::pin(async move {
        if tasks.is_empty() {
            bail!("`delegate` requires either `agent` + `prompt` or a non-empty `tasks` array");
        }
        if tasks.len() > MAX_BATCH_TASKS {
            bail!("`delegate` accepts at most {MAX_BATCH_TASKS} tasks per batch");
        }

        let mut prepared = Vec::with_capacity(tasks.len());
        for (idx, mut input) in tasks.into_iter().enumerate() {
            if input.prompt.trim().is_empty() {
                bail!("subagent task #{} is missing `prompt`", idx + 1);
            }
            let spec = resolve_agent(&host.data_dir, &host.cwd, &input.agent)?;
            let mut blocked_reason = None;

            if let Some(engine) = &host.hooks {
                let mut ctx = HookContext::new(
                    host.permissions.session_id(),
                    host.cwd.clone(),
                    host.target.clone(),
                    host.permissions.mode().label(),
                    HookEvent::SubagentStart,
                    hook_state,
                );
                ctx.turn_seq = host.turn_seq;
                ctx.subagent_name = Some(spec.name.clone());
                ctx.prompt = Some(input.prompt.clone());

                let out = engine.dispatch(ctx).await;
                emit_hook_event(parent_tx, &out);
                hook_state = out.state;

                match out.decision {
                    HookDecision::Continue => {}
                    HookDecision::Ask { reason, by, .. } => {
                        if let Err(err) = confirm_hook_ask(
                            &host.asks,
                            parent_tx,
                            HookEvent::SubagentStart,
                            &reason,
                            &by,
                        )
                        .await
                        {
                            blocked_reason = Some(err);
                        }
                    }
                    HookDecision::Block { reason, .. } | HookDecision::Deny { reason, .. } => {
                        blocked_reason = Some(format!("blocked by SubagentStart hook: {reason}"));
                    }
                }
                if !out.additional_context.is_empty() {
                    input.prompt.push_str("\n\n<hook_context>\n");
                    input.prompt.push_str(&out.additional_context.join("\n"));
                    input.prompt.push_str("\n</hook_context>");
                }
            }

            prepared.push(PreparedTask {
                index: idx + 1,
                input,
                spec,
                blocked_reason,
            });
        }

        let total = prepared.len();
        let mut reports: Vec<SubagentTaskReport> = Vec::with_capacity(total);
        let mut runnable = Vec::new();
        for task in prepared {
            forward::announce_task(parent_tx, parent_call_id, total, &task);
            if let Some(reason) = task.blocked_reason {
                reports.push(SubagentTaskReport {
                    index: task.index,
                    agent: task.spec.name,
                    description: task
                        .input
                        .description
                        .unwrap_or_else(|| compact_preview(&task.input.prompt, 48)),
                    model: String::new(),
                    ok: false,
                    duration_ms: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    summary: reason,
                });
            } else {
                runnable.push(task);
            }
        }

        let (ran, hook_state) =
            isolate::run_tasks(host, parent_call_id, runnable, hook_state, parent_tx, total).await;
        reports.extend(ran);

        reports.sort_by_key(|r| r.index);
        let all_ok = reports.iter().all(|r| r.ok);
        let input_tokens = reports.iter().map(|r| r.input_tokens).sum();
        let output_tokens = reports.iter().map(|r| r.output_tokens).sum();
        Ok((
            format_reports(&reports),
            all_ok,
            hook_state,
            input_tokens,
            output_tokens,
        ))
    })
}

/// Apply keys the child actually changed. A missing child key must not delete
/// the parent value, and an unchanged key must not clobber a sibling's update.
pub(super) fn merge_hook_state_delta(
    base: &BTreeMap<String, Value>,
    child: &BTreeMap<String, Value>,
    target: &mut BTreeMap<String, Value>,
) {
    for (k, v) in child {
        if base.get(k) != Some(v) {
            target.insert(k.clone(), v.clone());
        }
    }
}

fn emit_line(tx: &mpsc::UnboundedSender<AgentEvent>, call_id: &str, text: String, err: bool) {
    let _ = tx.send(AgentEvent::ToolOutput {
        call_id: call_id.to_string(),
        text,
        err,
    });
}

fn compact_preview(s: &str, max_chars: usize) -> String {
    let first = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let count = first.chars().count();
    if count <= max_chars {
        first.to_string()
    } else {
        let head: String = first.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn format_reports(reports: &[SubagentTaskReport]) -> String {
    if reports.len() == 1 {
        let r = &reports[0];
        let status = if r.ok { "ok" } else { "error" };
        return format!(
            "[subagent:{} | model:{} | status:{} | {}ms | in:{} out:{}]\n\n{}",
            r.agent, r.model, status, r.duration_ms, r.input_tokens, r.output_tokens, r.summary
        );
    }

    let mut out = String::new();
    let ok_count = reports.iter().filter(|r| r.ok).count();
    out.push_str(&format!(
        "Subagent batch finished: {ok_count}/{} succeeded.\n",
        reports.len()
    ));
    for r in reports {
        let status = if r.ok { "ok" } else { "error" };
        out.push_str(&format!(
            "\n### Task #{} — `{}` ({}) [{} | {}ms | {}]\n{}\n",
            r.index, r.agent, r.description, status, r.duration_ms, r.model, r.summary
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Credential, ProviderProtocol};
    use crate::permissions::PermissionMode;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn hook_state_merge_keeps_keys_the_child_omitted() {
        let base = BTreeMap::from([
            ("keep".into(), serde_json::json!(1)),
            ("shared".into(), serde_json::json!("base")),
        ]);
        let mut target = base.clone();
        target.insert("sibling".into(), serde_json::json!(true));
        let child = BTreeMap::from([("shared".into(), serde_json::json!("child"))]);
        merge_hook_state_delta(&base, &child, &mut target);
        assert_eq!(target.get("keep"), Some(&serde_json::json!(1)));
        assert_eq!(target.get("shared"), Some(&serde_json::json!("child")));
        assert_eq!(target.get("sibling"), Some(&serde_json::json!(true)));
    }

    #[tokio::test]
    async fn delegate_runs_parallel_readonly_batch_and_blocks_nesting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen_bodies = Arc::new(Mutex::new(Vec::<String>::new()));
        let bodies_srv = Arc::clone(&seen_bodies);

        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                // Read HTTP headers + body using Content-Length when available.
                while let Ok(n) = sock.read(&mut tmp).await {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let hdr = String::from_utf8_lossy(&buf[..pos]);
                        let cl = hdr
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                if k.eq_ignore_ascii_case("content-length") {
                                    v.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);
                        if buf.len() >= pos + 4 + cl {
                            break;
                        }
                    }
                }
                let raw = String::from_utf8_lossy(&buf).to_string();
                let body_str = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                bodies_srv.lock().unwrap().push(body_str.clone());

                let sse = if body_str.contains("First check") {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"finding-A\"}}]}\n\ndata: [DONE]\n\n"
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"finding-B\"}}]}\n\ndata: [DONE]\n\n"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}",
                    sse.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let dir = std::env::temp_dir().join(format!(
            "jade-subagent-run-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let provider = StoredProvider {
            id: "test".into(),
            name: "Test".into(),
            protocol: ProviderProtocol::OpenAiChat,
            base_url: format!("http://{addr}/v1"),
            credential: Credential::ApiKey {
                key: "sk-test".into(),
            },
            custom_models: vec!["fast-mini".into()],
            fetched_models: Vec::new(),
            model_routes: BTreeMap::new(),
            extra_headers: Default::default(),
            updated_ms: 0,
        };
        let host = SubagentHost {
            data_dir: dir.clone(),
            conduit: None,
            local_socket: None,
            local_server: None,
            target: "local".into(),
            cwd: dir.clone(),
            provider,
            model: ModelRef {
                provider_id: "test".into(),
                model_id: "fast-mini".into(),
            },
            asks: AskBroker::new(),
            permissions: PermissionEngine::new(
                dir.clone(),
                dir.clone(),
                "s1".into(),
                "local".into(),
                PermissionMode::Default,
            ),
            mcp_pool: None,
            tools: None,
            providers: None,
            hooks: None,
            turn_seq: 1,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        let tasks = vec![
            SubagentTaskInput {
                agent: "explore".into(),
                prompt: "First check".into(),
                description: Some("first".into()),
                model_override: None,
            },
            SubagentTaskInput {
                agent: "review".into(),
                prompt: "Second check".into(),
                description: Some("second".into()),
                model_override: None,
            },
        ];

        let (out, all_ok, _, _, _) = execute_delegate(&host, "call_1", tasks, BTreeMap::new(), &tx)
            .await
            .unwrap();
        assert!(all_ok);
        assert!(out.contains("2/2 succeeded"));
        assert!(out.contains("finding-A") || out.contains("finding-B"));

        let bodies = seen_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        for body in bodies.iter() {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            let tool_names: Vec<&str> = v["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|t| t["function"]["name"].as_str())
                .collect();
            assert!(!tool_names.contains(&"delegate"));
            assert!(!tool_names.contains(&"write"));
            assert!(tool_names.contains(&"read"));
        }

        let mut saw_progress = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, AgentEvent::ToolOutput { .. }) {
                saw_progress = true;
            }
        }
        assert!(saw_progress);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
