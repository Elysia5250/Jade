//! Daemon-side session store, turn runner, and manual `/compact` + `/handoff`.

use super::hooks::run_manual_context_op;
use super::ops::{cancel_active_job, persist_daemon_session, SessionSnapshot};
use super::protocol::{SendMode, SeqEvent, SessionInfo};
use crate::agent::checkpoint::CheckpointStore;
use crate::agent::MonitorBroker;
use crate::agent::{AgentEvent, TurnConfig};
use crate::auth::resolve_provider_with_overrides;
use crate::permissions::PermissionMode;
use crate::session::ChatMessage;
use anyhow::{bail, Result};
use jade_service::AgentServer;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::debug;

pub(crate) use super::live::{Live, LivePhase, PhaseGuard};

pub struct Runtime {
    pub(crate) data_dir: PathBuf,
    pub(crate) conduit: Option<PathBuf>,
    pub(crate) local_server: Option<AgentServer>,
    pub(crate) mcp_pool: Arc<crate::mcp::pool::McpPool>,
    sessions: Mutex<HashMap<String, Arc<Mutex<Live>>>>,
    incarnation_id: u64,
}

impl Runtime {
    pub fn new(data_dir: PathBuf) -> Arc<Self> {
        Self::with_local_server(data_dir, None, None)
    }

    pub fn with_conduit(data_dir: PathBuf, conduit: Option<PathBuf>) -> Arc<Self> {
        Self::with_local_server(data_dir, conduit, None)
    }

    pub fn with_local_server(
        data_dir: PathBuf,
        conduit: Option<PathBuf>,
        local_server: Option<AgentServer>,
    ) -> Arc<Self> {
        let mcp_pool = crate::mcp::pool::McpPool::new(data_dir.clone());
        Arc::new(Self {
            data_dir,
            conduit,
            local_server,
            mcp_pool,
            sessions: Mutex::new(HashMap::new()),
            incarnation_id: new_incarnation(),
        })
    }

    pub(crate) fn incarnation_id(&self) -> u64 {
        self.incarnation_id
    }

    pub(crate) fn session(&self, id: &str) -> Option<Arc<Mutex<Live>>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub(crate) fn remove_idle_session(&self, id: &str) -> Result<Option<Arc<Mutex<Live>>>> {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(live) = sessions.get(id) else {
            return Ok(None);
        };
        if live.lock().unwrap().running {
            anyhow::bail!("session is active; interrupt before closing");
        }
        Ok(sessions.remove(id))
    }

    pub(crate) fn drain_sessions(&self) -> Vec<(String, Arc<Mutex<Live>>)> {
        self.sessions.lock().unwrap().drain().collect()
    }

    /// Test helper: boot a session already `Idle`. Production sessions go
    /// through `ensure_with_hooks`, which claims `Initializing` first.
    #[cfg(test)]
    pub(crate) fn ensure(
        self: &Arc<Self>,
        id: String,
        cwd: PathBuf,
        target: String,
        history: Vec<ChatMessage>,
    ) -> Arc<Mutex<Live>> {
        self.ensure_in(id, cwd, target, history, LivePhase::Idle).0
    }

    pub(crate) fn ensure_in(
        self: &Arc<Self>,
        id: String,
        cwd: PathBuf,
        target: String,
        history: Vec<ChatMessage>,
        boot: LivePhase,
    ) -> (Arc<Mutex<Live>>, bool) {
        let mut map = self.sessions.lock().unwrap();
        if let Some(existing) = map.get(&id).cloned() {
            drop(map);
            let mut state = existing.lock().unwrap();
            state.last_used = Instant::now();
            if state.cwd != cwd || state.target != target {
                state.permissions = state
                    .permissions
                    .rebind_workspace(cwd.clone(), target.clone());
                state.hooks = super::hooks::new_hook_engine(&self.data_dir, &cwd);
                state.cwd = cwd;
                state.target = target;
            }
            let fresh = !state.session_started && state.phase == LivePhase::Idle;
            if fresh && boot == LivePhase::Initializing {
                state.phase = LivePhase::Initializing;
                state.session_started = true;
            }
            drop(state);
            return (existing, fresh && boot == LivePhase::Initializing);
        }
        Live::evict_idle_sessions(&mut map);
        let data_dir = &self.data_dir;
        let mut live = Live::new(self, data_dir, &id, cwd, target, history);
        if let Ok(saved) = crate::session::load_session(data_dir, &id) {
            live.calibration_factor =
                crate::agent::context::TokenCalibration::from_stored(saved.calibration_factor)
                    .factor;
        }
        live.seq = 1;
        live.record_logged(1, &AgentEvent::SessionReset);
        if boot == LivePhase::Initializing {
            live.phase = LivePhase::Initializing;
            live.session_started = true;
        }
        let arc = Arc::new(Mutex::new(live));
        map.insert(id, arc.clone());
        (arc, boot == LivePhase::Initializing)
    }

    pub(crate) fn publish(&self, live: &mut Live, event: AgentEvent) {
        live.seq += 1;
        let seq = live.seq;
        live.record_logged(seq, &event);
        let live_event = if jade_proto::codec::payload_len(&event).is_ok_and(|n| n <= 1024 * 1024) {
            event
        } else {
            super::live::slim_event(&event)
        };
        let item = SeqEvent {
            seq,
            event: live_event,
        };
        let mut dropped_slow = false;
        live.subs.retain(|tx| match tx.try_send(item.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
            Err(mpsc::error::TrySendError::Full(_)) => {
                dropped_slow = true;
                false
            }
        });
        if dropped_slow {
            live.seq += 1;
            let reset = AgentEvent::SessionReset;
            live.record_logged(live.seq, &reset);
            let reset_item = SeqEvent {
                seq: live.seq,
                event: reset,
            };
            for tx in &live.subs {
                let _ = tx.try_send(reset_item.clone());
            }
        }
    }

    pub(crate) fn start_turn(
        self: &Arc<Self>,
        id: &str,
        live: Arc<Mutex<Live>>,
        prompt: Option<String>,
    ) {
        let model_override = live.lock().unwrap().model_override.clone();
        let Some((provider, model)) =
            resolve_provider_with_overrides(&self.data_dir, model_override.as_deref())
        else {
            let mut state = live.lock().unwrap();
            if let Some(text) = prompt {
                self.publish(
                    &mut state,
                    AgentEvent::UserMessage {
                        text,
                        mode: "Agent".into(),
                    },
                );
            }
            state.release();
            self.publish(
                &mut state,
                AgentEvent::TurnError("no model configured: run /login".into()),
            );
            return;
        };
        if model.model_id.trim().is_empty() {
            let mut state = live.lock().unwrap();
            state.release();
            self.publish(
                &mut state,
                AgentEvent::TurnError("no model selected: pass --model or set JADE_MODEL".into()),
            );
            return;
        }

        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let (
            cwd,
            target,
            local_socket,
            history,
            asks,
            steer,
            permissions,
            monitors,
            hooks,
            hook_state,
            hook_state_sink,
            turn_seq,
            max_steps,
            usage,
        ) = {
            let mut state = live.lock().unwrap();
            let claimed = (state.phase == LivePhase::RunningTurn && state.turn.is_none())
                || state.claim(LivePhase::RunningTurn);
            if !claimed {
                self.publish(&mut state, AgentEvent::TurnError("session is busy".into()));
                return;
            }
            state.turn_seq += 1;
            *state.turn_usage.lock().unwrap() = crate::agent::TurnUsage {
                calibration_factor: state.calibration_factor,
                ..crate::agent::TurnUsage::default()
            };
            let monitors: Arc<dyn MonitorBroker> = state.monitors.clone();
            let hook_state = state.hook_state.lock().unwrap().clone();
            let (cwd, target, history) = state.next_turn_context();
            (
                cwd,
                target,
                state.local_socket.clone(),
                history,
                state.asks.clone(),
                state.steer.clone(),
                state.permissions.clone(),
                monitors,
                Some(state.hooks.clone()),
                hook_state,
                Some(Arc::clone(&state.hook_state)),
                state.turn_seq,
                state.max_steps,
                Arc::clone(&state.turn_usage),
            )
        };

        let cfg = TurnConfig {
            data_dir: self.data_dir.clone(),
            conduit: self.conduit.clone(),
            local_socket,
            local_server: self.local_server.clone(),
            target,
            cwd,
            provider,
            model,
            history,
            asks,
            steer,
            permissions,
            monitors: Some(monitors),
            mcp_pool: Some(Arc::clone(&self.mcp_pool)),
            tools: None,
            providers: None,
            hooks,
            hook_state,
            hook_state_sink,
            turn_seq,
            max_steps,
            usage: Some(usage),
        };
        debug!(session = %id, turn_seq, "turn start");
        let turn = tokio::spawn(Arc::clone(self).run_turn_with_hooks(
            id.to_string(),
            Arc::clone(&live),
            prompt,
            cfg,
            tx,
        ));

        let runtime = Arc::clone(self);
        let id = id.to_string();
        let pump_live = Arc::clone(&live);
        let pump = tokio::spawn(async move {
            loop {
                let event = match rx.recv().await {
                    Some(event) => event,
                    None => {
                        debug!(session = %id, "turn channel closed without a terminal event");
                        let mut state = pump_live.lock().unwrap();
                        state.release();
                        state.active_checkpoint_id = None;
                        state.active_job = None;
                        state.queued.clear();
                        runtime.publish(
                            &mut state,
                            AgentEvent::TurnError("agent loop ended unexpectedly".into()),
                        );
                        return;
                    }
                };
                debug!(session = %id, event = ?std::mem::discriminant(&event), "turn event");
                let finished = matches!(
                    event,
                    AgentEvent::TurnDone { .. } | AgentEvent::TurnError(_)
                );
                let checkpoint_flush = {
                    let mut state = pump_live.lock().unwrap();
                    let mut flush = None;
                    match &event {
                        AgentEvent::TurnDone { messages, .. }
                        | AgentEvent::Compacted { messages, .. } => {
                            state.messages = messages.clone();
                        }
                        AgentEvent::FileChanged {
                            path,
                            before,
                            existed_before,
                            restorable_before,
                            ..
                        } => {
                            if let Some(cp_id) = state.active_checkpoint_id {
                                if state.checkpoints.note_file_change(
                                    cp_id,
                                    path.clone(),
                                    *existed_before,
                                    before.clone(),
                                    *restorable_before,
                                ) {
                                    flush = state.checkpoints.queued_write().ok();
                                }
                            }
                        }
                        AgentEvent::ToolJob { job_id, .. } => {
                            state.active_job = Some(*job_id);
                        }
                        AgentEvent::ToolDone { .. } => {
                            state.active_job = None;
                        }
                        _ => {}
                    }
                    runtime.publish(&mut state, event);
                    if finished {
                        let usage = state.turn_usage.lock().unwrap().clone();
                        if (0.65..=1.65).contains(&usage.calibration_factor) {
                            state.calibration_factor = usage.calibration_factor;
                        }
                        let snapshot = SessionSnapshot {
                            cwd: state.cwd.clone(),
                            target: state.target.clone(),
                            messages: state.messages.clone(),
                            usage,
                        };
                        state.release();
                        state.active_checkpoint_id = None;
                        state.active_job = None;
                        let next = state.queued.pop_front();
                        let continued = match next {
                            Some(next) if state.claim(LivePhase::RunningTurn) => Some(next),
                            Some(next) => {
                                state.queued.push_front(next);
                                None
                            }
                            None => None,
                        };
                        drop(state);
                        if let Some((path, bytes)) = flush {
                            let _ = CheckpointStore::write_queued(&path, &bytes);
                        }
                        persist_daemon_session(&runtime.data_dir, &id, snapshot);
                        if let Some(next) = continued {
                            runtime.start_turn(&id, Arc::clone(&pump_live), Some(next));
                        }
                        return;
                    }
                    flush
                };
                if let Some((path, bytes)) = checkpoint_flush {
                    let _ = CheckpointStore::write_queued(&path, &bytes);
                }
            }
        });

        let mut state = live.lock().unwrap();
        state.turn = Some(turn);
        state.pump = Some(pump);
    }

    pub fn send(self: &Arc<Self>, id: &str, text: String, mode: SendMode) {
        self.send_with_images(id, text, Vec::new(), mode);
    }

    pub fn send_with_images(
        self: &Arc<Self>,
        id: &str,
        text: String,
        images: Vec<crate::session::ImageAttachment>,
        mode: SendMode,
    ) {
        let Some(live) = self.session(id) else {
            debug!(session = id, "send for an unknown session");
            return;
        };
        enum Dispatch {
            Turn(String),
            Side {
                text: String,
                images: Vec<crate::session::ImageAttachment>,
                queue: bool,
            },
        }
        let dispatch = {
            let mut state = live.lock().unwrap();
            debug!(session = id, ?mode, running = state.running, "send");
            match mode {
                SendMode::Prompt | SendMode::Steer if state.phase == LivePhase::RunningTurn => {
                    state.steer.lock().unwrap().push(text.clone());
                    self.publish(
                        &mut state,
                        AgentEvent::UserMessage {
                            text,
                            mode: "steer".into(),
                        },
                    );
                    return;
                }
                SendMode::FollowUp if state.phase != LivePhase::Idle || state.running => {
                    state.queued.push_back(text);
                    return;
                }
                SendMode::Side if state.phase == LivePhase::Idle && !state.running => {
                    if !state.claim(LivePhase::RunningTurn) {
                        self.publish(&mut state, AgentEvent::TurnError("session is busy".into()));
                        return;
                    }
                    state.pending_turn_images = images;
                    Dispatch::Turn(text)
                }
                SendMode::Side | SendMode::SideTalk | SendMode::SideQueue => Dispatch::Side {
                    text,
                    images,
                    queue: matches!(mode, SendMode::SideQueue),
                },
                _ => {
                    if !state.claim(LivePhase::RunningTurn) {
                        self.publish(&mut state, AgentEvent::TurnError("session is busy".into()));
                        return;
                    }
                    state.pending_turn_images = images;
                    Dispatch::Turn(text)
                }
            }
        };
        match dispatch {
            Dispatch::Turn(text) => self.start_turn(id, live, Some(text)),
            Dispatch::Side {
                text,
                images,
                queue,
            } => {
                self.launch_side(id, &live, text, images, queue);
            }
        }
    }

    pub fn start_compact(
        self: &Arc<Self>,
        id: &str,
        focus: Option<String>,
        handoff: bool,
    ) -> Result<()> {
        let Some(live) = self.session(id) else {
            bail!("no such session: {id}");
        };
        let model_override = live.lock().unwrap().model_override.clone();
        let Some((provider, model)) =
            resolve_provider_with_overrides(&self.data_dir, model_override.as_deref())
        else {
            bail!("no model configured: run /login");
        };
        let (cwd, target, mut messages) = {
            let mut state = live.lock().unwrap();
            if !state.claim(LivePhase::Compacting) {
                bail!("session is busy; wait for the active turn to finish or interrupt first");
            }
            if state.messages.len() < 2 && !handoff {
                state.release();
                bail!("nothing to compact yet");
            }
            if state.messages.is_empty() && handoff {
                state.release();
                bail!("nothing to hand off yet");
            }
            self.publish(
                &mut state,
                AgentEvent::Compacting {
                    auto: false,
                    handoff,
                },
            );
            (
                state.cwd.clone(),
                state.target.clone(),
                state.messages.clone(),
            )
        };

        let runtime = Arc::clone(self);
        let data_dir = self.data_dir.clone();
        let session_id = id.to_string();
        let task_live = Arc::clone(&live);
        let task = tokio::spawn(async move {
            let res = run_manual_context_op(
                &runtime,
                &session_id,
                &task_live,
                &data_dir,
                &provider,
                &model.model_id,
                &target,
                &cwd,
                &mut messages,
                focus.as_deref(),
                handoff,
            )
            .await;

            let mut state = task_live.lock().unwrap();
            if state.phase == LivePhase::Compacting {
                state.release();
            }
            match res {
                Ok(ev) => {
                    if let AgentEvent::Compacted { messages, .. } = &ev {
                        state.messages = messages.clone();
                    }
                    runtime.publish(&mut state, ev);
                }
                Err(e) => {
                    runtime.publish(&mut state, AgentEvent::TurnError(format!("{e:#}")));
                }
            }
        });
        let mut state = live.lock().unwrap();
        state.turn = Some(task);
        Ok(())
    }

    pub fn interrupt(&self, id: &str) {
        let Some(live) = self.session(id) else {
            return;
        };
        let (turn, pump, job) = {
            let mut state = live.lock().unwrap();
            state.steer.lock().unwrap().clear();
            state.queued.clear();
            state.release();
            state.active_checkpoint_id = None;
            let job = state
                .active_job
                .take()
                .map(|job_id| (job_id, state.target.clone(), state.local_socket.clone()));
            (state.turn.take(), state.pump.take(), job)
        };
        if let Some(task) = turn {
            task.abort();
        }
        if let Some(task) = pump {
            task.abort();
        }
        if let Some((job_id, target, local_socket)) = job {
            let data_dir = self.data_dir.clone();
            let conduit = self.conduit.clone();
            let local_server = self.local_server.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    cancel_active_job(
                        &data_dir,
                        &target,
                        local_socket,
                        conduit,
                        local_server,
                        job_id,
                    )
                    .await;
                });
            }
        }
        let mut state = live.lock().unwrap();
        self.publish(
            &mut state,
            AgentEvent::TurnError("interrupted by user".into()),
        );
    }

    pub fn answer(&self, id: &str, ask_id: u64, label: String) -> bool {
        self.session(id)
            .map(|live| live.lock().unwrap().asks.answer(ask_id, label))
            .unwrap_or(false)
    }

    /// The console reports that the user is interacting with a shown prompt, so
    /// the answer clock stops instead of firing the recommended option.
    pub fn touch_ask(&self, id: &str, ask_id: u64) -> bool {
        self.session(id)
            .map(|live| live.lock().unwrap().asks.engage(ask_id))
            .unwrap_or(false)
    }

    pub fn recall(&self, id: &str) -> Option<String> {
        self.session(id)
            .and_then(|live| live.lock().unwrap().queued.pop_back())
    }

    pub fn set_permission_mode(&self, id: &str, mode: PermissionMode) {
        if let Some(live) = self.session(id) {
            let mut state = live.lock().unwrap();
            state.permissions.set_mode(mode);
            self.publish(&mut state, AgentEvent::PermissionModeChanged { mode });
        }
    }

    pub fn permissions_status(
        &self,
        id: &str,
    ) -> Result<(PermissionMode, String, bool, usize, usize)> {
        let Some(live) = self.session(id) else {
            bail!("no such session: {id}");
        };
        let state = live.lock().unwrap();
        let mode = state.permissions.mode();
        let loaded = state.permissions.loaded_policies();
        let trusted = state.permissions.is_trusted();
        let sandbox = if state.target == "local" || state.target.is_empty() {
            jade_core::sandbox::backend_label().to_string()
        } else {
            "peer".to_string()
        };
        Ok((
            mode,
            sandbox,
            trusted,
            loaded.rules.len(),
            loaded.extra_rw.len(),
        ))
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, live)| live.lock().unwrap().info(id))
            .collect()
    }
}

fn new_incarnation() -> u64 {
    let mut buf = [0u8; 8];
    if getrandom::fill(&mut buf).is_err() {
        return 1;
    }
    let id = u64::from_le_bytes(buf);
    if id == 0 {
        1
    } else {
        id
    }
}
