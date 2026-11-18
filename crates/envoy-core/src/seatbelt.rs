//! macOS Seatbelt profiles applied with `sandbox-exec -p`.
//!
//! The profile starts permissive, then takes write access away and punches
//! holes for the workspace and a private scratch directory. Later rules win,
//! so protected directories and secret files stay closed after those holes.
//! A per-command scratch directory is `$TMPDIR` for that process only.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const PROTECTED_DIRS: &[&str] = &[".git", ".envoy", ".codex", ".agents", ".ssh", ".aws"];

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct SeatbeltCommand {
    pub argv: Vec<String>,
    pub scratch_dir: PathBuf,
}

pub fn find_sandbox_exec() -> Option<PathBuf> {
    for fixed in ["/usr/bin/sandbox-exec"] {
        let path = Path::new(fixed);
        if is_executable(path) {
            return Some(path.to_path_buf());
        }
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("sandbox-exec");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

pub fn wrap(
    bin: &Path,
    argv: &[String],
    cwd: &Path,
    extra_rw: &[String],
    writable_cwd: bool,
    allow_net: bool,
) -> Result<SeatbeltCommand> {
    let scratch = create_scratch()?;
    let profile = match build_profile(cwd, extra_rw, writable_cwd, allow_net, &scratch) {
        Ok(profile) => profile,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(err);
        }
    };
    let scratch_s = scratch.display().to_string();
    let mut out = vec![
        bin.display().to_string(),
        "-p".into(),
        profile,
        "--".into(),
        "/usr/bin/env".into(),
        format!("TMPDIR={scratch_s}"),
        format!("TMP={scratch_s}"),
        format!("TEMP={scratch_s}"),
    ];
    out.extend(argv.iter().cloned());
    Ok(SeatbeltCommand {
        argv: out,
        scratch_dir: scratch,
    })
}

fn create_scratch() -> Result<PathBuf> {
    let n = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("envoy-scratch-{}-{nanos}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).context("creating sandbox scratch directory")?;
    crate::fs::set_mode(&dir, 0o700);
    std::fs::canonicalize(&dir).context("canonicalizing sandbox scratch directory")
}

fn build_profile(
    cwd: &Path,
    extra_rw: &[String],
    writable_cwd: bool,
    allow_net: bool,
    scratch: &Path,
) -> Result<String> {
    let cwd = canonical_existing(cwd);
    let mut writable = Vec::new();
    if writable_cwd {
        if let Some(root) = &cwd {
            writable.push(root.clone());
        }
    }
    for extra in extra_rw {
        let resolved = crate::fs::resolve(extra);
        if let Some(root) = canonical_existing(&resolved) {
            if !writable.iter().any(|have| have == &root) {
                writable.push(root);
            }
        }
    }

    let mut inspect = writable.clone();
    if let Some(root) = &cwd {
        if !inspect.iter().any(|have| have == root) {
            inspect.push(root.clone());
        }
    }

    let mut lines = vec![
        "(version 1)".to_string(),
        "(allow default)".to_string(),
        "(deny file-write*)".to_string(),
        "(allow file-write* (literal \"/dev/null\"))".to_string(),
        "(allow file-ioctl (literal \"/dev/null\"))".to_string(),
        format!("(allow file-write* (subpath {}))", embed_path(scratch)?),
    ];
    for root in &writable {
        lines.push(format!(
            "(allow file-write* (subpath {}))",
            embed_path(root)?
        ));
    }
    for root in &inspect {
        for name in PROTECTED_DIRS {
            lines.push(format!(
                "(deny file-write* (subpath {}))",
                embed_path(&root.join(name))?
            ));
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let home = std::fs::canonicalize(&home).unwrap_or(home);
        for name in [".ssh", ".aws"] {
            lines.push(format!(
                "(deny file-read* file-write* (subpath {}))",
                embed_path(&home.join(name))?
            ));
        }
    }
    for root in &inspect {
        let body = regex_env_body(&root.display().to_string());
        lines.push(format!("(deny file-read* (regex \"{body}\"))"));
    }
    if !allow_net {
        lines.push("(deny network*)".to_string());
    }
    Ok(lines.join("\n"))
}

fn canonical_existing(path: &Path) -> Option<PathBuf> {
    if !path.exists() {
        return None;
    }
    Some(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn embed_path(path: &Path) -> Result<String> {
    let text = path.display().to_string();
    if text
        .chars()
        .any(|c| matches!(c, '"' | '\\' | '\n' | '\r' | '(' | ')'))
    {
        bail!("refusing to sandbox a path seatbelt cannot quote safely: {text}");
    }
    Ok(format!("\"{text}\""))
}

fn regex_env_body(root: &str) -> String {
    let mut escaped = String::new();
    for c in root.chars() {
        if matches!(
            c,
            '.' | '^' | '$' | '*' | '+' | '?' | '[' | ']' | '{' | '}' | '|'
        ) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    format!("^{escaped}/\\.env($|\\.[^/]*)$")
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
