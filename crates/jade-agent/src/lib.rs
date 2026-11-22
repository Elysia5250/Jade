//! Daemon-owned coding agent library: runtime, loop, tools, providers,
//! permissions, hooks, skills, MCP, sessions, auth, and git worktrees.
//!
//! This crate intentionally has no dependency on `ratatui` or TUI code.

#[cfg(not(unix))]
compile_error!("jade-agent currently targets Unix: the daemon speaks over a Unix socket");

pub mod agent;
pub mod auth;
pub mod commands;
pub mod daemon;
pub(crate) mod frontmatter;
pub mod hooks;
pub mod mcp;
pub mod memory;
pub mod model_registry;
pub mod permissions;
pub mod plugins;
pub mod runtime;
pub mod session;
pub mod skills;
pub(crate) mod tools_core;
pub mod worktree;
