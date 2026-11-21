//! Linux `bubblewrap` (`bwrap`) command builder.
//!
//! Mounts the host filesystem read-only, isolates PID/IPC/UTS/session (and net
//! when disabled), binds allowed writable roots, and THEN masks user secret/
//! control directories and workspace `.git`/`.jade`/`.env*` paths so an
//! ancestor `--bind` (like `$HOME`) can never unmask them.

use super::{is_env_filename, is_executable, user_secret_paths, PreparedCommand, PROTECTED_DIRS};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SKIP_WALK_DIRS: &[&str] = &["target", "node_modules", ".venv", "venv", "__pycache__"];
const MAX_WALK_ENTRIES: usize = 16_384;

pub(crate) fn find_bwrap() -> Option<PathBuf> {
    for fixed in ["/usr/bin/bwrap", "/bin/bwrap"] {
        let p = Path::new(fixed);
        if is_executable(p) {
            return Some(p.to_path_buf());
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("bwrap");
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn effective_uid() -> u32 {
    #[cfg(unix)]
    unsafe {
        libc::geteuid()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

fn sentinel_dir_candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(val) = std::env::var_os("JADE_DATA_DIR").filter(|s| !s.is_empty()) {
        dirs.push(crate::fs::resolve(&val.to_string_lossy()).join("runtime"));
    } else if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        dirs.push(PathBuf::from(home).join(".jade").join("runtime"));
    }
    dirs.push(std::env::temp_dir().join(format!("jade-sandbox-{}", effective_uid())));
    dirs
}

fn empty_sentinel_file() -> Result<PathBuf> {
    let mut last_err = None;
    for dir in sentinel_dir_candidates() {
        if let Err(err) = crate::fs::ensure_private_dir(&dir) {
            last_err = Some(err);
            continue;
        }
        let sentinel = dir.join("empty");
        match ensure_empty_sentinel(&sentinel) {
            Ok(()) => return Ok(sentinel),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("cannot create sandbox sentinel directory")))
}

fn ensure_empty_sentinel(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() || meta.len() != 0 {
                bail!("unsafe sandbox sentinel file at {}", path.display());
            }
            crate::fs::set_mode_strict(path, 0o444)?;
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o444);
            }
            match opts.open(path) {
                Ok(_) => {
                    crate::fs::set_mode_strict(path, 0o444)?;
                    Ok(())
                }
                Err(create_err) if create_err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let meta = std::fs::symlink_metadata(path)
                        .with_context(|| format!("inspecting {}", path.display()))?;
                    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() != 0 {
                        bail!("unsafe sandbox sentinel file at {}", path.display());
                    }
                    Ok(())
                }
                Err(create_err) => {
                    Err(create_err).with_context(|| format!("creating {}", path.display()))
                }
            }
        }
        Err(err) => Err(err).with_context(|| format!("stat {}", path.display())),
    }
}

pub(crate) fn build_bwrap(
    bwrap: &Path,
    argv: &[String],
    cwd: &Path,
    extra_rw: &[String],
    writable_cwd: bool,
    allow_net: bool,
    env: &[(String, String)],
) -> Result<PreparedCommand> {
    let secrets = user_secret_paths();
    build_bwrap_with_secrets(
        bwrap,
        argv,
        cwd,
        extra_rw,
        writable_cwd,
        allow_net,
        &secrets,
        env,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_bwrap_with_secrets(
    bwrap: &Path,
    argv: &[String],
    cwd: &Path,
    extra_rw: &[String],
    writable_cwd: bool,
    allow_net: bool,
    secret_paths: &[PathBuf],
    env: &[(String, String)],
) -> Result<PreparedCommand> {
    let empty = empty_sentinel_file()?;
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
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
        "--new-session".into(),
        "--die-with-parent".into(),
    ];
    if !allow_net {
        out.push("--unshare-net".into());
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
            if !rw_roots.iter().any(|r| r == &canon) {
                rw_roots.push(canon);
            }
        }
    }

    let mut masked = HashSet::new();

    // Hide user-level credential & daemon control directories AFTER all `--bind`
    // mounts so binding `$HOME` or an ancestor cannot expose them again.
    for path in secret_paths {
        mask_secret_path(path, &empty, &mut masked, &mut out);
    }

    // Re-lock protected directories inside inspected roots and mask `.env*` files.
    let mut inspect_roots = rw_roots;
    if !writable_cwd && cwd.exists() && !inspect_roots.iter().any(|r| r == cwd) {
        inspect_roots.push(cwd.to_path_buf());
    }
    let home_paths = home_candidates();
    for root in &inspect_roots {
        let max_depth = if root == Path::new("/") {
            1
        } else if home_paths.iter().any(|h| h == root) {
            2
        } else {
            6
        };
        inspect_and_protect_root(root, max_depth, &empty, &mut masked, &mut out)?;
    }

    for (k, v) in env {
        if k.is_empty() || k.contains('=') || k.contains('\0') || v.contains('\0') {
            bail!("invalid environment variable `{k}` for sandbox");
        }
        out.extend(["--setenv".into(), k.clone(), v.clone()]);
    }

    out.push("--".into());
    out.extend(argv.iter().cloned());
    Ok(PreparedCommand {
        argv: out,
        cwd_override: cwd.exists().then(|| cwd.display().to_string()),
        landlock: None,
        scratch_dir: None,
        apply_env_in_child: false,
    })
}

fn home_candidates() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        let p = PathBuf::from(home);
        homes.push(p.clone());
        if let Ok(canon) = std::fs::canonicalize(&p) {
            if !homes.iter().any(|h| h == &canon) {
                homes.push(canon);
            }
        }
    }
    homes
}

fn mask_secret_path(
    path: &Path,
    empty: &Path,
    masked: &mut HashSet<PathBuf>,
    out: &mut Vec<String>,
) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() || !masked.insert(path.to_path_buf()) {
        return;
    }
    if meta.is_dir() {
        out.extend(["--tmpfs".into(), path.display().to_string()]);
    } else if meta.is_file() {
        out.extend([
            "--ro-bind".into(),
            empty.display().to_string(),
            path.display().to_string(),
        ]);
    }
}

fn inspect_and_protect_root(
    root: &Path,
    max_depth: usize,
    empty: &Path,
    masked: &mut HashSet<PathBuf>,
    out: &mut Vec<String>,
) -> Result<()> {
    let mut stack = vec![(root.to_path_buf(), 1usize)];
    let mut seen_entries = 0usize;

    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            seen_entries += 1;
            if seen_entries > MAX_WALK_ENTRIES {
                bail!(
                    "sandbox walk exceeded {MAX_WALK_ENTRIES} entries under {}; refusing to run with unmasked subtrees",
                    root.display()
                );
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            let name_os = entry.file_name();
            let name = name_os.to_string_lossy();

            if PROTECTED_DIRS.contains(&name.as_ref()) {
                lock_protected_entry(&path, &name, ft, empty, masked, out);
                continue;
            }
            if is_env_filename(&name) {
                mask_env_entry(root, &path, ft, empty, masked, out);
                continue;
            }
            if ft.is_dir() && depth < max_depth && !SKIP_WALK_DIRS.contains(&name.as_ref()) {
                stack.push((path, depth + 1));
            }
        }
    }
    Ok(())
}

fn lock_protected_entry(
    path: &Path,
    name: &str,
    ft: std::fs::FileType,
    empty: &Path,
    masked: &mut HashSet<PathBuf>,
    out: &mut Vec<String>,
) {
    if ft.is_symlink() {
        return;
    }
    if !masked.insert(path.to_path_buf()) {
        return;
    }
    if matches!(name, ".ssh" | ".aws") {
        if ft.is_dir() {
            out.extend(["--tmpfs".into(), path.display().to_string()]);
        } else if ft.is_file() {
            out.extend([
                "--ro-bind".into(),
                empty.display().to_string(),
                path.display().to_string(),
            ]);
        }
    } else if ft.is_dir() || ft.is_file() {
        let s = path.display().to_string();
        out.extend(["--ro-bind".into(), s.clone(), s]);
    }
}

fn mask_env_entry(
    root: &Path,
    path: &Path,
    ft: std::fs::FileType,
    empty: &Path,
    masked: &mut HashSet<PathBuf>,
    out: &mut Vec<String>,
) {
    if ft.is_file() {
        if masked.insert(path.to_path_buf()) {
            out.extend([
                "--ro-bind".into(),
                empty.display().to_string(),
                path.display().to_string(),
            ]);
        }
        return;
    }
    if ft.is_symlink() {
        if let Ok(canon) = std::fs::canonicalize(path) {
            if canon.starts_with(root) && canon.is_file() && masked.insert(canon.clone()) {
                out.extend([
                    "--ro-bind".into(),
                    empty.display().to_string(),
                    canon.display().to_string(),
                ]);
            }
        }
    }
}
