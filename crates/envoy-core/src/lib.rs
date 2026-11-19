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
pub use config::{gate, AgentConfig, AgentConfigStore, Caller};
pub use exec::{kill_group, ExecSpec};
pub use fs::{
    edit_file, ensure_private_dir, harden_process_dumpable, home_dir, list_dir, mode_of,
    open_append_private, parse_mode, part_path, read_range, resolve, set_mode, set_mode_strict,
    system_hostname, write_atomic, write_atomic_sync, write_file,
};
pub use job::{now_ms, Jobs, KEEP_JOBS};
pub use log::{read_from, JobLogs, LogWriter, MAX_LOG_BYTES};
pub use sandbox::{
    backend_label, capabilities, is_dangerous_env_var, prepare_command, prepare_command_with_env,
    probe_backend, SandboxBackend, SandboxCapabilities,
};
pub use search::{glob_files, grep_files};
