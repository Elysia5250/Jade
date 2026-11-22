//! Plan sidechat beside the worker turn.
//!
//! The side thread has its own messages and steer queue. It cannot claim
//! `LivePhase::RunningTurn`, and it does not steer, queue, or interrupt the
//! worker until a plan file exists and the user answers the handoff ask.
use super::live::{Live, LivePhase};
use super::server::Runtime;
use crate::agent::{run_side_turn, snapshot_for_side, AgentEvent, SideNote, TurnConfig};
use crate::auth::resolve_provider_with_overrides;
use crate::permissions::PermissionMode;
use crate::session::{ChatMessage, ChatRole, ImageAttachment};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub(crate) const INTERRUPT_LABEL: &str = "立刻打断";
pub(crate) const WAIT_LABEL: &str = "等这轮结束";

pub(crate) struct SideLane {
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) running: bool,
    pub(crate) steer: Arc<Mutex<Vec<String>>>,
    pub(crate) queued: std::collections::VecDeque<String>,
    pub(crate) plan_path: Option<PathBuf>,
    task: Option<JoinHandle<()>>,
}

impl Drop for SideLane {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl SideLane {
    fn new(steer: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            messages: Vec::new(),
            running: false,
            steer,
            queued: std::collections::VecDeque::new(),
            plan_path: None,
            task: None,
        }
    }
}

pub(crate) fn handoff_interrupts(label: Option<&str>) -> bool {
    label == Some(INTERRUPT_LABEL)
}

pub(crate) fn plan_prompt(path: &Path) -> String {
    format!("先读这份计划再做：{}", path.display())
}

/// Atomically claim the worker turn if idle, or queue the plan behind it.
pub(crate) fn queue_plan_or_start(state: &mut Live, prompt: String) -> Option<String> {
    if state.phase == LivePhase::Idle && !state.running && state.claim(LivePhase::RunningTurn) {
        Some(prompt)
    } else {
        state.queued.push_back(prompt);
        None
    }
}

impl Runtime {
    pub(crate) fn launch_side(
        self: &Arc<Self>,
        id: &str,
        live: &Arc<Mutex<Live>>,
        text: String,
        images: Vec<ImageAttachment>,
        queue: bool,
    ) {
        self.begin_side(id, live, text, images, queue, true);
    }

    pub(crate) fn begin_side(
        self: &Arc<Self>,
        id: &str,
        live: &Arc<Mutex<Live>>,
        text: String,
        images: Vec<ImageAttachment>,
        queue: bool,
        announce: bool,
    ) {
        if text.trim().is_empty() && images.is_empty() {
            return;
        }
        let prepared = {
            let mut state = live.lock().unwrap();
            if side_is_running(&state) {
                park_on_running_side(&mut state, &text, queue);
                if announce {
                    self.publish_side(&mut state, SideNote::User { text });
                }
                return;
            }
            self.prepare_side(&mut state, text, images, announce)
        };
        self.spawn_side(id, live, prepared);
    }

    fn prepare_side(
        &self,
        state: &mut Live,
        text: String,
        images: Vec<ImageAttachment>,
        announce: bool,
    ) -> PreparedSide {
        let mut history = match &state.side {
            Some(side) => side.messages.clone(),
            None => snapshot_for_side(&state.messages),
        };
        history.push(user_message(text.clone(), images));
        let steer = state
            .side
            .as_ref()
            .map(|side| Arc::clone(&side.steer))
            .unwrap_or_else(|| Arc::new(Mutex::new(Vec::new())));
        let permissions =
            state
                .permissions
                .fork_for_subagent(PermissionMode::Plan, false, state.cwd.clone());
        if state.side.is_none() {
            state.side = Some(SideLane::new(Arc::clone(&steer)));
        }
        let side = state.side.as_mut().unwrap();
        side.messages = history.clone();
        side.running = true;
        if announce {
            self.publish_side(state, SideNote::User { text });
        }
        PreparedSide {
            history,
            steer,
            permissions,
            cwd: state.cwd.clone(),
            target: state.target.clone(),
            local_socket: state.local_socket.clone(),
            hooks: state.hooks.clone(),
            hook_state: state.hook_state.lock().unwrap().clone(),
            asks: Arc::clone(&state.asks),
            model_override: state.model_override.clone(),
        }
    }

    fn spawn_side(self: &Arc<Self>, id: &str, live: &Arc<Mutex<Live>>, prepared: PreparedSide) {
        let Some((provider, model)) =
            resolve_provider_with_overrides(&self.data_dir, prepared.model_override.as_deref())
        else {
            self.fail_side(live, "no model configured: run /login");
            return;
        };
        let cfg = TurnConfig {
            data_dir: self.data_dir.clone(),
            topaz: self.topaz.clone(),
            local_socket: prepared.local_socket,
            local_server: self.local_server.clone(),
            target: prepared.target.clone(),
            cwd: prepared.cwd.clone(),
            provider,
            model,
            history: prepared.history,
            asks: prepared.asks,
            steer: prepared.steer,
            permissions: prepared.permissions,
            monitors: None,
            mcp_pool: None,
            tools: None,
            providers: None,
            hooks: Some(prepared.hooks),
            hook_state: prepared.hook_state,
            hook_state_sink: None,
            turn_seq: 0,
            max_steps: Some(40),
            usage: None,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let runtime = Arc::clone(self);
        let task_live = Arc::clone(live);
        let session_id = id.to_string();
        let plan = Arc::new(Mutex::new(None));
        let data_dir = self.data_dir.clone();
        let cwd = prepared.cwd.clone();
        let watched = Arc::clone(&plan);
        let task = tokio::spawn(async move {
            let run = run_side_turn(cfg, tx);
            let watch = super::side_watch::watch_side(
                runtime.clone(),
                task_live.clone(),
                data_dir,
                cwd,
                watched,
                rx,
            );
            let (outcome, _) = tokio::join!(run, watch);
            let (messages, ok) = match outcome {
                Ok(item) => (Some(item.messages), true),
                Err(_) => (None, false),
            };
            runtime
                .finish_side(&session_id, &task_live, messages, ok, plan)
                .await;
        });
        if let Some(side) = live.lock().unwrap().side.as_mut() {
            side.task = Some(task);
        } else {
            task.abort();
        }
    }

    fn fail_side(&self, live: &Arc<Mutex<Live>>, reason: &str) {
        let mut state = live.lock().unwrap();
        if let Some(side) = state.side.as_mut() {
            side.running = false;
        }
        self.publish_side(
            &mut state,
            SideNote::Say {
                text: reason.to_string(),
            },
        );
        self.publish_side(&mut state, SideNote::Done { ok: false });
    }

    pub(crate) fn publish_side(&self, state: &mut Live, note: SideNote) {
        self.publish(state, AgentEvent::Side { note });
    }
}

struct PreparedSide {
    history: Vec<ChatMessage>,
    steer: Arc<Mutex<Vec<String>>>,
    permissions: Arc<crate::permissions::PermissionEngine>,
    cwd: PathBuf,
    target: String,
    local_socket: Option<PathBuf>,
    hooks: Arc<crate::hooks::HookEngine>,
    hook_state: std::collections::BTreeMap<String, serde_json::Value>,
    asks: Arc<crate::agent::AskBroker>,
    model_override: Option<String>,
}

fn side_is_running(state: &Live) -> bool {
    state.side.as_ref().is_some_and(|side| side.running)
}

fn park_on_running_side(state: &mut Live, text: &str, queue: bool) {
    let Some(side) = state.side.as_mut() else {
        return;
    };
    if queue {
        side.queued.push_back(text.to_string());
    } else {
        side.steer.lock().unwrap().push(text.to_string());
    }
}

fn user_message(text: String, images: Vec<ImageAttachment>) -> ChatMessage {
    let mut message = ChatMessage::text(ChatRole::User, text);
    message.images = images;
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::SendMode;
    use std::time::Duration;

    #[test]
    fn only_the_interrupt_label_stops_the_worker() {
        assert!(handoff_interrupts(Some(INTERRUPT_LABEL)));
        assert!(!handoff_interrupts(Some(WAIT_LABEL)));
        assert!(!handoff_interrupts(None));
    }

    #[test]
    fn a_running_worker_keeps_the_plan_queued() {
        let runtime = Runtime::new(PathBuf::from("/tmp"));
        let live = runtime.ensure(
            "s-plan".into(),
            PathBuf::from("/tmp"),
            "local".into(),
            Vec::new(),
        );
        {
            let mut state = live.lock().unwrap();
            state.phase = LivePhase::RunningTurn;
            state.running = true;
            assert!(queue_plan_or_start(&mut state, "先读这份计划再做：plan.md".into()).is_none());
            assert_eq!(state.queued.len(), 1);
        }
    }

    #[tokio::test]
    async fn side_send_does_not_steer_or_queue_the_worker() {
        let runtime = Runtime::new(PathBuf::from("/tmp"));
        let live = runtime.ensure(
            "s-side".into(),
            PathBuf::from("/tmp"),
            "local".into(),
            vec![ChatMessage::text(ChatRole::User, "worker")],
        );
        {
            let mut state = live.lock().unwrap();
            state.phase = LivePhase::RunningTurn;
            state.running = true;
            let mut pending = ChatMessage::text(ChatRole::Assistant, "");
            pending.tool_calls.push(crate::session::ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: "{}".into(),
            });
            state.messages.push(pending);
        }
        runtime.send("s-side", "plan this".into(), SendMode::Side);
        {
            let mut state = live.lock().unwrap();
            let side = state.side.as_mut().unwrap();
            if let Some(task) = side.task.take() {
                task.abort();
            }
            side.running = true;
        }
        runtime.send("s-side", "later".into(), SendMode::SideQueue);
        runtime.send("s-side", "fix the name".into(), SendMode::Steer);
        let state = live.lock().unwrap();
        assert_eq!(state.steer.lock().unwrap().as_slice(), ["fix the name"]);
        assert!(state.queued.is_empty());
        let side = state.side.as_ref().unwrap();
        assert_eq!(side.queued.front().map(String::as_str), Some("later"));
        assert!(side.messages.iter().all(|msg| msg.tool_calls.is_empty()));
        assert_eq!(side.messages[0].content, "worker");
        assert_eq!(side.messages[1].content, "plan this");
    }

    fn busy_side(runtime: &Arc<Runtime>, id: &str) -> Arc<Mutex<Live>> {
        let live = runtime.ensure(id.into(), PathBuf::from("/tmp"), "local".into(), Vec::new());
        let mut state = live.lock().unwrap();
        state.phase = LivePhase::RunningTurn;
        state.running = true;
        state.steer.lock().unwrap().push("keep".into());
        state.side = Some(SideLane::new(Arc::new(Mutex::new(Vec::new()))));
        state.side.as_mut().unwrap().running = true;
        drop(state);
        live
    }

    #[tokio::test]
    async fn no_plan_file_leaves_the_worker_alone() {
        let runtime = Runtime::new(PathBuf::from("/tmp"));
        let live = busy_side(&runtime, "s-none");
        runtime
            .finish_side(
                "s-none",
                &live,
                Some(vec![ChatMessage::text(ChatRole::User, "plan this")]),
                true,
                Arc::new(Mutex::new(None)),
            )
            .await;
        let state = live.lock().unwrap();
        assert_eq!(state.phase, LivePhase::RunningTurn);
        assert!(state.running);
        assert_eq!(state.steer.lock().unwrap().as_slice(), ["keep"]);
        assert!(state.queued.is_empty());
        let side = state.side.as_ref().unwrap();
        assert!(side.plan_path.is_none());
        assert!(!side.running);
        assert!(state.events.iter().any(|event| {
            matches!(
                event.event,
                AgentEvent::Side {
                    note: SideNote::Done { ok: true }
                }
            )
        }));
        assert!(!state
            .events
            .iter()
            .any(|event| matches!(event.event, AgentEvent::Ask { .. })));
    }

    async fn answer_handoff(
        runtime: &Arc<Runtime>,
        live: &Arc<Mutex<Live>>,
        id: &str,
        label: &str,
    ) {
        let run = {
            let runtime = Arc::clone(runtime);
            let live = Arc::clone(live);
            let id = id.to_string();
            let plan = Arc::new(Mutex::new(Some(PathBuf::from(
                "/tmp/work/.jade/plans/next.md",
            ))));
            tokio::spawn(async move {
                runtime
                    .finish_side(
                        &id,
                        &live,
                        Some(vec![ChatMessage::text(ChatRole::User, "plan this")]),
                        true,
                        plan,
                    )
                    .await;
            })
        };
        let ask_id = loop {
            let found = live
                .lock()
                .unwrap()
                .events
                .iter()
                .find_map(|event| match &event.event {
                    AgentEvent::Ask { id, question, .. } => Some((*id, question.clone())),
                    _ => None,
                });
            if let Some((ask_id, question)) = found {
                assert!(question.contains("计划已写好"));
                assert!(question.contains(".jade/plans/next.md"));
                break ask_id;
            }
            if run.is_finished() {
                panic!("handoff ended before the ask");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(runtime.answer(id, ask_id, label.into()));
        run.await.unwrap();
    }

    #[tokio::test]
    async fn waiting_queues_the_plan_and_closes_the_sidechat() {
        let runtime = Runtime::new(PathBuf::from("/tmp"));
        let live = busy_side(&runtime, "s-wait");
        answer_handoff(&runtime, &live, "s-wait", WAIT_LABEL).await;
        let state = live.lock().unwrap();
        assert_eq!(state.phase, LivePhase::RunningTurn);
        assert!(state.running);
        assert_eq!(state.steer.lock().unwrap().as_slice(), ["keep"]);
        assert_eq!(state.queued.len(), 1);
        assert!(state.queued[0].contains(".jade/plans/next.md"));
        assert!(state.side.is_none());
        assert!(state.events.iter().any(|event| {
            matches!(
                event.event,
                AgentEvent::Side {
                    note: SideNote::Closed
                }
            )
        }));
        assert!(!state
            .events
            .iter()
            .any(|event| matches!(event.event, AgentEvent::UserMessage { .. })));
    }

    #[tokio::test]
    async fn interrupting_stops_the_worker_and_starts_the_plan() {
        let runtime = Runtime::new(PathBuf::from("/tmp"));
        let live = busy_side(&runtime, "s-stop");
        live.lock().unwrap().queued.push_back("later".into());
        answer_handoff(&runtime, &live, "s-stop", INTERRUPT_LABEL).await;
        let state = live.lock().unwrap();
        assert_eq!(state.phase, LivePhase::Idle);
        assert!(!state.running);
        assert!(state.steer.lock().unwrap().is_empty());
        assert!(state.queued.is_empty());
        assert!(state.side.is_none());
        assert!(state.events.iter().any(|event| {
            matches!(&event.event, AgentEvent::TurnError(msg) if msg == "interrupted by user")
        }));
        assert!(state.events.iter().any(|event| {
            matches!(&event.event, AgentEvent::UserMessage { text, .. } if text.contains("next.md"))
        }));
    }

    #[tokio::test]
    async fn queue_plan_or_start_claims_turn_atomically_and_queues_second() {
        let runtime = Runtime::new(PathBuf::from("/tmp"));
        let live = runtime.ensure(
            "s-claim".into(),
            PathBuf::from("/tmp"),
            "local".into(),
            Vec::new(),
        );
        let mut state = live.lock().unwrap();
        let first = queue_plan_or_start(&mut state, "plan-1".into());
        assert_eq!(first.as_deref(), Some("plan-1"));
        assert_eq!(state.phase, LivePhase::RunningTurn);
        assert!(state.running);

        let second = queue_plan_or_start(&mut state, "plan-2".into());
        assert!(second.is_none());
        assert_eq!(state.queued.front().map(String::as_str), Some("plan-2"));
    }
}
