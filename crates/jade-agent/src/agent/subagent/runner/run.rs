//! One isolated subagent loop: model resolution, event forwarding, timeout.

use super::super::catalog::SubagentSpec;
use super::{compact_preview, emit_line, PreparedTask, SubagentHost, SubagentTaskReport};
use crate::agent::config::{TurnConfig, TurnUsage};
use crate::agent::events::AgentEvent;
use crate::agent::prompt::build_subagent_system_prompt;
use crate::agent::r#loop::{execute_loop, LoopOptions, LoopOutcome};
use crate::auth::resolve_model_target;
use crate::hooks::{emit_hook_event, HookContext, HookEvent};
use crate::session::{ChatMessage, ChatRole};
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub(super) async fn run_single_subagent(
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
    let child_perm =
        host.permissions
            .fork_for_subagent(perm_mode, !spec.is_readonly(), host.cwd.clone());
    let mut cfg = TurnConfig {
        data_dir: host.data_dir.clone(),
        conduit: host.conduit.clone(),
        local_socket: host.local_socket.clone(),
        local_server: host.local_server.clone(),
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

            reasoning_blocks: Vec::new(),
            images: Vec::new(),
        }],
        asks: Arc::clone(&host.asks),
        steer: Arc::new(Mutex::new(Vec::new())),
        permissions: child_perm,
        monitors: None,
        mcp_pool: host.mcp_pool.clone(),
        tools: host.tools.clone(),
        providers: host.providers.clone(),
        hooks: host.hooks.clone(),
        hook_state: child_hook_state.clone(),
        hook_state_sink: None,
        turn_seq: host.turn_seq,
        max_steps: None,
        usage: None,
    };
    let usage = Arc::new(Mutex::new(TurnUsage::default()));
    cfg.usage = Some(Arc::clone(&usage));

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
    let tap_tx = parent_tx.clone();
    let tap_call = parent_call_id.to_string();
    let tap_prefix = prefix.clone();
    let tap_name = spec.name.clone();
    let tap_desc = desc.clone();
    let tap_index = task.index;
    let forwarder = tokio::spawn(async move {
        let mut tap = super::forward::ChildTap::new(
            tap_tx,
            &tap_call,
            tap_index,
            total,
            &tap_name,
            &tap_desc,
            &tap_prefix,
        );
        while let Some(ev) = child_rx.recv().await {
            tap.push(ev);
        }
        tap.flush_text();
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
            close_lane(
                parent_tx,
                lane_id(parent_call_id, task.index, total, &spec.name, &desc),
                true,
                None,
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
            close_lane(
                parent_tx,
                lane_id(parent_call_id, task.index, total, &spec.name, &desc),
                false,
                Some(&msg),
            );
            let (input_tokens, output_tokens) = charged(&usage);
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
                    input_tokens,
                    output_tokens,
                    summary: format!("error: {msg}{}", mcp_shutdown_note(spec)),
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
            close_lane(
                parent_tx,
                lane_id(parent_call_id, task.index, total, &spec.name, &desc),
                false,
                Some(&msg),
            );
            let (input_tokens, output_tokens) = charged(&usage);
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
                    input_tokens,
                    output_tokens,
                    summary: format!("error: {msg}{}", mcp_shutdown_note(spec)),
                },
                next_state,
            )
        }
    }
}

fn lane_id<'a>(
    parent_call_id: &'a str,
    index: usize,
    total: usize,
    name: &'a str,
    description: &'a str,
) -> super::forward::LaneId<'a> {
    super::forward::LaneId {
        parent_call_id,
        index: u32::try_from(index).unwrap_or(u32::MAX),
        total: u32::try_from(total).unwrap_or(u32::MAX),
        name,
        description,
    }
}

fn close_lane(
    tx: &mpsc::UnboundedSender<AgentEvent>,
    id: super::forward::LaneId<'_>,
    ok: bool,
    detail: Option<&str>,
) {
    use super::forward::{cap_text, emit_subagent, LaneId};
    use crate::agent::events::SubagentNote;
    if let Some(detail) = detail {
        emit_subagent(
            tx,
            LaneId {
                parent_call_id: id.parent_call_id,
                index: id.index,
                total: id.total,
                name: id.name,
                description: id.description,
            },
            SubagentNote::Text {
                text: cap_text(detail, 800),
            },
        );
    }
    emit_subagent(tx, id, SubagentNote::Finished { ok });
}

/// `McpManager` has no close or shutdown method. A timed-out or failed child
/// cannot drain its client; the note records that the close was skipped.
fn charged(slot: &Arc<Mutex<TurnUsage>>) -> (u64, u64) {
    let usage = slot.lock().unwrap();
    (usage.input_tokens, usage.output_tokens)
}

fn mcp_shutdown_note(spec: &SubagentSpec) -> &'static str {
    if spec.is_readonly() {
        ""
    } else {
        "\nMCP manager has no close method; graceful MCP shutdown was skipped"
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
