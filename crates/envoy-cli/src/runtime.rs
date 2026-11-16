//! Daemon-side single-loop agent sessions.
//!
//! The console is a client: it sends prompts/steers/answers here and renders
//! the event stream. A turn lives in the daemon, so closing the TUI does not
//! kill it; an attach from a later TUI replays events from a cursor.
//!
//! Transport is a local Unix socket (`<data_dir>/agent.sock`, 0600) with the
//! same postcard framing as the machine protocol. This is the console control
//! plane, separate from `service_id = 16`.

use crate::auth::resolve_active_provider;
use crate::llm::{run_agent_turn, AgentEvent, AskBroker, TurnConfig};
use crate::session::ChatMessage;
use anyhow::{bail, Context, Result};
use envoy_proto::codec::{read_framed, write_framed};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tracing::debug;

pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent.sock")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendMode {
    /// Start a turn if idle, otherwise steer the running one.
    Prompt,
    /// Deliver at the next tool boundary.
    Steer,
    /// Deliver after the current run finishes.
    FollowUp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeqEvent {
    pub seq: u64,
    pub event: AgentEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentReq {
    /// Create the session if missing and sync its history.
    Ensure {
        id: String,
        cwd: PathBuf,
        target: String,
        history: Vec<ChatMessage>,
    },
    /// Ensure the session exists, then attach to its event stream. One round
    /// trip so a reconnecting console cannot race the replay.
    Resume {
        id: String,
        cwd: PathBuf,
        target: String,
        history: Vec<ChatMessage>,
        from: u64,
    },
    Send {
        id: String,
        text: String,
        mode: SendMode,
    },
    Attach {
        id: String,
        from: u64,
    },
    Interrupt {
        id: String,
    },
    Answer {
        id: String,
        ask_id: u64,
        label: String,
    },
    /// Remove the most recently queued follow-up and return it.
    Recall {
        id: String,
    },
    List,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentResp {
    /// Replay payload for an attach: everything after `from`, plus whether a
    /// turn is currently live in the daemon.
    Snapshot { events: Vec<SeqEvent>, running: bool },
    Event(SeqEvent),
    Sessions(Vec<SessionInfo>),
    /// A queued follow-up returned to the console, or `None` if the queue was
    /// empty.
    Recalled(Option<String>),
    Ack,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: PathBuf,
    pub target: String,
    pub running: bool,
    pub queued: usize,
}

struct Live {
    cwd: PathBuf,
    target: String,
    messages: Vec<ChatMessage>,
    events: Vec<SeqEvent>,
    seq: u64,
    running: bool,
    queued: VecDeque<String>,
    steer: Arc<Mutex<Vec<String>>>,
    asks: Arc<AskBroker>,
    subs: Vec<mpsc::UnboundedSender<SeqEvent>>,
    turn: Option<tokio::task::JoinHandle<()>>,
    pump: Option<tokio::task::JoinHandle<()>>,
}

impl Live {
    fn new(cwd: PathBuf, target: String, messages: Vec<ChatMessage>) -> Self {
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

    fn session(&self, id: &str) -> Option<Arc<Mutex<Live>>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    fn ensure(
        self: &Arc<Self>,
        id: String,
        cwd: PathBuf,
        target: String,
        history: Vec<ChatMessage>,
    ) -> Arc<Mutex<Live>> {
        let mut map = self.sessions.lock().unwrap();
        map.entry(id)
            .or_insert_with(|| {
                let mut live = Live::new(cwd, target, history);
                // A fresh runtime cannot know the client's old event cursor;
                // this marker tells the client to rebuild its view.
                live.seq = 1;
                live.events.push(SeqEvent {
                    seq: 1,
                    event: AgentEvent::SessionReset,
                });
                Arc::new(Mutex::new(live))
            })
            .clone()
    }

    /// Broadcast one event to every attached client and record it for replay.
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

    /// Start the next turn from `live.messages` plus one user message.
    fn start_turn(self: &Arc<Self>, id: &str, live: Arc<Mutex<Live>>, prompt: Option<String>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let (cwd, target, history, asks, steer) = {
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
                        // The loop task panicked or ended without TurnDone.
                        // Never leave the session stuck in `running`.
                        debug!(session = %id, "turn channel closed without a terminal event");
                        let mut state = pump_live.lock().unwrap();
                        state.running = false;
                        // Drop queued work rather than replay it against a
                        // transcript we never received.
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
                if let AgentEvent::TurnDone { messages, .. } = &event {
                    state.messages = messages.clone();
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
                // `Prompt` is the console's only "talk" verb: it starts a turn
                // when idle and steers the live one. The TUI uses it for every
                // enter, so a stale local `running` flag can never swallow the
                // next prompt.
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

    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, live)| live.lock().unwrap().info(id))
            .collect()
    }
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
        std::time::Duration::from_secs(5),
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
        // A cursor from before a daemon restart is meaningless: replay from
        // the beginning, including the SessionReset marker.
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

/// Open one request/response exchange with the daemon.
pub async fn request(data_dir: &Path, req: &AgentReq) -> Result<AgentResp> {
    let mut stream = tokio::net::UnixStream::connect(socket_path(data_dir))
        .await
        .with_context(|| {
            format!(
                "connecting to the agent runtime at {} (is `envoy serve` up?)",
                socket_path(data_dir).display()
            )
        })?;
    write_framed(&mut stream, req).await?;
    let resp: AgentResp = read_framed(&mut stream).await?;
    Ok(resp)
}

/// Fire-and-forget helper for send/interrupt/answer.
pub async fn tell(data_dir: &Path, req: AgentReq) -> Result<()> {
    request(data_dir, &req).await.map(|_| ()).inspect_err(|e| {
        debug!(error = %e, "agent runtime is unreachable");
    })
}

/// Attach (and create if needed) and forward the replay plus live events into
/// `tx`. Sends an `Error` when the stream ends so the console can reconnect.
pub async fn attach(
    data_dir: PathBuf,
    id: String,
    cwd: PathBuf,
    target: String,
    history: Vec<ChatMessage>,
    from: u64,
    tx: mpsc::UnboundedSender<AgentResp>,
) {
    let path = socket_path(&data_dir);
    let Ok(mut stream) = tokio::net::UnixStream::connect(&path).await else {
        let _ = tx.send(AgentResp::Error("agent runtime is not running".into()));
        return;
    };
    let req = AgentReq::Resume {
        id,
        cwd,
        target,
        history,
        from,
    };
    if write_framed(&mut stream, &req).await.is_err() {
        let _ = tx.send(AgentResp::Error("agent runtime is not accepting requests".into()));
        return;
    }
    // The first frame is the replay snapshot. Do not sit forever on a daemon
    // that accepted the socket but never answered.
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_framed::<_, AgentResp>(&mut stream),
    )
    .await
    {
        Ok(Ok(resp)) => {
            if tx.send(resp).is_err() {
                return;
            }
        }
        Ok(Err(e)) => {
            debug!(error = %e, "agent attach failed");
            let _ = tx.send(AgentResp::Error("agent event stream closed".into()));
            return;
        }
        Err(_) => {
            let _ = tx.send(AgentResp::Error(
                "agent runtime did not answer within 5s".into(),
            ));
            return;
        }
    }
    loop {
        match read_framed::<_, AgentResp>(&mut stream).await {
            Ok(resp) => {
                if tx.send(resp).is_err() {
                    return;
                }
            }
            Err(e) => {
                debug!(error = %e, "agent event stream ended");
                let _ = tx.send(AgentResp::Error("agent event stream closed".into()));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_returns_queued_follow_ups_lifo() {
        let runtime = Runtime::new(PathBuf::from("/tmp"));
        let live = runtime.ensure(
            "s1".into(),
            PathBuf::from("/tmp"),
            "local".into(),
            Vec::new(),
        );
        live.lock().unwrap().running = true;
        runtime.send("s1", "first".into(), SendMode::FollowUp);
        runtime.send("s1", "second".into(), SendMode::FollowUp);
        assert_eq!(runtime.recall("s1").as_deref(), Some("second"));
        assert_eq!(runtime.recall("s1").as_deref(), Some("first"));
        assert_eq!(runtime.recall("s1"), None);
    }
}
