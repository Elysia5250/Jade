//! Daemon-side session store, turn runner, and manual `/compact` + `/handoff`.

use super::hooks::{new_hook_engine, run_manual_context_op};
use super::monitors::MonitorHub;
use super::ops::seed_checkpoint_user_turns;
use super::protocol::{SendMode, SeqEvent, SessionInfo};
use crate::agent::checkpoint::CheckpointStore;
use crate::agent::MonitorBroker;
use crate::auth::resolve_provider_with_overrides;
use crate::hooks::HookEngine;
use crate::llm::{AgentEvent, AskBroker, TurnConfig};
use crate::permissions::{PermissionEngine, PermissionMode};
use crate::session::ChatMessage;
use anyhow::{bail, Result};

use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::debug;

pub(crate) struct Live {
    pub(crate) cwd: PathBuf,
    pub(crate) target: String,
    pub(crate) local_socket: Option<PathBuf>,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) events: Vec<SeqEvent>,
    pub(crate) seq: u64,
    pub(crate) running: bool,
    queued: VecDeque<String>,
    pub(crate) steer: Arc<Mutex<Vec<String>>>,
    pub(crate) asks: Arc<AskBroker>,
    pub(crate) permissions: Arc<PermissionEngine>,
    pub(crate) monitors: Arc<MonitorHub>,
    pub(crate) hooks: Arc<HookEngine>,
    pub(crate) hook_state: Arc<Mutex<BTreeMap<String, Value>>>,
    pub(crate) pending_session_context: Vec<String>,
    pub(crate) turn_seq: u64,
    pub(crate) session_started: bool,
    pub(crate) session_ended: bool,
    pub(crate) checkpoints: CheckpointStore,
    pub(crate) active_checkpoint_id: Option<u64>,
    pub(crate) user_turns: usize,
    pub(crate) model_override: Option<String>,
    pub(crate) max_steps: Option<usize>,
    pub(crate) subs: Vec<mpsc::UnboundedSender<SeqEvent>>,
    turn: Option<tokio::task::JoinHandle<()>>,
    pump: Option<tokio::task::JoinHandle<()>>,
}

impl Live {
    fn new(
        runtime: &Arc<Runtime>,
        data_dir: &Path,
        id: &str,
        cwd: PathBuf,
        target: String,
        messages: Vec<ChatMessage>,
    ) -> Self {
        let permissions = PermissionEngine::new(
            data_dir.to_path_buf(),
            cwd.clone(),
            id.to_string(),
            target.clone(),
            PermissionMode::Default,
        );
        let monitors = MonitorHub::new(
            id.to_string(),
            data_dir.to_path_buf(),
            Arc::downgrade(runtime),
        );
        let hooks = new_hook_engine(data_dir, &cwd);
        let checkpoints = CheckpointStore::load(data_dir.to_path_buf(), id.to_string());
        let user_turns = seed_checkpoint_user_turns(&checkpoints, &messages);
        Self {
            cwd,
            target,
            local_socket: None,
            messages,
            events: Vec::new(),
            seq: 0,
            running: false,
            queued: VecDeque::new(),
            steer: Arc::new(Mutex::new(Vec::new())),
            asks: AskBroker::new(),
            permissions,
            monitors,
            hooks,
            hook_state: Arc::new(Mutex::new(BTreeMap::new())),
            pending_session_context: Vec::new(),
            turn_seq: 0,
            session_started: false,
            session_ended: false,
            checkpoints,
            active_checkpoint_id: None,
            user_turns,
            model_override: None,
            max_steps: None,
            subs: Vec::new(),
            turn: None,
            pump: None,
        }
    }

    pub(crate) fn subscribe(&mut self) -> mpsc::UnboundedReceiver<SeqEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subs.push(tx);
        rx
    }

    fn info(&self, id: &str) -> SessionInfo {
        SessionInfo {
            id: id.to_string(),
            cwd: self.cwd.clone(),
            target: self.target.clone(),
            running: self.running,
            queued: self.queued.len(),
        }
    }
}

pub struct Runtime {
    pub(crate) data_dir: PathBuf,
    sessions: Mutex<HashMap<String, Arc<Mutex<Live>>>>,
}

impl Runtime {
    pub fn new(data_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            data_dir,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn session(&self, id: &str) -> Option<Arc<Mutex<Live>>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub(crate) fn remove_session(&self, id: &str) -> Option<Arc<Mutex<Live>>> {
        self.sessions.lock().unwrap().remove(id)
    }

    pub(crate) fn drain_sessions(&self) -> Vec<(String, Arc<Mutex<Live>>)> {
        self.sessions.lock().unwrap().drain().collect()
    }

    pub(crate) fn ensure(
        self: &Arc<Self>,
        id: String,
        cwd: PathBuf,
        target: String,
        history: Vec<ChatMessage>,
    ) -> Arc<Mutex<Live>> {
        let mut map = self.sessions.lock().unwrap();
        let data_dir = &self.data_dir;
        map.entry(id.clone())
            .or_insert_with(|| {
                let mut live = Live::new(self, data_dir, &id, cwd, target, history);
                live.seq = 1;
                live.events.push(SeqEvent {
                    seq: 1,
                    event: AgentEvent::SessionReset,
                });
                Arc::new(Mutex::new(live))
            })
            .clone()
    }

    pub(crate) fn publish(&self, live: &mut Live, event: AgentEvent) {
        live.seq += 1;
        let seq = live.seq;
        let item = SeqEvent { seq, event };
        live.events.push(item.clone());
        if live.events.len() > 20_000 {
            live.events.drain(..5_000);
        }
        live.subs.retain(|tx| tx.send(item.clone()).is_ok());
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
            state.running = false;
            self.publish(
                &mut state,
                AgentEvent::TurnError("no model configured: run /login".into()),
            );
            return;
        };
        if model.model_id.trim().is_empty() {
            let mut state = live.lock().unwrap();
            state.running = false;
            self.publish(
                &mut state,
                AgentEvent::TurnError("no model selected: pass --model or set ENVOY_MODEL".into()),
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
        ) = {
            let mut state = live.lock().unwrap();
            state.running = true;
            state.turn_seq += 1;
            let monitors: Arc<dyn MonitorBroker> = state.monitors.clone();
            let hook_state = state.hook_state.lock().unwrap().clone();
            (
                state.cwd.clone(),
                state.target.clone(),
                state.local_socket.clone(),
                state.messages.clone(),
                state.asks.clone(),
                state.steer.clone(),
                state.permissions.clone(),
                monitors,
                Some(state.hooks.clone()),
                hook_state,
                Some(Arc::clone(&state.hook_state)),
                state.turn_seq,
                state.max_steps,
            )
        };

        let cfg = TurnConfig {
            data_dir: self.data_dir.clone(),
            conduit: None,
            local_socket,
            target,
            cwd,
            provider,
            model,
            history,
            asks,
            steer,
            permissions,
            monitors: Some(monitors),
            hooks,
            hook_state,
            hook_state_sink,
            turn_seq,
            max_steps,
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
                        state.running = false;
                        state.active_checkpoint_id = None;
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
                let mut state = pump_live.lock().unwrap();
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
                            state.checkpoints.record_file_change(
                                cp_id,
                                path.clone(),
                                *existed_before,
                                before.clone(),
                                *restorable_before,
                            );
                        }
                    }
                    _ => {}
                }
                runtime.publish(&mut state, event);
                if finished {
                    state.running = false;
                    state.active_checkpoint_id = None;
                    let next = state.queued.pop_front();
                    drop(state);
                    if let Some(next) = next {
                        runtime.start_turn(&id, Arc::clone(&pump_live), Some(next));
                    }
                    return;
                }
            }
        });

        let mut state = live.lock().unwrap();
        state.turn = Some(turn);
        state.pump = Some(pump);
    }

    pub fn send(self: &Arc<Self>, id: &str, text: String, mode: SendMode) {
        let Some(live) = self.session(id) else {
            debug!(session = id, "send for an unknown session");
            return;
        };
        let start = {
            let mut state = live.lock().unwrap();
            debug!(session = id, ?mode, running = state.running, "send");
            match mode {
                SendMode::Prompt | SendMode::Steer if state.running => {
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
                SendMode::FollowUp if state.running => {
                    state.queued.push_back(text);
                    return;
                }
                _ => text,
            }
        };
        self.start_turn(id, live, Some(start));
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
            if state.running {
                bail!("session is busy; wait for the active turn to finish or interrupt first");
            }
            if state.messages.len() < 2 && !handoff {
                bail!("nothing to compact yet");
            }
            if state.messages.is_empty() && handoff {
                bail!("nothing to hand off yet");
            }
            state.running = true;
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
            state.running = false;
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
        let (turn, pump) = {
            let mut state = live.lock().unwrap();
            state.steer.lock().unwrap().clear();
            state.queued.clear();
            state.running = false;
            state.active_checkpoint_id = None;
            (state.turn.take(), state.pump.take())
        };
        if let Some(task) = turn {
            task.abort();
        }
        if let Some(task) = pump {
            task.abort();
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

    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, live)| live.lock().unwrap().info(id))
            .collect()
    }
}
