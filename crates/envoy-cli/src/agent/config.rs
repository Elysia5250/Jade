//! Turn configuration.
use super::asks::AskBroker;
use crate::auth::{ModelRef, StoredProvider};
use crate::permissions::PermissionEngine;
use crate::session::ChatMessage;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub data_dir: PathBuf,
    pub conduit: Option<PathBuf>,
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
}
