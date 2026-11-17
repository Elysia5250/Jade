//! Daemon-side session store, turn runner, and manual `/compact` + `/handoff`.

use super::protocol::{AgentReq, AgentResp, SendMode, SeqEvent, SessionInfo};
use crate::agent::context::{
    generate_handoff, prepare_context, resolve_budget, tools_overhead, TokenCalibration,
};
use crate::agent::prompt::build_system_prompt;
use crate::auth::resolve_active_provider;
use crate::llm::{run_agent_turn, AgentEvent, AskBroker, TurnConfig};
use crate::permissions::{PermissionEngine, PermissionMode};
use crate::session::ChatMessage;
use anyhow::{bail, Result};
use envoy_proto::codec::{read_framed, write_framed};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tracing::debug;

pub(crate) struct Live {
    cwd: PathBuf,
    target: String,
    messages: Vec<ChatMessage>,
    events: Vec<SeqEvent>,
    seq: u64,
    pub(crate) running: bool,
    queued: VecDeque<String>,
    steer: Arc<Mutex<Vec<String>>>,
    asks: Arc<AskBroker>,
    permissions: Arc<PermissionEngine>,
    subs: Vec<mpsc::UnboundedSender<SeqEvent>>,
    turn: Option<tokio::task::JoinHandle<()>>,
    pump: Option<tokio::task::JoinHandle<()>>,
}

impl Live {
    fn new(
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
        Self {
            cwd,
            target,
            messages,
            events: Vec::new(),
            seq: 0,
            running: false,
            queued: VecDeque::new(),
            steer: Arc::new(Mutex::new(Vec::new())),
            asks: AskBroker::new(),
            permissions,
            subs: Vec::new(),
            turn: None,
            pump: None,
        }
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
    data_dir: PathBuf,
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
                let mut live = Live::new(data_dir, &id, cwd, target, history);
                live.seq = 1;
                live.events.push(SeqEvent {
                    seq: 1,
                    event: AgentEvent::SessionReset,
                });
                Arc::new(Mutex::new(live))
            })
            .clone()
    }

    fn publish(&self, live: &mut Live, event: AgentEvent) {
        live.seq += 1;
        let seq = live.seq;
        let item = SeqEvent { seq, event };
        live.events.push(item.clone());
        if live.events.len() > 20_000 {
            live.events.drain(..5_000);
        }
        live.subs.retain(|tx| tx.send(item.clone()).is_ok());
    }

    fn start_turn(self: &Arc<Self>, id: &str, live: Arc<Mutex<Live>>, prompt: Option<String>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let (cwd, target, history, asks, steer, permissions) = {
            let mut state = live.lock().unwrap();
            if let Some(text) = prompt {
                state.messages.push(ChatMessage {
                    role: crate::session::ChatRole::User,
                    content: text.clone(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                });
                self.publish(
                    &mut state,
                    AgentEvent::UserMessage {
                        text,
                        mode: "Agent".into(),
                    },
                );
            }
            state.running = true;
            (
                state.cwd.clone(),
                state.target.clone(),
                state.messages.clone(),
                state.asks.clone(),
                state.steer.clone(),
                state.permissions.clone(),
            )
        };

        let Some((provider, model)) = resolve_active_provider(&self.data_dir) else {
            let mut state = live.lock().unwrap();
            state.running = false;
            self.publish(
                &mut state,
                AgentEvent::TurnError("no model configured: run /login".into()),
            );
            return;
        };

        let cfg = TurnConfig {
            data_dir: self.data_dir.clone(),
            conduit: None,
            target,
            cwd,
            provider,
            model,
            history,
            asks,
            steer,
            permissions,
        };
        debug!(session = %id, "turn start");
        let turn = tokio::spawn(run_agent_turn(cfg, tx));

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
                    _ => {}
                }
                runtime.publish(&mut state, event);
                if finished {
                    state.running = false;
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
        let Some((provider, model)) = resolve_active_provider(&self.data_dir) else {
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
        let task_live = Arc::clone(&live);
        let task = tokio::spawn(async move {
            let res = run_manual_context_op(
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

#[allow(clippy::too_many_arguments)]
async fn run_manual_context_op(
    data_dir: &PathBuf,
    provider: &crate::auth::StoredProvider,
    model_id: &str,
    target: &str,
    cwd: &PathBuf,
    messages: &mut Vec<ChatMessage>,
    focus: Option<&str>,
    handoff: bool,
) -> Result<AgentEvent> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()?;
    let system = build_system_prompt(target, cwd);
    if handoff {
        let out = generate_handoff(&http, data_dir, provider, model_id, &system, messages, focus)
            .await?;
        return Ok(AgentEvent::HandoffReady {
            summary: out.summary,
            before_tokens: out.before_tokens,
            after_tokens: out.after_tokens,
            title: out.title,
        });
    }
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
        tools_overhead(0),
        true,
        focus,
        None,
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("conversation is too short to compact further"))?;

    Ok(AgentEvent::Compacted {
        messages: outcome.messages,
        before_tokens: outcome.before_tokens,
        after_tokens: outcome.after_tokens,
        summary: outcome.summary,
        auto: false,
    })
}

/// Serve the console control socket until cancelled.
pub async fn serve(runtime: Arc<Runtime>, listener: tokio::net::UnixListener) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                debug!(error = %e, "agent socket accept failed");
                continue;
            }
        };
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, runtime).await {
                debug!(error = %e, "agent session connection ended");
            }
        });
    }
}

async fn handle<S>(stream: S, runtime: Arc<Runtime>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let req: AgentReq = match tokio::time::timeout(
        Duration::from_secs(5),
        read_framed(&mut rd),
    )
    .await
    {
        Ok(Ok(req)) => req,
        Ok(Err(e)) => bail!("reading agent request: {e}"),
        Err(_) => bail!("no agent request within 5s"),
    };

    match req {
        AgentReq::Ensure {
            id,
            cwd,
            target,
            history,
        } => {
            runtime.ensure(id, cwd, target, history);
            write_framed(&mut wr, &AgentResp::Ack).await?;
        }
        AgentReq::Resume {
            id,
            cwd,
            target,
            history,
            from,
        } => {
            let live = runtime.ensure(id.clone(), cwd, target, history);
            stream_session(live, id, from, &mut wr).await?;
        }
        AgentReq::Send { id, text, mode } => {
            runtime.send(&id, text, mode);
            write_framed(&mut wr, &AgentResp::Ack).await?;
        }
        AgentReq::Interrupt { id } => {
            runtime.interrupt(&id);
            write_framed(&mut wr, &AgentResp::Ack).await?;
        }
        AgentReq::Answer { id, ask_id, label } => {
            runtime.answer(&id, ask_id, label);
            write_framed(&mut wr, &AgentResp::Ack).await?;
        }
        AgentReq::Recall { id } => {
            write_framed(&mut wr, &AgentResp::Recalled(runtime.recall(&id))).await?;
        }
        AgentReq::Compact { id, focus } => match runtime.start_compact(&id, focus, false) {
            Ok(()) => write_framed(&mut wr, &AgentResp::Ack).await?,
            Err(e) => write_framed(&mut wr, &AgentResp::Error(format!("{e:#}"))).await?,
        },
        AgentReq::Handoff { id, focus } => match runtime.start_compact(&id, focus, true) {
            Ok(()) => write_framed(&mut wr, &AgentResp::Ack).await?,
            Err(e) => write_framed(&mut wr, &AgentResp::Error(format!("{e:#}"))).await?,
        },
        AgentReq::SetPermissionMode { id, mode } => {
            runtime.set_permission_mode(&id, mode);
            write_framed(&mut wr, &AgentResp::Ack).await?;
        }
        AgentReq::List => {
            write_framed(&mut wr, &AgentResp::Sessions(runtime.list())).await?;
        }
        AgentReq::Attach { id, from } => {
            let Some(live) = runtime.session(&id) else {
                write_framed(
                    &mut wr,
                    &AgentResp::Error(format!("no such session: {id}")),
                )
                .await?;
                return Ok(());
            };
            stream_session(live, id, from, &mut wr).await?;
        }
    }
    Ok(())
}

async fn stream_session<W>(
    live: Arc<Mutex<Live>>,
    id: String,
    from: u64,
    wr: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let (snapshot, running, mut rx) = {
        let mut state = live.lock().unwrap();
        debug!(session = %id, from, seq = state.seq, "attach");
        let from = if from > state.seq { 0 } else { from };
        let snapshot: Vec<SeqEvent> = state
            .events
            .iter()
            .filter(|e| e.seq > from)
            .cloned()
            .collect();
        let running = state.running;
        let (tx, rx) = mpsc::unbounded_channel();
        state.subs.push(tx);
        (snapshot, running, rx)
    };
    write_framed(wr, &AgentResp::Snapshot { events: snapshot, running }).await?;
    while let Some(event) = rx.recv().await {
        if write_framed(wr, &AgentResp::Event(event)).await.is_err() {
            break;
        }
    }
    Ok(())
}
