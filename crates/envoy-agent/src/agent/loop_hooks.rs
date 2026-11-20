//! Explicit `HookContext` orchestration helpers for [`super::r#loop::execute_loop`].

use super::asks::AskBroker;
use super::context::{
    prepare_context, BudgetPlan, CompactionOutcome, PreCompactHook, TokenCalibration,
};
use super::events::AgentEvent;
use super::monitors::MonitorBroker;
use super::subagent::SubagentHost;
use super::tools::{run_prepared_tool_call, ToolExecOutcome, ToolFilter};
use crate::auth::StoredProvider;
use crate::hooks::{
    confirm_hook_ask, emit_hook_event, HookContext, HookDecision, HookEngine, HookEvent,
    MAX_STOP_HOOK_REENTRIES,
};
use crate::mcp::client::McpManager;
use crate::permissions::PermissionEngine;
use crate::session::{ChatMessage, ChatRole, ToolCall};
use anyhow::{bail, Result};
use envoy_service::{AgentDialer, Remote};
use futures::future::join_all;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

pub(crate) struct TurnHookSession {
    pub(crate) engine: Option<Arc<HookEngine>>,
    pub(crate) session_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) target: String,
    pub(crate) permission_mode: String,
    pub(crate) turn_seq: u64,
    pub(crate) depth: u8,
    pub(crate) subagent_name: Option<String>,
    pub(crate) state: BTreeMap<String, Value>,
    pub(crate) stop_hook_count: u8,
}

pub(crate) struct PreparedTool {
    pub(crate) call: ToolCall,
    pub(crate) effective_args: Value,
    pub(crate) blocked_error: Option<String>,
    pub(crate) pre_context: Vec<String>,
}

impl TurnHookSession {
    pub(crate) fn base_ctx(&self, event: HookEvent, step: usize) -> HookContext {
        let mut ctx = HookContext::new(
            self.session_id.clone(),
            self.cwd.clone(),
            self.target.clone(),
            self.permission_mode.clone(),
            event,
            self.state.clone(),
        );
        ctx.turn_seq = self.turn_seq;
        ctx.step = step;
        ctx.depth = self.depth;
        ctx.subagent_name = self.subagent_name.clone();
        ctx.stop_hook_active = self.stop_hook_count > 0;
        ctx.stop_hook_count = self.stop_hook_count;
        ctx
    }

    pub(crate) async fn run_pre_model_call(
        &mut self,
        step: usize,
        model_id: &str,
        estimated_tokens: u64,
        context_tokens: u64,
        asks: &Arc<AskBroker>,
        tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<Option<String>> {
        let Some(engine) = &self.engine else {
            return Ok(None);
        };
        let mut ctx = self.base_ctx(HookEvent::PreModelCall, step);
        ctx.model_id = Some(model_id.to_string());
        ctx.estimated_tokens = Some(estimated_tokens);
        ctx.context_tokens = Some(context_tokens);

        let out = engine.dispatch(ctx).await;
        emit_hook_event(tx, &out);
        self.state = out.state;

        match out.decision {
            HookDecision::Continue => {}
            HookDecision::Ask { reason, by, .. } => {
                if let Err(err) =
                    confirm_hook_ask(asks, tx, HookEvent::PreModelCall, &reason, &by).await
                {
                    bail!("{err}");
                }
            }
            HookDecision::Block { reason, .. } | HookDecision::Deny { reason, .. } => {
                bail!("blocked by PreModelCall hook: {reason}");
            }
        }

        if out.additional_context.is_empty() {
            Ok(None)
        } else {
            Ok(Some(out.additional_context.join("\n")))
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_post_model_call(
        &mut self,
        step: usize,
        model_id: &str,
        input_tokens: u64,
        output_tokens: u64,
        assistant_text: &str,
        planned_tools: Vec<String>,
        asks: &Arc<AskBroker>,
        tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<Vec<String>> {
        let Some(engine) = &self.engine else {
            return Ok(Vec::new());
        };
        let mut ctx = self.base_ctx(HookEvent::PostModelCall, step);
        ctx.model_id = Some(model_id.to_string());
        ctx.input_tokens = Some(input_tokens);
        ctx.output_tokens = Some(output_tokens);
        ctx.assistant_text = Some(assistant_text.to_string());
        ctx.planned_tools = planned_tools;

        let out = engine.dispatch(ctx).await;
        emit_hook_event(tx, &out);
        self.state = out.state;

        match out.decision {
            HookDecision::Continue => {}
            HookDecision::Ask { reason, by, .. } => {
                if let Err(err) =
                    confirm_hook_ask(asks, tx, HookEvent::PostModelCall, &reason, &by).await
                {
                    bail!("{err}");
                }
            }
            HookDecision::Block { reason, .. } | HookDecision::Deny { reason, .. } => {
                bail!("blocked by PostModelCall hook: {reason}");
            }
        }
        Ok(out.additional_context)
    }

    pub(crate) async fn prepare_tool_batch(
        &mut self,
        step: usize,
        tool_calls: &[ToolCall],
        asks: &Arc<AskBroker>,
        tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Vec<PreparedTool> {
        let mut prepared = Vec::with_capacity(tool_calls.len());
        for tc in tool_calls {
            let args: Value = match serde_json::from_str(&tc.arguments) {
                Ok(value) => value,
                Err(err) => {
                    prepared.push(PreparedTool {
                        call: tc.clone(),
                        effective_args: Value::Null,
                        blocked_error: Some(format!(
                            "invalid JSON tool arguments for `{}`: {err}",
                            tc.name
                        )),
                        pre_context: Vec::new(),
                    });
                    continue;
                }
            };
            let Some(engine) = &self.engine else {
                prepared.push(PreparedTool {
                    call: tc.clone(),
                    effective_args: args,
                    blocked_error: None,
                    pre_context: Vec::new(),
                });
                continue;
            };

            let mut ctx = self.base_ctx(HookEvent::PreToolUse, step);
            ctx.tool_name = Some(tc.name.clone());
            ctx.tool_call_id = Some(tc.id.clone());
            ctx.tool_input = Some(args.clone());

            let out = engine.dispatch(ctx).await;
            emit_hook_event(tx, &out);
            self.state = out.state;

            let mut effective_args = out.tool_input.unwrap_or(args);
            let mut pre_context = out.additional_context;
            let mut blocked_error = match out.decision {
                HookDecision::Continue => None,
                HookDecision::Ask { reason, by, .. } => {
                    confirm_hook_ask(asks, tx, HookEvent::PreToolUse, &reason, &by)
                        .await
                        .err()
                }
                HookDecision::Block { reason, .. } | HookDecision::Deny { reason, .. } => {
                    Some(format!("blocked by PreToolUse hook: {reason}"))
                }
            };

            if blocked_error.is_none()
                && tc.name == "monitor"
                && effective_args["action"].as_str() == Some("start")
            {
                if let Some(cmd) = effective_args["command"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let mut exec_input = json!({ "command": cmd });
                    if let Some(raw_cwd) = effective_args["cwd"].as_str() {
                        exec_input["cwd"] = Value::String(raw_cwd.to_string());
                    }
                    let mut exec_ctx = self.base_ctx(HookEvent::PreToolUse, step);
                    exec_ctx.tool_name = Some("exec".into());
                    exec_ctx.tool_call_id = Some(tc.id.clone());
                    exec_ctx.tool_input = Some(exec_input);
                    let exec_out = engine.dispatch(exec_ctx).await;
                    emit_hook_event(tx, &exec_out);
                    self.state = exec_out.state;
                    pre_context.extend(exec_out.additional_context);
                    if let Some(updated) = exec_out.tool_input {
                        if let Some(new_cmd) = updated.get("command").and_then(Value::as_str) {
                            effective_args["command"] = Value::String(new_cmd.to_string());
                        }
                        if let Some(new_cwd) = updated.get("cwd").and_then(Value::as_str) {
                            effective_args["cwd"] = Value::String(new_cwd.to_string());
                        }
                    }
                    blocked_error = match exec_out.decision {
                        HookDecision::Continue => None,
                        HookDecision::Ask { reason, by, .. } => {
                            confirm_hook_ask(asks, tx, HookEvent::PreToolUse, &reason, &by)
                                .await
                                .err()
                        }
                        HookDecision::Block { reason, .. } | HookDecision::Deny { reason, .. } => {
                            Some(format!("blocked by PreToolUse hook: {reason}"))
                        }
                    };
                }
            }

            prepared.push(PreparedTool {
                call: tc.clone(),
                effective_args,
                blocked_error,
                pre_context,
            });
        }
        prepared
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn finish_tool_call(
        &mut self,
        step: usize,
        tc: &ToolCall,
        effective_args: &Value,
        mut content: String,
        ok: bool,
        pre_context: Vec<String>,
        tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> String {
        let mut extras = pre_context;
        if let Some(engine) = &self.engine {
            let ev = if ok {
                HookEvent::PostToolUse
            } else {
                HookEvent::PostToolUseFailure
            };
            let mut ctx = self.base_ctx(ev, step);
            ctx.tool_name = Some(tc.name.clone());
            ctx.tool_call_id = Some(tc.id.clone());
            ctx.tool_input = Some(effective_args.clone());
            ctx.tool_output = Some(content.clone());
            ctx.tool_ok = Some(ok);

            let out = engine.dispatch(ctx).await;
            emit_hook_event(tx, &out);
            self.state = out.state;
            extras.extend(out.additional_context);
        }

        if !extras.is_empty() {
            content.push_str("\n\n<hook_context>\n");
            content.push_str(&extras.join("\n"));
            content.push_str("\n</hook_context>");
        }
        content
    }

    pub(crate) async fn check_stop_hook(
        &mut self,
        step: usize,
        assistant_text: &str,
        can_reenter: bool,
        tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Option<String> {
        let engine = self.engine.as_ref()?;
        let event = if self.depth == 0 {
            HookEvent::Stop
        } else {
            HookEvent::SubagentStop
        };
        let allowed_reenter = can_reenter && self.stop_hook_count < MAX_STOP_HOOK_REENTRIES;
        let mut ctx = self.base_ctx(event, step);
        ctx.can_block = allowed_reenter;
        ctx.assistant_text = Some(assistant_text.to_string());
        if self.depth > 0 {
            ctx.subagent_ok = Some(true);
        }

        let out = engine.dispatch(ctx).await;
        emit_hook_event(tx, &out);
        self.state = out.state;

        if allowed_reenter && out.decision.is_blocking() {
            self.stop_hook_count += 1;
            let reason = out
                .decision
                .reason()
                .unwrap_or("continue and complete verification")
                .to_string();
            let mut msg = format!(
                "<stop_hook_feedback event=\"{}\" attempt=\"{}/{}\">\n{}\n</stop_hook_feedback>",
                event.label(),
                self.stop_hook_count,
                MAX_STOP_HOOK_REENTRIES,
                reason
            );
            if !out.additional_context.is_empty() {
                msg.push_str("\n\n<hook_context>\n");
                msg.push_str(&out.additional_context.join("\n"));
                msg.push_str("\n</hook_context>");
            }
            return Some(msg);
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn prepare_context_step(
        &mut self,
        step: usize,
        http: &reqwest::Client,
        data_dir: &Path,
        provider: &StoredProvider,
        model_id: &str,
        target: &str,
        cwd: &Path,
        system: &str,
        messages: &mut Vec<ChatMessage>,
        calibration: TokenCalibration,
        plan: &BudgetPlan,
        tools_overhead: u64,
        force: bool,
        tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<Option<CompactionOutcome>> {
        match self.engine.clone() {
            Some(engine) => {
                let mut hctx = self.base_ctx(HookEvent::PreCompact, step);
                let res = prepare_context(
                    http,
                    data_dir,
                    provider,
                    model_id,
                    target,
                    cwd,
                    system,
                    messages,
                    calibration,
                    plan,
                    tools_overhead,
                    force,
                    None,
                    Some(tx),
                    Some(PreCompactHook {
                        engine: &engine,
                        ctx: &mut hctx,
                    }),
                    &self.session_id,
                )
                .await;
                self.state = hctx.state;
                res
            }
            None => {
                prepare_context(
                    http,
                    data_dir,
                    provider,
                    model_id,
                    target,
                    cwd,
                    system,
                    messages,
                    calibration,
                    plan,
                    tools_overhead,
                    force,
                    None,
                    Some(tx),
                    None,
                    &self.session_id,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_tool_batch(
        &mut self,
        step: usize,
        remote: &Remote<Box<dyn AgentDialer>>,
        target: &str,
        cwd: &Path,
        tool_calls: &[ToolCall],
        messages: &mut Vec<ChatMessage>,
        tx: &mpsc::UnboundedSender<AgentEvent>,
        asks: &Arc<AskBroker>,
        permissions: &Arc<PermissionEngine>,
        mcp: Option<&McpManager>,
        tool_filter: &mut ToolFilter,
        subagent_host: Option<&SubagentHost>,
        monitors: Option<&Arc<dyn MonitorBroker>>,
    ) -> (u64, u64) {
        let all_readonly_batch = batch_is_parallel_readonly(tool_calls);

        let prepared = self.prepare_tool_batch(step, tool_calls, asks, tx).await;
        let shared_filter = &*tool_filter;

        let outcomes: Vec<ToolExecOutcome> = if all_readonly_batch {
            let limit = Arc::new(tokio::sync::Semaphore::new(READONLY_BATCH_LIMIT));
            let futs = prepared.iter().map(|p| {
                let limit = Arc::clone(&limit);
                async move {
                    let _permit = limit.acquire().await.ok();
                    run_prepared_tool_call(
                        remote,
                        target,
                        cwd,
                        p,
                        None,
                        tx,
                        asks,
                        permissions,
                        mcp,
                        shared_filter,
                        subagent_host,
                        monitors,
                    )
                    .await
                }
            });
            join_all(futs).await
        } else {
            let mut seq_outcomes = Vec::with_capacity(prepared.len());
            for p in &prepared {
                let snapshot = if p.call.name == "delegate" {
                    Some(self.state.clone())
                } else {
                    None
                };
                let outcome = run_prepared_tool_call(
                    remote,
                    target,
                    cwd,
                    p,
                    snapshot,
                    tx,
                    asks,
                    permissions,
                    mcp,
                    shared_filter,
                    subagent_host,
                    monitors,
                )
                .await;
                if let Some(next_state) = &outcome.updated_hook_state {
                    self.state = next_state.clone();
                }
                seq_outcomes.push(outcome);
            }
            seq_outcomes
        };

        for outcome in &outcomes {
            if let Some(names) = &outcome.skill_allow {
                tool_filter.restrict_to(names);
            }
        }

        let mut child_in = 0u64;
        let mut child_out = 0u64;
        for (p, outcome) in prepared.into_iter().zip(outcomes) {
            child_in += outcome.child_input_tokens;
            child_out += outcome.child_output_tokens;
            let final_content = self
                .finish_tool_call(
                    step,
                    &p.call,
                    &p.effective_args,
                    outcome.text,
                    outcome.ok,
                    p.pre_context,
                    tx,
                )
                .await;
            messages.push(ChatMessage {
                role: ChatRole::Tool,
                content: final_content,
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: Some(p.call.id),
                tool_name: Some(p.call.name),
            
                reasoning_blocks: Vec::new(),});
        }
        (child_in, child_out)
    }
}

const READONLY_BATCH_LIMIT: usize = 8;

/// `say` is display-only. It must not force an otherwise read-only batch
/// back onto the serial path.
fn batch_is_parallel_readonly(tool_calls: &[ToolCall]) -> bool {
    let mut work = 0usize;
    for tc in tool_calls {
        if tc.name == "say" {
            continue;
        }
        if !matches!(
            tc.name.as_str(),
            "read" | "ls" | "glob" | "grep" | "webfetch"
        ) {
            return false;
        }
        work += 1;
    }
    work > 1
}

pub(crate) fn hook_context_message(extra: &[String]) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        content: format!("<hook_context>\n{}\n</hook_context>", extra.join("\n")),
        reasoning: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        reasoning_blocks: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::config::user_hooks_path;

    #[tokio::test]
    async fn monitor_start_command_dispatches_pre_tool_use_exec_hook() {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let root = std::env::temp_dir().join(format!(
            "envoy-mon-hook-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        let data = root.join("data");
        let ws = root.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            user_hooks_path(&data),
            r#"{
              "hooks": {
                "PreToolUse": [
                  {
                    "id": "block-exec",
                    "matcher": "exec",
                    "command": "echo 'blocked exec inside monitor' >&2; exit 2"
                  }
                ]
              }
            }"#,
        )
        .unwrap();

        let mut sess = TurnHookSession {
            engine: Some(HookEngine::new(data, ws.clone())),
            session_id: "s-mon".into(),
            cwd: ws,
            target: "local".into(),
            permission_mode: "default".into(),
            turn_seq: 1,
            depth: 0,
            subagent_name: None,
            state: BTreeMap::new(),
            stop_hook_count: 0,
        };
        let tc = ToolCall {
            id: "c1".into(),
            name: "monitor".into(),
            arguments: r#"{"action":"start","command":"echo secret","pattern":".*"}"#.into(),
        };
        let asks = AskBroker::new_headless();
        let (tx, _rx) = mpsc::unbounded_channel();
        let out = sess.prepare_tool_batch(0, &[tc], &asks, &tx).await;
        assert!(
            out[0]
                .blocked_error
                .as_deref()
                .is_some_and(|m| m.contains("blocked exec inside monitor")),
            "{:?}",
            out[0].blocked_error
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn invalid_tool_arguments_are_blocked_instead_of_empty_object() {
        let mut sess = TurnHookSession {
            engine: None,
            session_id: "s-json".into(),
            cwd: std::env::temp_dir(),
            target: "local".into(),
            permission_mode: "default".into(),
            turn_seq: 1,
            depth: 0,
            subagent_name: None,
            state: BTreeMap::new(),
            stop_hook_count: 0,
        };
        let tc = ToolCall {
            id: "c-bad".into(),
            name: "read".into(),
            arguments: "{not json".into(),
        };
        let asks = AskBroker::new_headless();
        let (tx, _rx) = mpsc::unbounded_channel();
        let out = sess.prepare_tool_batch(0, &[tc], &asks, &tx).await;
        let err = out[0].blocked_error.as_deref().unwrap();
        assert!(err.contains("invalid JSON tool arguments for `read`"), "{err}");
        assert_ne!(out[0].effective_args, json!({}));
    }

    #[test]
    fn say_does_not_disable_a_readonly_batch() {
        let call = |name: &str| ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: "{}".into(),
        };
        assert!(batch_is_parallel_readonly(&[
            call("say"),
            call("read"),
            call("grep")
        ]));
        assert!(!batch_is_parallel_readonly(&[
            call("say"),
            call("read"),
            call("exec")
        ]));
        assert!(!batch_is_parallel_readonly(&[call("read")]));
    }
}
