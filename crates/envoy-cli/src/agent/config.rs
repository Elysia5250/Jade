//! Turn configuration.
use super::asks::AskBroker;
use super::monitors::MonitorBroker;
use crate::auth::{ModelRef, StoredProvider};
use crate::hooks::HookEngine;
use crate::permissions::PermissionEngine;
use crate::session::ChatMessage;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub data_dir: PathBuf,
    pub conduit: Option<PathBuf>,
    pub local_socket: Option<PathBuf>,
    pub target: String,
    pub cwd: PathBuf,
    pub provider: StoredProvider,
    pub model: ModelRef,
    pub history: Vec<ChatMessage>,
    /// Questions the model asks the user.
    pub asks: Arc<AskBroker>,
    /// Messages the user steered in while the turn is running; drained between
    /// tool batches and delivered verbatim as `role=user`.
    pub steer: Arc<Mutex<Vec<String>>>,
    /// Permission & sandbox gate for tool calls in this session.
    pub permissions: Arc<PermissionEngine>,
    /// Optional daemon-side background monitor broker (root turn only).
    pub monitors: Option<Arc<dyn MonitorBroker>>,
    /// Deterministic lifecycle hook engine for this session/subagent.
    pub hooks: Option<Arc<HookEngine>>,
    /// Explicit initial `HookContext.state` passed into the turn.
    pub hook_state: BTreeMap<String, Value>,
    /// Optional commit slot to persist updated `HookContext.state` back to `Live` on turn exit.
    pub hook_state_sink: Option<Arc<Mutex<BTreeMap<String, Value>>>>,
    /// Monotonic turn sequence within the session.
    pub turn_seq: u64,
    /// Optional per-turn tool-step ceiling (defaults to 25).
    pub max_steps: Option<usize>,
}
