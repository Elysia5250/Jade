//! Agent loop, tools, providers, and events.
//!
//! The daemon owns a single turn at a time. This module is transport-agnostic:
//! it never touches the TUI, and providers are selected behind a small adapter.

mod asks;
mod config;
pub mod context;
mod events;
mod r#loop;
pub(crate) mod prompt;
mod providers;
pub(crate) mod tools;

pub use asks::AskBroker;
pub use config::TurnConfig;
pub use context::CompactionTier;
pub use events::{AgentEvent, AskOption};
pub use r#loop::run_agent_turn;
