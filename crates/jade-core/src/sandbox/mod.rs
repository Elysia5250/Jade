//! OS-level execution sandbox.
//!
//! Policy evaluation in the agent decides *whether* a command may run; this
//! module enforces the real OS boundary so a command cannot escape its
//! allowed filesystem roots, mutate protected metadata (`.git`, `.jade`),
//! read secrets (`.env`, `~/.ssh`, `~/.aws`), or open network sockets unless
//! explicitly permitted. Backends are `bwrap`, then Linux Landlock, then
//! macOS Seatbelt. A requested sandbox with no backend fails closed.

use anyhow::{bail, Context, Result};
use jade_proto::SandboxPolicy;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

mod bwrap;
mod landlock;
#[cfg(test)]
mod tests;

pub use landlock::apply_landlock_in_child;

pub(crate) const PROTECTED_DIRS: &[&str] = &[".git", ".jade", ".codex", ".agents", ".ssh", ".aws"];
pub(crate) const USER_SECRET_DIRS: &[&str] = &[
    ".ssh", ".aws", ".jade", ".conduit", ".gnupg", ".kube", ".codex", ".agents",
];
const USER_CONTROL_DIRS: &[&str] = &[".jade", ".conduit"];

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);
static BACKEND_CACHE: OnceLock<SandboxBackend> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackend {
    Bwrap(PathBuf),
    Landlock(u32),
    Seatbelt(PathBuf),
    None,
}

/// Explicit security capabilities enforced by the active sandbox backend.
///
/// Higher layers must inspect these capabilities rather than assuming every
/// backend hides secrets or protects subpaths inside a writable root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxCapabilities {
    pub can_hide_secrets: bool,
    pub can_protect_subpaths: bool,
    pub can_isolate_net: bool,
    pub can_isolate_ipc: bool,
}

impl SandboxCapabilities {
    pub fn for_backend(backend: &SandboxBackend) -> Self {
        match backend {
            SandboxBackend::Bwrap(_) => Self {
                can_hide_secrets: true,
                can_protect_subpaths: true,
                can_isolate_net: true,
                can_isolate_ipc: true,
            },
            SandboxBackend::Seatbelt(_) => Self {
                can_hide_secrets: true,
                can_protect_subpaths: true,
                can_isolate_net: true,
                can_isolate_ipc: false,
            },
            SandboxBackend::Landlock(abi) => Self {
                // Landlock PATH_BENEATH rules are strictly additive, so subpaths
                // inside a writable workspace root cannot be locked read-only.
                can_hide_secrets: false,
                can_protect_subpaths: false,
                can_isolate_net: *abi >= 4,
                can_isolate_ipc: false,
            },
            SandboxBackend::None => Self {
                can_hide_secrets: false,
                can_protect_subpaths: false,
                can_isolate_net: false,
                can_isolate_ipc: false,
            },
        }
    }

    /// Whether the backend isolates secrets, subpaths, and network strongly
    /// enough to relax interactive prompts in `Auto` / `Default` modes.
    pub fn enforces_workspace_boundary(&self) -> bool {
        self.can_hide_secrets && self.can_protect_subpaths && self.can_isolate_net
    }
}

#[derive(Debug, Clone)]
pub struct LandlockPlan {
    pub abi: u32,
    pub rw_paths: Vec<PathBuf>,
    pub ro_paths: Vec<PathBuf>,
    pub deny_net: bool,
}

#[derive(Debug, Clone)]
pub struct PreparedCommand {
    pub argv: Vec<String>,
    pub cwd_override: Option<String>,
    pub landlock: Option<LandlockPlan>,
    /// Private directory exported as `$TMPDIR` for this command. Callers remove
    /// it after the process exits.
    pub scratch_dir: Option<PathBuf>,
    /// Whether caller-provided `env` should be set directly on the outer `Command`.
    /// False for `bwrap`, which injects env inside the container via `--setenv`.
    pub apply_env_in_child: bool,
}

/// Probe which OS sandbox backend is active on this host (cached per process).
pub fn probe_backend() -> SandboxBackend {
    BACKEND_CACHE
        .get_or_init(|| {
            if let Some(p) = bwrap::find_bwrap() {
                return SandboxBackend::Bwrap(p);
            }
            #[cfg(target_os = "linux")]
            if let Some(abi) = landlock::landlock_abi() {
                return SandboxBackend::Landlock(abi);
            }
            #[cfg(target_os = "macos")]
            if let Some(path) = crate::seatbelt::find_sandbox_exec() {
                return SandboxBackend::Seatbelt(path);
            }
            SandboxBackend::None
        })
        .clone()
}

/// Capabilities enforced by the active host sandbox backend.
pub fn capabilities() -> SandboxCapabilities {
    SandboxCapabilities::for_backend(&probe_backend())
}

/// Short status token (`"bwrap"`, `"landlock"`, `"seatbelt"`, or `"none"`).
pub fn backend_label() -> &'static str {
    match probe_backend() {
        SandboxBackend::Bwrap(_) => "bwrap",
        SandboxBackend::Landlock(_) => "landlock",
        SandboxBackend::Seatbelt(_) => "seatbelt",
        SandboxBackend::None => "none",
    }
}

/// Wrap `argv` under `policy`, failing closed if isolation is requested and no
/// supported sandbox backend exists.
pub fn prepare_command(
    argv: &[String],
    cwd: &str,
    policy: &SandboxPolicy,
) -> Result<PreparedCommand> {
    prepare_command_with_env(argv, cwd, policy, &[])
}

/// Wrap `argv` and `env` under `policy`, injecting `env` inside the sandbox
/// boundary (`bwrap --setenv`) whenever the backend executes an outer wrapper.
pub fn prepare_command_with_env(
    argv: &[String],
    cwd: &str,
    policy: &SandboxPolicy,
    env: &[(String, String)],
) -> Result<PreparedCommand> {
    if argv.is_empty() {
        bail!("empty argv");
    }
    match policy {
        SandboxPolicy::Off | SandboxPolicy::DangerFullAccess => Ok(PreparedCommand {
            argv: argv.to_vec(),
            cwd_override: None,
            landlock: None,
            scratch_dir: None,
            apply_env_in_child: true,
        }),
        SandboxPolicy::ReadOnly {
            cwd: policy_cwd,
            allow_net,
        } => {
            let root = effective_cwd(policy_cwd, cwd);
            match probe_backend() {
                SandboxBackend::Bwrap(bwrap) => {
                    bwrap::build_bwrap(&bwrap, argv, &root, &[], false, *allow_net, env)
                }
                SandboxBackend::Landlock(abi) => {
                    landlock::build_landlock(abi, argv, &root, &[], false, *allow_net)
                }
                SandboxBackend::Seatbelt(bin) => {
                    build_seatbelt(&bin, argv, &root, &[], false, *allow_net)
                }
                SandboxBackend::None => host_without_sandbox("read-only"),
            }
        }
        SandboxPolicy::WorkspaceWrite {
            cwd: policy_cwd,
            extra_rw,
            allow_net,
        } => {
            let root = effective_cwd(policy_cwd, cwd);
            match probe_backend() {
                SandboxBackend::Bwrap(bwrap) => {
                    bwrap::build_bwrap(&bwrap, argv, &root, extra_rw, true, *allow_net, env)
                }
                SandboxBackend::Landlock(abi) => {
                    landlock::build_landlock(abi, argv, &root, extra_rw, true, *allow_net)
                }
                SandboxBackend::Seatbelt(bin) => {
                    build_seatbelt(&bin, argv, &root, extra_rw, true, *allow_net)
                }
                SandboxBackend::None => host_without_sandbox("workspace-write"),
            }
        }
    }
}

/// Isolation was requested and this host has no backend that can apply it.
fn host_without_sandbox(kind: &str) -> Result<PreparedCommand> {
    bail!(
        "sandbox requested ({kind}), but no sandbox backend is available (`bwrap`, Landlock, or `sandbox-exec`)"
    )
}

fn build_seatbelt(
    bin: &Path,
    argv: &[String],
    cwd: &Path,
    extra_rw: &[String],
    writable_cwd: bool,
    allow_net: bool,
) -> Result<PreparedCommand> {
    let wrapped = crate::seatbelt::wrap(bin, argv, cwd, extra_rw, writable_cwd, allow_net)?;
    Ok(PreparedCommand {
        argv: wrapped.argv,
        cwd_override: cwd.exists().then(|| cwd.display().to_string()),
        landlock: None,
        scratch_dir: Some(wrapped.scratch_dir),
        apply_env_in_child: true,
    })
}

fn effective_cwd(policy_cwd: &str, exec_cwd: &str) -> PathBuf {
    let raw = if !policy_cwd.trim().is_empty() {
        policy_cwd.trim()
    } else if !exec_cwd.trim().is_empty() {
        exec_cwd.trim()
    } else {
        "."
    };
    let p = crate::fs::resolve(raw);
    std::fs::canonicalize(&p).unwrap_or(p)
}

/// User-level credential and control directories that sandboxes must isolate
/// and remote callers must never read or mutate.
pub fn user_secret_paths() -> Vec<PathBuf> {
    collect_user_dirs(USER_SECRET_DIRS, &["JADE_DATA_DIR", "CONDUIT_DATA_DIR"])
}

/// User-level local daemon control directories whose Unix sockets must stay
/// unreachable even when outbound network is otherwise allowed.
pub fn user_control_dirs() -> Vec<PathBuf> {
    collect_user_dirs(USER_CONTROL_DIRS, &["JADE_DATA_DIR", "CONDUIT_DATA_DIR"])
}

fn collect_user_dirs(rel_dirs: &[&str], env_vars: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push_with_canon = |p: PathBuf| {
        if !out.iter().any(|existing| existing == &p) {
            out.push(p.clone());
        }
        if let Ok(canon) = std::fs::canonicalize(&p) {
            if !out.iter().any(|existing| existing == &canon) {
                out.push(canon);
            }
        }
    };
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        let home = PathBuf::from(home);
        for dir in rel_dirs {
            push_with_canon(home.join(dir));
        }
    }
    for var in env_vars {
        if let Some(val) = std::env::var_os(var).filter(|s| !s.is_empty()) {
            let p = crate::fs::resolve(&val.to_string_lossy());
            push_with_canon(p);
        }
    }
    out
}

/// Create a private per-command `$TMPDIR` scratch directory (`0700` on Unix).
pub(crate) fn create_scratch_dir() -> Result<PathBuf> {
    let n = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("jade-scratch-{}-{nanos}-{n}", std::process::id()));
    crate::fs::ensure_private_dir(&dir).context("creating sandbox scratch directory")?;
    std::fs::canonicalize(&dir).context("canonicalizing sandbox scratch directory")
}

/// Case-insensitive check for `.env` and `.env.*` filenames.
pub fn is_env_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == ".env" || lower.starts_with(".env.")
}

/// True if `key` is a loader, shell-startup, interpreter, or proxy variable
/// that untrusted callers must not inject into sandboxed commands.
pub fn is_dangerous_env_var(key: &str) -> bool {
    if key.is_empty() || key.contains('=') || key.contains('\0') {
        return true;
    }
    let upper = key.to_ascii_uppercase();
    upper.starts_with("LD_")
        || upper.starts_with("DYLD_")
        || matches!(
            upper.as_str(),
            "BASH_ENV"
                | "ENV"
                | "IFS"
                | "PATH"
                | "SHELLOPTS"
                | "BASHOPTS"
                | "PS4"
                | "CDPATH"
                | "GLOBIGNORE"
                | "FPATH"
                | "PERL5OPT"
                | "PERL5LIB"
                | "PYTHONPATH"
                | "PYTHONSTARTUP"
                | "NODE_OPTIONS"
                | "RUBYOPT"
                | "RUBYLIB"
                | "GIT_SSH_COMMAND"
                | "GIT_EXEC_PATH"
                | "GIT_CONFIG_PARAMETERS"
                | "GIT_CONFIG_COUNT"
                | "GIT_EXTERNAL_DIFF"
                | "GIT_PAGER"
                | "SSL_CERT_FILE"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "ALL_PROXY"
        )
}

pub(crate) fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Removes a sandbox scratch directory when dropped.
#[derive(Debug)]
pub struct ScratchGuard(pub Option<std::path::PathBuf>);

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
