//! Transport-agnostic monitor types and broker trait for the agent loop.
//!
//! Monitors run in the daemon (`runtime::monitors`) and deliver notifications
//! purely in-session / in-TUI over `agent.sock` (no OS-native D-Bus or macOS
//! Bootstrap Namespace notification calls).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

/// Snapshot of a session monitor, shared across `agent.sock` via postcard.
///
/// Never add `#[serde(skip_serializing_if = ...)]` on fields of this struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: u64,
    pub job_id: u64,
    pub label: String,
    pub pattern: String,
    pub command: String,
    pub trigger_count: u32,
    pub max_triggers: Option<u32>,
    pub owns_job: bool,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct MonitorStartSpec {
    pub label: String,
    pub job_id: u64,
    pub command: String,
    pub pattern: String,
    pub debounce_ms: u64,
    pub max_triggers: Option<u32>,
    pub owns_job: bool,
    pub target: String,
}

pub trait MonitorBroker: Send + Sync + std::fmt::Debug {
    fn start(&self, spec: MonitorStartSpec) -> Result<MonitorInfo>;
    fn list(&self) -> Vec<MonitorInfo>;
    fn stop(
        &self,
        id: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<MonitorInfo>>> + Send + '_>>;
}
