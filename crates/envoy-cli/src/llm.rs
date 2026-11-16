//! Compatibility facade for the agent implementation.
//!
//! New code should import from `crate::agent`; this module keeps the existing
//! `crate::llm::*` paths working while the modules are split.

pub use crate::agent::{run_agent_turn, AgentEvent, AskBroker, AskOption, TurnConfig};
