//! Agent loop, tools, providers, and events.
//!
//! The daemon owns a single turn at a time. This module is transport-agnostic:
//! it never touches the TUI, and providers are selected behind a small adapter.

mod asks;
pub mod checkpoint;
mod config;
pub mod context;
pub mod delegate_pref;
pub mod effort;
mod events;
mod r#loop;
mod loop_hooks;
mod loop_provider;
pub mod monitors;
pub(crate) mod prompt;
mod providers;
pub(crate) mod subagent;
#[cfg(test)]
mod tests_hooks;
#[cfg(test)]
mod tests_phase5;
#[cfg(test)]
mod tests_loop;
#[cfg(test)]
mod tests_stress;
pub mod tools;

pub use asks::AskBroker;
pub use config::{TurnConfig, TurnUsage};
pub use context::CompactionTier;
pub use events::{AgentEvent, AskOption};
pub use monitors::{MonitorBroker, MonitorInfo, MonitorStartSpec};
pub use r#loop::run_agent_turn;
pub(crate) use providers::shared_http_client;
