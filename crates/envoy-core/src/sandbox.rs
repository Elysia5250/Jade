//! OS-level execution sandbox.
//!
//! Policy evaluation in the agent decides *whether* a command may run; this
//! module enforces the real OS boundary so a command cannot escape its
//! allowed filesystem roots, mutate protected metadata (`.git`, `.envoy`),
//! read secrets (`.env`, `~/.ssh`, `~/.aws`), or open network sockets unless
//! explicitly permitted. Backends are `bwrap`, then Linux Landlock, then
//! macOS Seatbelt. A requested sandbox with no backend fails closed.

use anyhow::{bail, Result};
use envoy_proto::SandboxPolicy;
use std::path::{Path, PathBuf};

const PROTECTED_DIRS: &[&str] = &[".git", ".envoy", ".codex", ".agents", ".ssh", ".aws"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackend {
    Bwrap(PathBuf),
    Landlock(u32),
    Seatbelt(PathBuf),
    None,
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
}

/// Probe which OS sandbox backend is active on this host.
pub fn probe_backend() -> SandboxBackend {
    if let Some(p) = find_bwrap() {
        return SandboxBackend::Bwrap(p);
    }
    #[cfg(target_os = "linux")]
    if let Some(abi) = landlock_abi() {
        return SandboxBackend::Landlock(abi);
    }
    #[cfg(target_os = "macos")]
    if let Some(path) = crate::seatbelt::find_sandbox_exec() {
        return SandboxBackend::Seatbelt(path);
    }
    SandboxBackend::None
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
    if argv.is_empty() {
        bail!("empty argv");
    }
    match policy {
        SandboxPolicy::Off | SandboxPolicy::DangerFullAccess => Ok(PreparedCommand {
            argv: argv.to_vec(),
            cwd_override: None,
            landlock: None,
            scratch_dir: None,
        }),
        SandboxPolicy::ReadOnly {
            cwd: policy_cwd,
            allow_net,
        } => {
            let root = effective_cwd(policy_cwd, cwd);
            match probe_backend() {
                SandboxBackend::Bwrap(bwrap) => {
                    Ok(build_bwrap(&bwrap, argv, &root, &[], false, *allow_net))
                }
                SandboxBackend::Landlock(abi) => {
                    build_landlock(abi, argv, &root, &[], false, *allow_net)
                }
                SandboxBackend::Seatbelt(bin) => {
                    build_seatbelt(&bin, argv, &root, &[], false, *allow_net)
                }
                SandboxBackend::None => host_without_sandbox(argv, "read-only"),
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
                    Ok(build_bwrap(&bwrap, argv, &root, extra_rw, true, *allow_net))
                }
                SandboxBackend::Landlock(abi) => {
                    build_landlock(abi, argv, &root, extra_rw, true, *allow_net)
                }
                SandboxBackend::Seatbelt(bin) => {
                    build_seatbelt(&bin, argv, &root, extra_rw, true, *allow_net)
                }
                SandboxBackend::None => host_without_sandbox(argv, "workspace-write"),
            }
        }
    }
}

/// Isolation was requested and this host has no backend that can apply it.
fn host_without_sandbox(argv: &[String], kind: &str) -> Result<PreparedCommand> {
    let _ = argv;
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

fn find_bwrap() -> Option<PathBuf> {
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("bwrap");
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    for fixed in ["/usr/bin/bwrap", "/bin/bwrap"] {
        let p = Path::new(fixed);
        if is_executable(p) {
            return Some(p.to_path_buf());
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
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

fn empty_sentinel_file() -> Option<PathBuf> {
    let p = std::env::temp_dir().join("envoy-sandbox-empty");
    if !p.exists() {
        if std::fs::write(&p, b"").is_err() {
            return None;
        }
        crate::fs::set_mode(&p, 0o444);
    }
    Some(p)
}

fn build_bwrap(
    bwrap: &Path,
    argv: &[String],
    cwd: &Path,
    extra_rw: &[String],
    writable_cwd: bool,
    allow_net: bool,
) -> PreparedCommand {
    let mut out = vec![
        bwrap.display().to_string(),
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--die-with-parent".into(),
    ];
    if !allow_net {
        out.push("--unshare-net".into());
    }

    // Hide user-level credential directories regardless of workspace mounts.
    if let Ok(home) = std::env::var("HOME") {
        for secret_dir in [".ssh", ".aws"] {
            let p = PathBuf::from(&home).join(secret_dir);
            if p.is_dir() {
                let s = p.display().to_string();
                out.extend(["--tmpfs".into(), s]);
            }
        }
    }

    let mut rw_roots = Vec::new();
    if writable_cwd && cwd.exists() {
        let s = cwd.display().to_string();
        out.extend(["--bind".into(), s.clone(), s]);
        rw_roots.push(cwd.to_path_buf());
    }
    for extra in extra_rw {
        let p = crate::fs::resolve(extra);
        let canon = std::fs::canonicalize(&p).unwrap_or(p);
        if canon.exists() {
            let s = canon.display().to_string();
            out.extend(["--bind".into(), s.clone(), s]);
            rw_roots.push(canon);
        }
    }

    // Re-lock protected directories inside writable roots and mask `.env*` files.
    let mut inspect_roots = rw_roots;
    if !writable_cwd && cwd.exists() {
        inspect_roots.push(cwd.to_path_buf());
    }
    let sentinel = empty_sentinel_file();
    for root in &inspect_roots {
        for dir_name in PROTECTED_DIRS {
            let target = root.join(dir_name);
            if target.is_dir() {
                let s = target.display().to_string();
                out.extend(["--ro-bind".into(), s.clone(), s]);
            }
        }
        if let Some(empty) = &sentinel {
            for env_file in find_env_files(root) {
                out.extend([
                    "--ro-bind".into(),
                    empty.display().to_string(),
                    env_file.display().to_string(),
                ]);
            }
        }
    }

    out.push("--".into());
    out.extend(argv.iter().cloned());
    PreparedCommand {
        argv: out,
        cwd_override: cwd.exists().then(|| cwd.display().to_string()),
        landlock: None,
        scratch_dir: None,
    }
}

fn find_env_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return files;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if (s == ".env" || s.starts_with(".env.")) && entry.path().is_file() {
            files.push(entry.path());
        }
    }
    files
}

fn build_landlock(
    abi: u32,
    argv: &[String],
    cwd: &Path,
    extra_rw: &[String],
    writable_cwd: bool,
    allow_net: bool,
) -> Result<PreparedCommand> {
    if !allow_net && abi < 4 {
        bail!(
            "sandbox requires network isolation, but `bwrap` is absent and Landlock ABI {abi} < 4"
        );
    }
    let mut rw_paths = vec![PathBuf::from("/tmp")];
    if writable_cwd && cwd.exists() {
        rw_paths.push(cwd.to_path_buf());
    }
    for extra in extra_rw {
        let p = crate::fs::resolve(extra);
        let canon = std::fs::canonicalize(&p).unwrap_or(p);
        if canon.exists() {
            rw_paths.push(canon);
        }
    }
    let mut ro_paths = vec![PathBuf::from("/")];
    for dir_name in PROTECTED_DIRS {
        let target = cwd.join(dir_name);
        if target.exists() {
            ro_paths.push(target);
        }
    }
    Ok(PreparedCommand {
        argv: argv.to_vec(),
        cwd_override: None,
        landlock: Some(LandlockPlan {
            abi,
            rw_paths,
            ro_paths,
            deny_net: !allow_net,
        }),
        scratch_dir: None,
    })
}

#[cfg(target_os = "linux")]
fn landlock_abi() -> Option<u32> {
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
    let ver = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if ver >= 1 {
        Some(ver as u32)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
pub fn apply_landlock_in_child(plan: &LandlockPlan) -> std::io::Result<()> {
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }
    #[repr(C)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }
    const RULE_PATH_BENEATH: u32 = 1;
    const FS_READ: u64 = (1 << 2) | (1 << 3) | (1 << 4);
    const FS_WRITE: u64 = (1 << 1)
        | (1 << 5)
        | (1 << 6)
        | (1 << 7)
        | (1 << 8)
        | (1 << 9)
        | (1 << 10)
        | (1 << 11)
        | (1 << 12)
        | (1 << 13);
    const NET_ALL: u64 = (1 << 0) | (1 << 1);

    let mut fs_mask = FS_READ | FS_WRITE | (1 << 0);
    if plan.abi < 2 {
        fs_mask &= !(1 << 13);
    }
    if plan.abi < 3 {
        fs_mask &= !(1 << 14);
    }
    let attr = RulesetAttr {
        handled_access_fs: fs_mask,
        handled_access_net: if plan.deny_net && plan.abi >= 4 {
            NET_ALL
        } else {
            0
        },
    };
    let size = if plan.abi >= 4 {
        std::mem::size_of::<RulesetAttr>()
    } else {
        std::mem::size_of::<u64>()
    };
    let rs = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr, size, 0u32) };
    if rs < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let rs_fd = rs as i32;

    let add_paths = |paths: &[PathBuf], access: u64| {
        for p in paths {
            let Ok(c) = std::ffi::CString::new(p.to_string_lossy().as_bytes()) else {
                continue;
            };
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if fd < 0 {
                continue;
            }
            let pb = PathBeneathAttr {
                allowed_access: access & fs_mask,
                parent_fd: fd,
            };
            unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    rs_fd,
                    RULE_PATH_BENEATH,
                    &pb,
                    0u32,
                );
                libc::close(fd);
            }
        }
    };

    add_paths(&plan.ro_paths, FS_READ | (1 << 0));
    add_paths(&plan.rw_paths, FS_READ | FS_WRITE | (1 << 0));

    let rc_priv = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc_priv < 0 {
        let e = std::io::Error::last_os_error();
        unsafe { libc::close(rs_fd) };
        return Err(e);
    }
    let rc = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, rs_fd, 0u32) };
    let err = std::io::Error::last_os_error();
    unsafe { libc::close(rs_fd) };
    if rc < 0 {
        return Err(err);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn apply_landlock_in_child(_plan: &LandlockPlan) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "landlock requires linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-sandbox-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn off_and_danger_leave_argv_untouched() {
        let argv = vec!["sh".into(), "-c".into(), "echo ok".into()];
        let off = prepare_command(&argv, "/tmp", &SandboxPolicy::Off).unwrap();
        assert_eq!(off.argv, argv);
        assert!(off.landlock.is_none());

        let danger = prepare_command(&argv, "/tmp", &SandboxPolicy::DangerFullAccess).unwrap();
        assert_eq!(danger.argv, argv);
    }

    #[test]
    fn workspace_write_builds_offline_bwrap_protecting_git_and_env() {
        let Some(bwrap) = find_bwrap() else {
            return;
        };
        let dir = tmpdir("bwrap-args");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();

        let argv = vec!["sh".into(), "-c".into(), "pwd".into()];
        let prep = prepare_command(
            &argv,
            dir.to_str().unwrap(),
            &SandboxPolicy::WorkspaceWrite {
                cwd: dir.display().to_string(),
                extra_rw: vec![],
                allow_net: false,
            },
        )
        .unwrap();

        assert_eq!(prep.argv[0], bwrap.display().to_string());
        assert!(prep.argv.contains(&"--unshare-net".to_string()));
        assert!(prep.argv.contains(&"--ro-bind".to_string()));
        assert!(prep.argv.contains(&dir.join(".git").display().to_string()));
        assert!(prep.argv.contains(&dir.join(".env").display().to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn seatbelt_wraps_workspace_write_or_refuses_without_a_backend() {
        let argv = vec!["sh".into(), "-c".into(), "echo ok".into()];
        let dir = tmpdir("seatbelt");
        let result = prepare_command(
            &argv,
            dir.to_str().unwrap(),
            &SandboxPolicy::WorkspaceWrite {
                cwd: dir.display().to_string(),
                extra_rw: vec![],
                allow_net: false,
            },
        );
        match probe_backend() {
            SandboxBackend::Seatbelt(bin) => {
                let prep = result.expect("seatbelt prepare");
                assert_eq!(prep.argv[0], bin.display().to_string());
                assert!(prep.argv.iter().any(|arg| arg == "-p"));
                assert!(prep.argv.iter().any(|arg| arg.contains("(deny network*)")));
                assert!(prep
                    .argv
                    .iter()
                    .any(|arg| arg.contains("(deny file-write* (subpath")));
                assert!(prep.argv.iter().any(|arg| arg.starts_with("TMPDIR=")));
                assert!(prep.scratch_dir.is_some());
                assert!(prep.landlock.is_none());
                if let Some(scratch) = prep.scratch_dir {
                    let _ = std::fs::remove_dir_all(scratch);
                }
            }
            SandboxBackend::None => {
                let err = result
                    .expect_err("missing backend must fail closed")
                    .to_string();
                assert!(err.contains("no sandbox backend"), "{err}");
            }
            other => panic!("unexpected non-linux backend {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
