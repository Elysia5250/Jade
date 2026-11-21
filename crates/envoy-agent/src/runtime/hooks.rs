//! Runtime lifecycle hook glue: `SessionStart`, `UserPromptSubmit`, `PreCompact`,
//! `PostRewind`, and bounded `SessionEnd` on close/daemon shutdown.

use super::ops::record_turn_checkpoint;
use super::server::{Live, LivePhase, Runtime};
use crate::agent::context::{
    generate_handoff, prepare_context, resolve_budget, tools_overhead, PreCompactHook,
    TokenCalibration,
};
use crate::agent::prompt::build_system_prompt;
use crate::agent::{run_agent_turn, AgentEvent, TurnConfig};
use crate::auth::StoredProvider;
use crate::hooks::{
    confirm_hook_ask, emit_hook_event, HookContext, HookDecision, HookEngine, HookEvent,
    HookStepStatus, SHUTDOWN_HOOK_TIMEOUT_MS,
};
use crate::session::{ChatMessage, ChatRole};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

impl Runtime {
    /// Ensure session exists and run `SessionStart` once on fresh creation.
    pub async fn ensure_with_hooks(
        self: &Arc<Self>,
        id: String,
        cwd: PathBuf,
        target: String,
        history: Vec<ChatMessage>,
    ) -> Arc<Mutex<Live>> {
        let (live, run_hooks) =
            self.ensure_in(id.clone(), cwd, target, history, LivePhase::Initializing);
        // Cancellation during SessionStart must not leave the phase stuck.
        let _init = run_hooks.then(|| InitGuard::new(Arc::clone(&live)));
        let start_args = if run_hooks {
            let state = live.lock().unwrap();
            let snap = state.hook_state.lock().unwrap().clone();
            Some((
                state.hooks.clone(),
                state.cwd.clone(),
                state.target.clone(),
                state.permissions.mode().label().to_string(),
                snap,
            ))
        } else {
            None
        };

        if let Some((engine, cwd, target, perm_mode, snap)) = start_args {
            let mut ctx =
                HookContext::new(id, cwd, target, perm_mode, HookEvent::SessionStart, snap);
            ctx.can_block = false;
            // Stay under the client attach budget (5s) so SessionStart cannot
            // outlive the first framed response.
            let out = engine.dispatch_bounded(ctx, Some(4_000)).await;

            let mut state = live.lock().unwrap();
            *state.hook_state.lock().unwrap() = out.state;
            if !out.additional_context.is_empty() {
                state.pending_session_context.extend(out.additional_context);
            }
            if !out.trace.is_empty() {
                let count = out
                    .trace
                    .iter()
                    .filter(|t| t.status != HookStepStatus::Skipped)
                    .count();
                let duration_ms: u64 = out.trace.iter().map(|t| t.duration_ms).sum();
                self.publish(
                    &mut state,
                    AgentEvent::HookRan {
                        event: HookEvent::SessionStart.label().into(),
                        count,
                        blocked: false,
                        reason: String::new(),
                        duration_ms,
                    },
                );
            }
            if state.phase == LivePhase::Initializing {
                state.release();
            }
        }
        live
    }

    /// Run `UserPromptSubmit` (if `prompt.is_some()`), record checkpoint, and invoke the agent loop.
    pub(crate) async fn run_turn_with_hooks(
        self: Arc<Self>,
        id: String,
        live: Arc<Mutex<Live>>,
        prompt: Option<String>,
        mut cfg: TurnConfig,
        tx: mpsc::UnboundedSender<AgentEvent>,
    ) {
        if let Some(raw_prompt) = prompt {
            let hook_engine = cfg.hooks.clone();
            let mut final_prompt = raw_prompt.clone();
            let mut hook_extras = Vec::new();

            if let Some(engine) = hook_engine {
                let mut ctx = HookContext::new(
                    id.clone(),
                    cfg.cwd.clone(),
                    cfg.target.clone(),
                    cfg.permissions.mode().label(),
                    HookEvent::UserPromptSubmit,
                    cfg.hook_state.clone(),
                );
                ctx.turn_seq = cfg.turn_seq;
                ctx.prompt = Some(raw_prompt.clone());

                let out = engine.dispatch(ctx).await;
                emit_hook_event(&tx, &out);
                cfg.hook_state = out.state.clone();
                if let Some(sink) = &cfg.hook_state_sink {
                    *sink.lock().unwrap() = out.state;
                }

                match out.decision {
                    HookDecision::Continue => {}
                    HookDecision::Ask { reason, by, .. } => {
                        if let Err(err) = confirm_hook_ask(
                            &cfg.asks,
                            &tx,
                            HookEvent::UserPromptSubmit,
                            &reason,
                            &by,
                        )
                        .await
                        {
                            let _ = tx.send(AgentEvent::TurnError(err));
                            return;
                        }
                    }
                    HookDecision::Block { reason, .. } | HookDecision::Deny { reason, .. } => {
                        let _ = tx.send(AgentEvent::TurnError(format!(
                            "blocked by UserPromptSubmit hook: {reason}"
                        )));
                        return;
                    }
                }

                if let Some(updated) = out.prompt {
                    final_prompt = updated;
                }
                hook_extras = out.additional_context;
            }

            let mut extras = {
                let mut st = live.lock().unwrap();
                std::mem::take(&mut st.pending_session_context)
            };
            extras.extend(hook_extras);

            let model_content = if extras.is_empty() {
                final_prompt.clone()
            } else {
                format!(
                    "{final_prompt}\n\n<hook_context>\n{}\n</hook_context>",
                    extras.join("\n")
                )
            };

            record_turn_checkpoint(&live, Some(&final_prompt));
            {
                let mut state = live.lock().unwrap();
                let images = std::mem::take(&mut state.pending_turn_images);
                crate::session::strip_prior_turn_reasoning(&mut state.messages);
                crate::session::strip_prior_turn_reasoning(&mut cfg.history);
                let msg = ChatMessage {
                    role: ChatRole::User,
                    content: model_content.clone(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    reasoning_blocks: Vec::new(),
                    images: images.clone(),
                };
                state.messages.push(msg.clone());
                cfg.history.push(msg);
                let display_text = if images.is_empty() {
                    final_prompt
                } else if final_prompt.trim().is_empty() {
                    format!("[attached {} image(s)]", images.len())
                } else {
                    format!("{final_prompt}\n[attached {} image(s)]", images.len())
                };
                self.publish(
                    &mut state,
                    AgentEvent::UserMessage {
                        text: display_text,
                        mode: "Agent".into(),
                    },
                );
            }
        }

        run_agent_turn(cfg, tx).await;
    }

    pub(crate) async fn run_post_rewind_hook(
        &self,
        id: &str,
        live: &Arc<Mutex<Live>>,
        checkpoint_id: u64,
    ) {
        let (engine, cwd, target, mode, turn_seq, snap) = {
            let state = live.lock().unwrap();
            let snap = state.hook_state.lock().unwrap().clone();
            (
                state.hooks.clone(),
                state.cwd.clone(),
                state.target.clone(),
                state.permissions.mode().label().to_string(),
                state.turn_seq,
                snap,
            )
        };
        let mut ctx = HookContext::new(id, cwd, target, mode, HookEvent::PostRewind, snap);
        ctx.turn_seq = turn_seq;
        ctx.can_block = false;
        ctx.rewind_checkpoint_id = Some(checkpoint_id);

        let out = engine.dispatch(ctx).await;
        let mut state = live.lock().unwrap();
        *state.hook_state.lock().unwrap() = out.state;
        if !out.trace.is_empty() {
            let count = out
                .trace
                .iter()
                .filter(|t| t.status != HookStepStatus::Skipped)
                .count();
            let duration_ms: u64 = out.trace.iter().map(|t| t.duration_ms).sum();
            self.publish(
                &mut state,
                AgentEvent::HookRan {
                    event: HookEvent::PostRewind.label().into(),
                    count,
                    blocked: false,
                    reason: String::new(),
                    duration_ms,
                },
            );
        }
    }

    pub(crate) async fn close_session(&self, id: &str) -> Result<bool> {
        let Some(live) = self.remove_idle_session(id)? else {
            return Ok(false);
        };
        fire_session_end(id, &live, "session_closed", Some(SHUTDOWN_HOOK_TIMEOUT_MS)).await;
        Ok(true)
    }

    /// Run `SessionEnd` across all live sessions within a bounded 2s drain window for daemon stop.
    pub async fn shutdown_all(&self) {
        let entries = self.drain_sessions();
        let start = Instant::now();
        const TOTAL_BUDGET_MS: u64 = 2_000;
        for (id, live) in entries {
            let remaining = TOTAL_BUDGET_MS.saturating_sub(start.elapsed().as_millis() as u64);
            if remaining == 0 {
                break;
            }
            fire_session_end(
                &id,
                &live,
                "daemon_shutdown",
                Some(remaining.min(SHUTDOWN_HOOK_TIMEOUT_MS)),
            )
            .await;
        }
    }
}

struct InitGuard {
    live: Arc<Mutex<Live>>,
}

impl InitGuard {
    fn new(live: Arc<Mutex<Live>>) -> Self {
        Self { live }
    }
}

impl Drop for InitGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.live.lock() {
            if state.phase == LivePhase::Initializing {
                state.release();
            }
        }
    }
}

async fn fire_session_end(id: &str, live: &Arc<Mutex<Live>>, reason: &str, budget_ms: Option<u64>) {
    let args = {
        let mut state = live.lock().unwrap();
        if state.session_ended {
            None
        } else {
            state.session_ended = true;
            Some((
                state.hooks.clone(),
                state.cwd.clone(),
                state.target.clone(),
                state.permissions.mode().label().to_string(),
                state.turn_seq,
                state.hook_state.lock().unwrap().clone(),
            ))
        }
    };
    let Some((engine, cwd, target, mode, turn_seq, snap)) = args else {
        return;
    };
    let mut ctx = HookContext::new(id, cwd, target, mode, HookEvent::SessionEnd, snap);
    ctx.turn_seq = turn_seq;
    ctx.can_block = false;
    ctx.shutdown_reason = Some(reason.to_string());
    let out = engine.dispatch_bounded(ctx, budget_ms).await;
    let state = live.lock().unwrap();
    *state.hook_state.lock().unwrap() = out.state;
}

pub(crate) fn new_hook_engine(data_dir: &Path, cwd: &Path) -> Arc<HookEngine> {
    HookEngine::new(data_dir.to_path_buf(), cwd.to_path_buf())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_manual_context_op(
    runtime: &Runtime,
    id: &str,
    live: &Arc<Mutex<Live>>,
    data_dir: &Path,
    provider: &StoredProvider,
    model_id: &str,
    target: &str,
    cwd: &Path,
    messages: &mut Vec<ChatMessage>,
    focus: Option<&str>,
    handoff: bool,
) -> Result<AgentEvent> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()?;
    let system = build_system_prompt(target, cwd);
    if handoff {
        let out = generate_handoff(
            &http, data_dir, provider, model_id, &system, messages, focus, id,
        )
        .await?;
        if let crate::agent::context::SummarySource::Fallback(reason) = &out.summary_source {
            let mut state = live.lock().unwrap();
            runtime.publish(
                &mut state,
                AgentEvent::Say {
                    text: format!("handoff fell back to deterministic summary ({reason})"),
                    kind: "warn".into(),
                },
            );
        }
        return Ok(AgentEvent::HandoffReady {
            summary: out.summary,
            before_tokens: out.before_tokens,
            after_tokens: out.after_tokens,
            title: out.title,
        });
    }

    let (engine, mode, turn_seq, snap) = {
        let state = live.lock().unwrap();
        let snap = state.hook_state.lock().unwrap().clone();
        (
            state.hooks.clone(),
            state.permissions.mode().label().to_string(),
            state.turn_seq,
            snap,
        )
    };
    let mut hctx = HookContext::new(
        id,
        cwd.to_path_buf(),
        target,
        mode,
        HookEvent::PreCompact,
        snap,
    );
    hctx.turn_seq = turn_seq;
    hctx.can_block = false;
    hctx.compact_auto = Some(false);

    let plan = resolve_budget(data_dir, &provider.id, model_id);
    let outcome = prepare_context(
        &http,
        data_dir,
        provider,
        model_id,
        target,
        cwd,
        &system,
        messages,
        TokenCalibration::default(),
        &plan,
        tools_overhead(&[]),
        true,
        focus,
        None,
        Some(PreCompactHook {
            engine: &engine,
            ctx: &mut hctx,
        }),
        id,
    )
    .await;

    {
        let mut state = live.lock().unwrap();
        *state.hook_state.lock().unwrap() = hctx.state;
        if !hctx.trace.is_empty() {
            let count = hctx
                .trace
                .iter()
                .filter(|t| t.status != HookStepStatus::Skipped)
                .count();
            let duration_ms: u64 = hctx.trace.iter().map(|t| t.duration_ms).sum();
            runtime.publish(
                &mut state,
                AgentEvent::HookRan {
                    event: HookEvent::PreCompact.label().into(),
                    count,
                    blocked: false,
                    reason: String::new(),
                    duration_ms,
                },
            );
        }
    }

    let outcome =
        outcome?.ok_or_else(|| anyhow::anyhow!("conversation is too short to compact further"))?;

    Ok(AgentEvent::Compacted {
        messages: outcome.messages,
        before_tokens: outcome.before_tokens,
        after_tokens: outcome.after_tokens,
        summary: outcome.summary,
        auto: false,
    })
}
