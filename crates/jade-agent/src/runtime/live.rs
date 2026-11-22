//! Session phase machine. Claim a phase under the lock, then do disk work outside it.

use super::hooks::new_hook_engine;
use super::monitors::MonitorHub;
use super::ops::seed_checkpoint_user_turns;
use super::protocol::{SeqEvent, SessionInfo};
use super::server::Runtime;
use crate::agent::checkpoint::CheckpointStore;
use crate::agent::AgentEvent;
use crate::agent::AskBroker;
use crate::hooks::HookEngine;
use crate::permissions::{PermissionEngine, PermissionMode};
use crate::session::{ChatMessage, ImageAttachment};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;

const SUB_BOUND: usize = 1024;
const MAX_IDLE_SESSIONS: usize = 32;
const EVENT_ITEM_CAP: usize = 20_000;
const EVENT_DRAIN: usize = 5_000;
/// Text kept on `FileChanged` / summaries inside the replay ring.
pub(crate) const TEXT_CAP: usize = 4 * 1024;
/// Byte ceiling for `Live::events`, in addition to the item cap.
pub(crate) const EVENT_BYTE_CAP: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LivePhase {
    Idle,
    Initializing,
    RunningTurn,
    Compacting,
    Rewinding,
}

/// Releases a claimed phase when the owning task ends, including cancellation.
/// A newer claim is left alone.
pub(crate) struct PhaseGuard {
    live: Arc<Mutex<Live>>,
    phase: LivePhase,
}

impl PhaseGuard {
    pub(crate) fn new(live: Arc<Mutex<Live>>, phase: LivePhase) -> Self {
        Self { live, phase }
    }
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.live.lock() {
            if state.phase == self.phase {
                state.release();
            }
        }
    }
}

pub struct Live {
    pub(crate) cwd: PathBuf,
    pub(crate) target: String,
    pub local_socket: Option<PathBuf>,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) calibration_factor: f64,
    pub(crate) events: Vec<SeqEvent>,
    event_sizes: Vec<usize>,
    pub(crate) event_bytes: usize,
    pub(crate) seq: u64,
    pub(crate) running: bool,
    pub(crate) phase: LivePhase,
    pub(crate) turn_usage: Arc<Mutex<crate::agent::TurnUsage>>,
    pub(crate) active_job: Option<u64>,
    pub(crate) last_used: Instant,
    pub(crate) queued: VecDeque<String>,
    pub(crate) steer: Arc<Mutex<Vec<String>>>,
    pub asks: Arc<AskBroker>,
    pub permissions: Arc<PermissionEngine>,
    pub monitors: Arc<MonitorHub>,
    pub(crate) hooks: Arc<HookEngine>,
    pub(crate) hook_state: Arc<Mutex<BTreeMap<String, Value>>>,
    pub(crate) pending_session_context: Vec<String>,
    pub(crate) pending_turn_images: Vec<ImageAttachment>,
    pub(crate) turn_seq: u64,
    pub(crate) session_started: bool,
    pub(crate) session_ended: bool,
    pub(crate) checkpoints: CheckpointStore,
    pub(crate) active_checkpoint_id: Option<u64>,
    pub(crate) user_turns: usize,
    pub model_override: Option<String>,
    pub max_steps: Option<usize>,
    pub(crate) subs: Vec<mpsc::Sender<SeqEvent>>,
    pub(crate) turn: Option<tokio::task::JoinHandle<()>>,
    pub(crate) pump: Option<tokio::task::JoinHandle<()>>,
    /// Plan conversation beside the worker. It never claims [`LivePhase::RunningTurn`].
    pub(crate) side: Option<super::side::SideLane>,
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(turn) = self.turn.take() {
            turn.abort();
        }
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
    }
}

impl Live {
    pub(crate) fn new(
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
            runtime.topaz.clone(),
            runtime.local_server.clone(),
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
            calibration_factor: 1.0,
            events: Vec::new(),
            event_sizes: Vec::new(),
            event_bytes: 0,
            seq: 0,
            running: false,
            phase: LivePhase::Idle,
            turn_usage: Arc::new(Mutex::new(crate::agent::TurnUsage::default())),
            active_job: None,
            last_used: Instant::now(),
            queued: VecDeque::new(),
            steer: Arc::new(Mutex::new(Vec::new())),
            asks: AskBroker::new(),
            permissions,
            monitors,
            hooks,
            hook_state: Arc::new(Mutex::new(BTreeMap::new())),
            pending_session_context: Vec::new(),
            pending_turn_images: Vec::new(),
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
            side: None,
        }
    }

    /// Fields [`super::server::Runtime::start_turn`] copies into the next turn.
    pub(crate) fn next_turn_context(&self) -> (PathBuf, String, Vec<ChatMessage>) {
        (self.cwd.clone(), self.target.clone(), self.messages.clone())
    }

    /// Store a slim copy in the ring. Subscribers still receive the full event.
    pub(crate) fn record_logged(&mut self, seq: u64, event: &AgentEvent) {
        let stored = SeqEvent {
            seq,
            event: slim_event(event),
        };
        let Ok(bytes) = jade_proto::codec::payload_len(&stored) else {
            return;
        };
        self.events.push(stored);
        self.event_sizes.push(bytes);
        self.event_bytes += bytes;
        self.evict_log();
    }

    fn evict_log(&mut self) {
        if self.events.len() > EVENT_ITEM_CAP {
            self.drop_front(EVENT_DRAIN);
        }
        while self.event_bytes > EVENT_BYTE_CAP && self.events.len() > 1 {
            self.drop_front(1);
        }
    }

    fn drop_front(&mut self, n: usize) {
        let n = n.min(self.events.len());
        if n == 0 {
            return;
        }
        let drained: usize = self.event_sizes.drain(..n).sum();
        self.events.drain(..n);
        self.event_bytes = self.event_bytes.saturating_sub(drained);
    }

    pub fn snapshot_progress(&self) -> (Vec<ChatMessage>, crate::agent::TurnUsage) {
        (
            self.messages.clone(),
            self.turn_usage.lock().unwrap().clone(),
        )
    }

    pub fn subscribe(&mut self) -> mpsc::Receiver<SeqEvent> {
        let (tx, rx) = mpsc::channel(SUB_BOUND);
        self.subs.push(tx);
        rx
    }

    /// `running == true` with `Idle` is the state tests use to simulate a busy
    /// session without starting a turn. Treat it as occupied.
    pub(crate) fn claim(&mut self, phase: LivePhase) -> bool {
        if self.phase != LivePhase::Idle || self.running {
            return false;
        }
        self.phase = phase;
        self.running = matches!(
            phase,
            LivePhase::RunningTurn | LivePhase::Compacting | LivePhase::Rewinding
        );
        self.last_used = Instant::now();
        true
    }

    pub(crate) fn release(&mut self) {
        self.phase = LivePhase::Idle;
        self.running = false;
        self.turn = None;
        self.pump = None;
    }

    /// Drop the oldest idle sessions until there is room for one more insert.
    /// Busy sessions and the id about to be inserted (not in the map) stay.
    pub(crate) fn evict_idle_sessions(map: &mut HashMap<String, Arc<Mutex<Live>>>) {
        if map.len() < MAX_IDLE_SESSIONS {
            return;
        }
        let mut idle = Vec::new();
        for (id, live) in map.iter() {
            let state = live.lock().unwrap();
            if state.phase == LivePhase::Idle && !state.running {
                idle.push((id.clone(), state.last_used));
            }
        }
        idle.sort_by_key(|(_, seen)| *seen);
        for (id, _) in idle {
            if map.len() < MAX_IDLE_SESSIONS {
                break;
            }
            map.remove(&id);
        }
    }

    pub(crate) fn info(&self, id: &str) -> SessionInfo {
        SessionInfo {
            id: id.to_string(),
            cwd: self.cwd.clone(),
            target: self.target.clone(),
            running: self.running,
            queued: self.queued.len(),
        }
    }
}

pub(crate) fn slim_event(event: &AgentEvent) -> AgentEvent {
    match event {
        AgentEvent::TurnDone {
            input_tokens,
            output_tokens,
            context_tokens,
            cache_read_tokens,
            cache_create_tokens,
            calibration_factor,
            ..
        } => AgentEvent::TurnDone {
            messages: Vec::new(),
            input_tokens: *input_tokens,
            output_tokens: *output_tokens,
            context_tokens: *context_tokens,
            cache_read_tokens: *cache_read_tokens,
            cache_create_tokens: *cache_create_tokens,
            calibration_factor: *calibration_factor,
        },
        AgentEvent::Compacted {
            before_tokens,
            after_tokens,
            summary,
            auto,
            ..
        } => AgentEvent::Compacted {
            messages: Vec::new(),
            before_tokens: *before_tokens,
            after_tokens: *after_tokens,
            summary: cap_text(summary, TEXT_CAP),
            auto: *auto,
        },
        AgentEvent::CheckpointRewound {
            checkpoint_id,
            label,
            remaining_user_turns,
            restored_files,
            deleted_files,
            skipped_files,
            ..
        } => AgentEvent::CheckpointRewound {
            checkpoint_id: *checkpoint_id,
            label: cap_text(label, TEXT_CAP),
            messages: Vec::new(),
            remaining_user_turns: *remaining_user_turns,
            restored_files: restored_files.clone(),
            deleted_files: deleted_files.clone(),
            skipped_files: skipped_files.clone(),
        },
        AgentEvent::FileChanged {
            path,
            before,
            after,
            added,
            removed,
            existed_before,
            restorable_before,
        } => AgentEvent::FileChanged {
            path: path.clone(),
            before: cap_text(before, TEXT_CAP),
            after: cap_text(after, TEXT_CAP),
            added: *added,
            removed: *removed,
            existed_before: *existed_before,
            restorable_before: *restorable_before,
        },
        other => other.clone(),
    }
}

fn cap_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_string();
    out.push('…');
    out
}
