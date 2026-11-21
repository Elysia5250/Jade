//! Watch a plan sidechat turn and hand the plan back to the worker.
use super::live::Live;
use super::server::Runtime;
use super::side::{
    handoff_interrupts, plan_prompt, queue_plan_or_start, INTERRUPT_LABEL, WAIT_LABEL,
};
use crate::agent::{is_side_plan_path, AgentEvent, AskOption, SideNote};
use crate::session::ChatMessage;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const HANDOFF_TIMEOUT: Duration = Duration::from_secs(300);
const TEXT_FLUSH: usize = 240;

pub(crate) async fn watch_side(
    runtime: Arc<Runtime>,
    live: Arc<Mutex<Live>>,
    data_dir: PathBuf,
    cwd: PathBuf,
    plan: Arc<Mutex<Option<PathBuf>>>,
    mut rx: mpsc::UnboundedReceiver<AgentEvent>,
) {
    let mut text = String::new();
    while let Some(event) = rx.recv().await {
        if let AgentEvent::FileChanged { ref path, .. } = event {
            if is_side_plan_path(&data_dir, &cwd, path) {
                *plan.lock().unwrap() = Some(PathBuf::from(path));
            }
            let mut state = live.lock().unwrap();
            runtime.publish(&mut state, event);
            continue;
        }
        if matches!(event, AgentEvent::Ask { .. } | AgentEvent::AskClosed { .. }) {
            let mut state = live.lock().unwrap();
            runtime.publish(&mut state, event);
            continue;
        }
        match event {
            AgentEvent::TextDelta(delta) => {
                text.push_str(&delta);
                if text.chars().count() >= TEXT_FLUSH {
                    flush_text(&runtime, &live, &mut text);
                }
            }
            AgentEvent::AssistantStart { .. } => flush_text(&runtime, &live, &mut text),
            AgentEvent::ToolStart {
                call_id,
                name,
                title,
                subtitle,
            } => {
                flush_text(&runtime, &live, &mut text);
                emit(
                    &runtime,
                    &live,
                    SideNote::Tool {
                        call_id,
                        name,
                        title,
                        subtitle,
                        running: true,
                        ok: false,
                        duration_ms: 0,
                    },
                );
            }
            AgentEvent::ToolDone {
                call_id,
                ok,
                duration_ms,
                ..
            } => emit(
                &runtime,
                &live,
                SideNote::Tool {
                    call_id,
                    name: String::new(),
                    title: String::new(),
                    subtitle: String::new(),
                    running: false,
                    ok,
                    duration_ms,
                },
            ),
            AgentEvent::Say { text: said, .. } => {
                flush_text(&runtime, &live, &mut text);
                emit(&runtime, &live, SideNote::Say { text: said });
            }
            _ => {}
        }
    }
    flush_text(&runtime, &live, &mut text);
}

fn flush_text(runtime: &Runtime, live: &Mutex<Live>, text: &mut String) {
    if text.is_empty() {
        return;
    }
    let note = SideNote::Text {
        text: std::mem::take(text),
    };
    emit(runtime, live, note);
}

fn emit(runtime: &Runtime, live: &Mutex<Live>, note: SideNote) {
    let mut state = live.lock().unwrap();
    runtime.publish_side(&mut state, note);
}

impl Runtime {
    pub(crate) async fn finish_side(
        self: &Arc<Self>,
        id: &str,
        live: &Arc<Mutex<Live>>,
        messages: Option<Vec<ChatMessage>>,
        ok: bool,
        plan: Arc<Mutex<Option<PathBuf>>>,
    ) {
        let plan_path = plan.lock().unwrap().clone();
        let decision = {
            let mut state = live.lock().unwrap();
            let Some(side) = state.side.as_mut() else {
                return;
            };
            if let Some(messages) = messages {
                side.messages = messages;
            }
            if plan_path.is_some() {
                side.plan_path = plan_path.clone();
            }
            side.running = false;
            if let Some(path) = side.plan_path.clone() {
                side.running = true;
                AfterSide::Handoff(path)
            } else if let Some(text) = side.queued.pop_front() {
                AfterSide::Next(text)
            } else {
                self.publish_side(&mut state, SideNote::Done { ok });
                AfterSide::Idle
            }
        };
        match decision {
            AfterSide::Handoff(path) => self.ask_handoff(id, live, path).await,
            AfterSide::Next(text) => self.begin_side(id, live, text, Vec::new(), false, false),
            AfterSide::Idle => {}
        }
    }

    async fn ask_handoff(self: &Arc<Self>, id: &str, live: &Arc<Mutex<Live>>, path: PathBuf) {
        let prompt = plan_prompt(&path);
        let interrupts = self.ask_interrupt(live, &path).await;
        if interrupts {
            self.interrupt(id);
        }
        let start = {
            let mut state = live.lock().unwrap();
            queue_plan_or_start(&mut state, prompt)
        };
        if let Some(prompt) = start {
            self.start_turn(id, Arc::clone(live), Some(prompt));
        }
        self.close_side(live);
    }

    async fn ask_interrupt(&self, live: &Arc<Mutex<Live>>, path: &Path) -> bool {
        let asks = live.lock().unwrap().asks.clone();
        if asks.is_non_interactive() {
            return false;
        }
        let _gate = asks.acquire_prompt().await;
        let ask_id = asks.next_id();
        let (tx, mut rx) = oneshot::channel();
        asks.register(ask_id, tx);
        {
            let mut state = live.lock().unwrap();
            self.publish(
                &mut state,
                AgentEvent::Ask {
                    id: ask_id,
                    question: format!(
                        "计划已写好（{}）。要立刻打断正在跑的工人吗？",
                        path.display()
                    ),
                    options: handoff_options(),
                    recommended: 1,
                    timeout_ms: HANDOFF_TIMEOUT.as_millis() as u64,
                },
            );
        }
        let answer = asks.wait_for_answer(ask_id, HANDOFF_TIMEOUT, &mut rx).await;
        asks.forget(ask_id);
        {
            let mut state = live.lock().unwrap();
            self.publish(&mut state, AgentEvent::AskClosed { id: ask_id });
        }
        handoff_interrupts(answer.as_deref())
    }

    fn close_side(&self, live: &Arc<Mutex<Live>>) {
        let mut state = live.lock().unwrap();
        state.side = None;
        self.publish_side(&mut state, SideNote::Closed);
    }
}

enum AfterSide {
    Handoff(PathBuf),
    Next(String),
    Idle,
}

fn handoff_options() -> Vec<AskOption> {
    vec![
        AskOption {
            label: INTERRUPT_LABEL.into(),
            description: Some("停掉当前回合，马上按计划做".into()),
            recommended: false,
        },
        AskOption {
            label: WAIT_LABEL.into(),
            description: Some("当前回合做完后再按计划做".into()),
            recommended: true,
        },
    ]
}
