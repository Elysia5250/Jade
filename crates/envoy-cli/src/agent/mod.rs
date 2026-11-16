//! Agent loop, tools, providers, and events.
//!
//! The daemon owns a single turn at a time. This module is transport-agnostic:
//! it never touches the TUI, and providers are selected behind a small adapter.

mod asks;
mod config;
mod events;
mod r#loop;
mod prompt;
mod providers;
mod tools;

pub use asks::AskBroker;
pub use config::TurnConfig;
pub use events::{AgentEvent, AskOption};
pub use r#loop::run_agent_turn;
