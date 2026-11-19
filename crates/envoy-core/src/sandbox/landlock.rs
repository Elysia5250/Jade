//! Linux Landlock policy builder and child-process enforcement.

use super::{create_scratch_dir, LandlockPlan, PreparedCommand};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

#[allow(dead_code)]
pub(crate) const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
pub(crate) const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
pub(crate) const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
pub(crate) const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
pub(crate) const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
pub(crate) const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
pub(crate) const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
pub(crate) const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
pub(crate) const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;
pub(crate) const LANDLOCK_ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;

pub(crate) const FS_READ: u64 = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
pub(crate) const FS_WRITE: u64 = LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_REMOVE_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_MAKE_CHAR
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_MAKE_SOCK
    | LANDLOCK_ACCESS_FS_MAKE_FIFO
    | LANDLOCK_ACCESS_FS_MAKE_BLOCK
    | LANDLOCK_ACCESS_FS_MAKE_SYM
    | LANDLOCK_ACCESS_FS_REFER
    | LANDLOCK_ACCESS_FS_TRUNCATE
    | LANDLOCK_ACCESS_FS_IOCTL_DEV;
#[allow(dead_code)]
pub(crate) const FS_FILE_ALLOWED: u64 = LANDLOCK_ACCESS_FS_EXECUTE
    | LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_READ_FILE
    | LANDLOCK_ACCESS_FS_TRUNCATE
    | LANDLOCK_ACCESS_FS_IOCTL_DEV;

const SYSTEM_RO_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/lib32",
    "/etc",
    "/dev",
    "/proc",
    "/sys",
    "/opt",
    "/nix",
    "/run/systemd/resolve",
];

pub(crate) fn handled_fs_mask(abi: u32) -> u64 {
    let mut fs_mask = FS_READ | FS_WRITE | LANDLOCK_ACCESS_FS_EXECUTE;
    if abi < 2 {
        fs_mask &= !LANDLOCK_ACCESS_FS_REFER;
    }
    if abi < 3 {
        fs_mask &= !LANDLOCK_ACCESS_FS_TRUNCATE;
    }
    if abi < 5 {
        fs_mask &= !LANDLOCK_ACCESS_FS_IOCTL_DEV;
    }
    fs_mask
}

pub(crate) fn build_landlock(
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

    let scratch = create_scratch_dir()?;
    let mut rw_paths = vec![scratch.clone()];
    let dev_null = PathBuf::from("/dev/null");
    if dev_null.exists() {
        rw_paths.push(dev_null);
    }
    if writable_cwd && cwd.exists() && !rw_paths.iter().any(|p| p == cwd) {
        rw_paths.push(cwd.to_path_buf());
    }
    for extra in extra_rw {
        let p = crate::fs::resolve(extra);
        let canon = std::fs::canonicalize(&p).unwrap_or(p);
        if canon.exists() && !rw_paths.iter().any(|r| r == &canon) {
            rw_paths.push(canon);
        }
    }

    // Never grant `/` or `$HOME`: enumerate system read-only roots explicitly so
    // `~/.ssh`, `~/.aws`, `~/.envoy`, and `~/.conduit` remain unreachable.
    let mut ro_paths = Vec::new();
    for root in SYSTEM_RO_ROOTS {
        let p = Path::new(root);
        if p.exists() {
            let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            if !ro_paths.iter().any(|existing| existing == &canon) {
                ro_paths.push(canon);
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        let home = PathBuf::from(home);
        for rel in [".cargo", ".rustup", ".local/bin"] {
            let p = home.join(rel);
            if p.exists() {
                let canon = std::fs::canonicalize(&p).unwrap_or(p);
                if !ro_paths.iter().any(|existing| existing == &canon) {
                    ro_paths.push(canon);
                }
            }
        }
    }
    if !writable_cwd && cwd.exists() && !ro_paths.iter().any(|r| r == cwd) {
        ro_paths.push(cwd.to_path_buf());
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
        scratch_dir: Some(scratch),
        apply_env_in_child: true,
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn landlock_abi() -> Option<u32> {
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
    const NET_ALL: u64 = (1 << 0) | (1 << 1);

    let fs_mask = handled_fs_mask(plan.abi);
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

    let add_paths = |paths: &[PathBuf], access: u64| -> std::io::Result<()> {
        for p in paths {
            let c = std::ffi::CString::new(p.to_string_lossy().as_bytes())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
            let is_dir = unsafe {
                if libc::fstat(fd, st.as_mut_ptr()) < 0 {
                    let e = std::io::Error::last_os_error();
                    libc::close(fd);
                    return Err(e);
                }
                (st.assume_init().st_mode & libc::S_IFMT) == libc::S_IFDIR
            };
            let mut allowed_access = access & fs_mask;
            if !is_dir {
                allowed_access &= FS_FILE_ALLOWED;
            }
            let pb = PathBeneathAttr {
                allowed_access,
                parent_fd: fd,
            };
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    rs_fd,
                    RULE_PATH_BENEATH,
                    &pb,
                    0u32,
                )
            };
            let add_err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            if rc < 0 {
                return Err(add_err);
            }
        }
        Ok(())
    };

    if let Err(err) = add_paths(&plan.ro_paths, FS_READ | LANDLOCK_ACCESS_FS_EXECUTE)
        .and_then(|()| add_paths(&plan.rw_paths, FS_READ | FS_WRITE | LANDLOCK_ACCESS_FS_EXECUTE))
    {
        unsafe { libc::close(rs_fd) };
        return Err(err);
    }

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
