//! Explicit `HookContext` orchestration helpers for [`super::r#loop::execute_loop`].

use super::asks::AskBroker;
use super::context::{prepare_context, BudgetPlan, CompactionOutcome, PreCompactHook, TokenCalibration};
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
            let args: Value = serde_json::from_str(&tc.arguments).unwrap_or_else(|_| json!({}));
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

            let effective_args = out.tool_input.unwrap_or(args);
            let blocked_error = match out.decision {
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

            prepared.push(PreparedTool {
                call: tc.clone(),
                effective_args,
                blocked_error,
                pre_context: out.additional_context,
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
        tool_filter: &ToolFilter,
        subagent_host: Option<&SubagentHost>,
        monitors: Option<&Arc<dyn MonitorBroker>>,
    ) {
        let all_readonly_batch = tool_calls.len() > 1
            && tool_calls.iter().all(|tc| {
                matches!(
                    tc.name.as_str(),
                    "read" | "ls" | "glob" | "grep" | "webfetch"
                )
            });

        let prepared = self.prepare_tool_batch(step, tool_calls, asks, tx).await;

        let outcomes: Vec<ToolExecOutcome> = if all_readonly_batch {
            let futs = prepared.iter().map(|p| {
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
                    tool_filter,
                    subagent_host,
                    monitors,
                )
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
                    tool_filter,
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

        for (p, outcome) in prepared.into_iter().zip(outcomes) {
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
            });
        }
    }
}

pub(crate) fn hook_context_message(extra: &[String]) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        content: format!("<hook_context>\n{}\n</hook_context>", extra.join("\n")),
        reasoning: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_name: None,
    }
}
