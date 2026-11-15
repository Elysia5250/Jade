//! Core execution, configuration, auditing, and job logging for the
//! standalone agent.

pub mod audit;
pub mod config;
pub mod job_log;
pub mod runner;

pub use audit::{audit, truncate};
pub use config::{gate, AgentConfig, AgentConfigStore};
pub use job_log::{prune_jobs, JobLog, JOB_LOG_CAP, KEEP_JOBS};
pub use runner::{mode_of, part_path, resolve, set_mode, write_atomic, Runner};
