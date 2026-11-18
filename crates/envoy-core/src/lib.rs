//! Core execution layer: jobs, process execution, job logs, filesystem.

pub mod audit;
pub mod config;
pub mod exec;
pub mod fs;
pub mod job;
pub mod log;
pub mod sandbox;
pub mod search;

mod seatbelt;

pub use audit::{audit, truncate};
pub use config::{gate, AgentConfig, AgentConfigStore};
pub use exec::{kill_group, ExecSpec};
pub use fs::{
    edit_file, home_dir, list_dir, mode_of, parse_mode, part_path, read_range, resolve, set_mode,
    write_atomic, write_file,
};
pub use job::{now_ms, Jobs, KEEP_JOBS};
pub use log::{read_from, JobLogs, LogWriter, MAX_LOG_BYTES};
pub use sandbox::{backend_label, prepare_command, probe_backend, SandboxBackend};
pub use search::{glob_files, grep_files};
