//! Subagent engine: built-in presets, declarative `.jade/agents/*.md` plugins,
//! isolated child loops, and parallel read-only delegation.

pub mod catalog;
pub mod runner;

pub(crate) use catalog::{discover_agents, format_agents_catalog};
pub(crate) use runner::SubagentHost;
