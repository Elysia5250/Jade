//! Subagent execution runtime: isolated context, depth=1 guard, parallel read-only batches,
//! timeout + cancellation propagation, and concise progress forwarding.

use super::catalog::{resolve_agent, SubagentSpec};
use crate::agent::asks::AskBroker;
use crate::agent::config::TurnConfig;
use crate::agent::events::AgentEvent;
use crate::agent::prompt::build_subagent_system_prompt;
use crate::agent::r#loop::{execute_loop, LoopOptions, LoopOutcome};
use crate::auth::{resolve_model_target, ModelRef, StoredProvider};
use crate::hooks::{
    confirm_hook_ask, emit_hook_event, HookContext, HookDecision, HookEngine, HookEvent,
};
use crate::permissions::PermissionEngine;
use crate::session::{ChatMessage, ChatRole};
use anyhow::{bail, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

const MAX_BATCH_TASKS: usize = 8;

#[derive(Clone)]
pub(crate) struct SubagentHost {
    pub(crate) data_dir: PathBuf,
    pub(crate) conduit: Option<PathBuf>,
    pub(crate) target: String,
    pub(crate) cwd: PathBuf,
    pub(crate) provider: StoredProvider,
    pub(crate) model: ModelRef,
    pub(crate) asks: Arc<AskBroker>,
    pub(crate) permissions: Arc<PermissionEngine>,
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

pub(crate) fn execute_delegate<'a>(
    host: &'a SubagentHost,
    parent_call_id: &'a str,
    tasks: Vec<SubagentTaskInput>,
    mut hook_state: BTreeMap<String, Value>,
    parent_tx: &'a mpsc::UnboundedSender<AgentEvent>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(String, bool, BTreeMap<String, Value>)>> + Send + 'a>,
> {
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

        let all_readonly = runnable.iter().all(|p| p.spec.is_readonly());
        if all_readonly && runnable.len() > 1 {
            let base_snapshot = hook_state.clone();
            let mut set = JoinSet::new();
            for task in runnable {
                let host = host.clone();
                let call_id = parent_call_id.to_string();
                let tx = parent_tx.clone();
                let child_state = base_snapshot.clone();
                set.spawn(async move {
                    run_single_subagent(task, total, &host, &call_id, child_state, &tx).await
                });
            }
            let mut child_outcomes = Vec::new();
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok((report, child_state)) => {
                        let idx = report.index;
                        reports.push(report);
                        child_outcomes.push((idx, child_state));
                    }
                    Err(e) => reports.push(SubagentTaskReport {
                        index: usize::MAX,
                        agent: "subagent".into(),
                        description: "task".into(),
                        model: String::new(),
                        ok: false,
                        duration_ms: 0,
                        input_tokens: 0,
                        output_tokens: 0,
                        summary: format!("subagent task panicked or cancelled: {e}"),
                    }),
                }
            }
            child_outcomes.sort_by_key(|(idx, _)| *idx);
            for (_, child_state) in child_outcomes {
                merge_hook_state_delta(&base_snapshot, &child_state, &mut hook_state);
            }
        } else {
            for task in runnable {
                let (report, next_state) =
                    run_single_subagent(task, total, host, parent_call_id, hook_state, parent_tx)
                        .await;
                hook_state = next_state;
                reports.push(report);
            }
        }

        reports.sort_by_key(|r| r.index);
        let all_ok = reports.iter().all(|r| r.ok);
        Ok((format_reports(&reports), all_ok, hook_state))
    })
}

fn merge_hook_state_delta(
    base: &BTreeMap<String, Value>,
    child: &BTreeMap<String, Value>,
    target: &mut BTreeMap<String, Value>,
) {
    for key in base.keys() {
        if !child.contains_key(key) {
            target.remove(key);
        }
    }
    for (k, v) in child {
        if base.get(k) != Some(v) {
            target.insert(k.clone(), v.clone());
        }
    }
}

async fn run_single_subagent(
    task: PreparedTask,
    total: usize,
    host: &SubagentHost,
    parent_call_id: &str,
    child_hook_state: BTreeMap<String, Value>,
    parent_tx: &mpsc::UnboundedSender<AgentEvent>,
) -> (SubagentTaskReport, BTreeMap<String, Value>) {
    let start = Instant::now();
    let spec = &task.spec;
    let model_spec = task
        .input
        .model_override
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&spec.model);
    let (provider, model_ref) =
        resolve_model_target(&host.data_dir, &host.provider, &host.model, model_spec);
    let model_label = model_ref.display();
    let desc = task
        .input
        .description
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| compact_preview(&task.input.prompt, 48));

    let prefix = if total > 1 {
        format!("[#{}/{} {}]", task.index, total, spec.name)
    } else {
        format!("[{}]", spec.name)
    };
    emit_line(
        parent_tx,
        parent_call_id,
        format!("{prefix} start ({model_label}) — {desc}"),
        false,
    );

    let perm_mode = spec.mode.permission_mode(host.permissions.mode());
    let child_perm = host
        .permissions
        .fork_for_subagent(perm_mode, !spec.is_readonly());
    let cfg = TurnConfig {
        data_dir: host.data_dir.clone(),
        conduit: host.conduit.clone(),
        target: host.target.clone(),
        cwd: host.cwd.clone(),
        provider,
        model: model_ref,
        history: vec![ChatMessage {
            role: ChatRole::User,
            content: task.input.prompt.clone(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        }],
        asks: Arc::clone(&host.asks),
        steer: Arc::new(Mutex::new(Vec::new())),
        permissions: child_perm,
        monitors: None,
        hooks: host.hooks.clone(),
        hook_state: child_hook_state.clone(),
        hook_state_sink: None,
        turn_seq: host.turn_seq,
    };

    let system = build_subagent_system_prompt(&host.target, &host.cwd, &spec.system_prompt);
    let opts = LoopOptions {
        system,
        max_steps: spec.max_steps,
        depth: 1,
        allowed_tools: spec.effective_tools(),
        connect_mcp: !spec.is_readonly(),
        subagent_host: None,
        subagent_name: Some(spec.name.clone()),
    };

    let (child_tx, mut child_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let fwd_call_id = parent_call_id.to_string();
    let fwd_prefix = prefix.clone();
    let fwd_tx = parent_tx.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(ev) = child_rx.recv().await {
            match ev {
                AgentEvent::ToolStart { title, .. } => {
                    emit_line(
                        &fwd_tx,
                        &fwd_call_id,
                        format!("{fwd_prefix} {title}"),
                        false,
                    );
                }
                AgentEvent::Say { text, .. } => {
                    emit_line(
                        &fwd_tx,
                        &fwd_call_id,
                        format!("{fwd_prefix} note: {text}"),
                        false,
                    );
                }
                ev @ (AgentEvent::FileChanged { .. }
                | AgentEvent::Ask { .. }
                | AgentEvent::AskClosed { .. }
                | AgentEvent::HookRan { .. }) => {
                    let _ = fwd_tx.send(ev);
                }
                _ => {}
            }
        }
    });

    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<LoopOutcome>> + Send + '_>> =
        Box::pin(execute_loop(cfg, opts, &child_tx));
    let res = tokio::time::timeout(Duration::from_secs(spec.timeout_s), fut).await;
    drop(child_tx);
    let _ = forwarder.await;

    let duration_ms = start.elapsed().as_millis() as u64;
    match res {
        Ok(Ok(LoopOutcome {
            messages,
            input_tokens,
            output_tokens,
            hook_state,
            ..
        })) => {
            let summary = extract_final_reply(&messages);
            emit_line(
                parent_tx,
                parent_call_id,
                format!("{prefix} done ({duration_ms}ms)"),
                false,
            );
            (
                SubagentTaskReport {
                    index: task.index,
                    agent: spec.name.clone(),
                    description: desc,
                    model: model_label,
                    ok: true,
                    duration_ms,
                    input_tokens,
                    output_tokens,
                    summary,
                },
                hook_state,
            )
        }
        Ok(Err(e)) => {
            let msg = format!("{e:#}");
            emit_line(
                parent_tx,
                parent_call_id,
                format!("{prefix} error: {msg}"),
                true,
            );
            let next_state =
                run_subagent_stop_fallback(host, spec, &msg, child_hook_state, parent_tx).await;
            (
                SubagentTaskReport {
                    index: task.index,
                    agent: spec.name.clone(),
                    description: desc,
                    model: model_label,
                    ok: false,
                    duration_ms,
                    input_tokens: 0,
                    output_tokens: 0,
                    summary: format!("error: {msg}"),
                },
                next_state,
            )
        }
        Err(_) => {
            let msg = format!("timed out after {}s", spec.timeout_s);
            emit_line(
                parent_tx,
                parent_call_id,
                format!("{prefix} error: {msg}"),
                true,
            );
            let next_state =
                run_subagent_stop_fallback(host, spec, &msg, child_hook_state, parent_tx).await;
            (
                SubagentTaskReport {
                    index: task.index,
                    agent: spec.name.clone(),
                    description: desc,
                    model: model_label,
                    ok: false,
                    duration_ms,
                    input_tokens: 0,
                    output_tokens: 0,
                    summary: format!("error: {msg}"),
                },
                next_state,
            )
        }
    }
}

async fn run_subagent_stop_fallback(
    host: &SubagentHost,
    spec: &SubagentSpec,
    err_summary: &str,
    state: BTreeMap<String, Value>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> BTreeMap<String, Value> {
    let Some(engine) = &host.hooks else {
        return state;
    };
    let mut ctx = HookContext::new(
        host.permissions.session_id(),
        host.cwd.clone(),
        host.target.clone(),
        host.permissions.mode().label(),
        HookEvent::SubagentStop,
        state,
    );
    ctx.turn_seq = host.turn_seq;
    ctx.depth = 1;
    ctx.can_block = false;
    ctx.subagent_name = Some(spec.name.clone());
    ctx.subagent_ok = Some(false);
    ctx.assistant_text = Some(err_summary.to_string());
    let out = engine.dispatch(ctx).await;
    emit_hook_event(tx, &out);
    out.state
}

fn extract_final_reply(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == ChatRole::Assistant && !m.content.trim().is_empty())
        .map(|m| m.content.trim().to_string())
        .unwrap_or_else(|| "(subagent returned no text output)".into())
}

fn emit_line(tx: &mpsc::UnboundedSender<AgentEvent>, call_id: &str, text: String, err: bool) {
    let _ = tx.send(AgentEvent::ToolOutput {
        call_id: call_id.to_string(),
        text,
        err,
    });
}

fn compact_preview(s: &str, max_chars: usize) -> String {
    let first = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
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
            r.agent,
            r.model,
            status,
            r.duration_ms,
            r.input_tokens,
            r.output_tokens,
            r.summary
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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
                loop {
                    let Ok(n) = sock.read(&mut tmp).await else {
                        break;
                    };
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
            "envoy-subagent-run-{:02x}{:02x}{:02x}{:02x}",
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
            model_routes: BTreeMap::new(),
            updated_ms: 0,
        };
        let host = SubagentHost {
            data_dir: dir.clone(),
            conduit: None,
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

        let (out, all_ok, _) =
            execute_delegate(&host, "call_1", tasks, BTreeMap::new(), &tx)
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
