//! macOS Seatbelt profiles applied with `sandbox-exec -p`.
//!
//! The profile starts permissive, then takes write access away and punches
//! holes for the workspace and a private scratch directory. Later rules win,
//! so protected directories, user secrets/control sockets, and `.env*` files
//! stay closed after those holes. A per-command scratch directory is
//! `$TMPDIR` for that process only.

use crate::sandbox::{create_scratch_dir, user_control_dirs, user_secret_paths, PROTECTED_DIRS};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

const WORKSPACE_READ_DENY_DIRS: &[&str] = &[".ssh", ".aws", ".jade"];
const CONTROL_SOCKET_NAMES: &[&str] = &["jade.sock", "agent.sock", "control.sock"];

pub struct SeatbeltCommand {
    pub argv: Vec<String>,
    pub scratch_dir: PathBuf,
}

#[cfg(target_os = "macos")]
pub fn find_sandbox_exec() -> Option<PathBuf> {
    let fixed = Path::new("/usr/bin/sandbox-exec");
    if crate::sandbox::is_executable(fixed) {
        return Some(fixed.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("sandbox-exec");
        if crate::sandbox::is_executable(&candidate) {
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
    let scratch = create_scratch_dir()?;
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
            if WORKSPACE_READ_DENY_DIRS.contains(name) {
                lines.push(format!(
                    "(deny file-read* file-write* (subpath {}))",
                    embed_path(&root.join(name))?
                ));
            } else {
                lines.push(format!(
                    "(deny file-write* (subpath {}))",
                    embed_path(&root.join(name))?
                ));
            }
        }
    }
    for secret in user_secret_paths() {
        lines.push(format!(
            "(deny file-read* file-write* (subpath {}))",
            embed_path(&secret)?
        ));
    }
    for root in &inspect {
        let body = regex_env_body(&root.display().to_string());
        lines.push(format!("(deny file-read* file-write* (regex \"{body}\"))"));
    }
    if !allow_net {
        lines.push("(deny network*)".to_string());
    } else {
        for sock in control_socket_paths() {
            lines.push(format!(
                "(deny network-outbound (remote unix-socket (path-literal {})))",
                embed_path(&sock)?
            ));
        }
    }
    Ok(lines.join("\n"))
}

fn control_socket_paths() -> Vec<PathBuf> {
    let mut sockets = Vec::new();
    for dir in user_control_dirs() {
        for name in CONTROL_SOCKET_NAMES {
            let p = dir.join(name);
            if !sockets.iter().any(|existing| existing == &p) {
                sockets.push(p);
            }
        }
    }
    if let Some(val) = std::env::var_os("TOPAZ_CONTROL_SOCK").filter(|s| !s.is_empty()) {
        let p = crate::fs::resolve(&val.to_string_lossy());
        if !sockets.iter().any(|existing| existing == &p) {
            sockets.push(p);
        }
    }
    sockets
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
    format!("^{escaped}/(.*/)?\\.[Ee][Nn][Vv]($|\\.[^/]*)$")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_regex_matches_nested_and_uppercase_names() {
        let body = regex_env_body("/tmp/proj.v1");
        assert!(body.starts_with("^/tmp/proj\\.v1/(.*/)?\\.[Ee][Nn][Vv]"));
        assert!(body.ends_with("($|\\.[^/]*)$"));
    }

    #[test]
    fn profile_blocks_jade_topaz_and_rw_env_files() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jade-seatbelt-unit-{n}"));
        let scratch = std::env::temp_dir().join(format!("jade-seatbelt-scratch-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        let canon_dir = std::fs::canonicalize(&dir).unwrap();
        let canon_scratch = std::fs::canonicalize(&scratch).unwrap();
        let profile = build_profile(&dir, &[], true, true, &canon_scratch).unwrap();

        let jade_rule = format!(
            "(deny file-read* file-write* (subpath \"{}\"))",
            canon_dir.join(".jade").display()
        );
        let ssh_rule = format!(
            "(deny file-read* file-write* (subpath \"{}\"))",
            canon_dir.join(".ssh").display()
        );
        let env_rule_prefix = format!(
            "(deny file-read* file-write* (regex \"^{}/(.*/)?\\.[Ee][Nn][Vv]",
            canon_dir.display()
        );
        assert!(profile.contains(&jade_rule), "{profile}");
        assert!(profile.contains(&ssh_rule), "{profile}");
        assert!(profile.contains(&env_rule_prefix), "{profile}");
        if std::env::var_os("HOME").is_some() {
            assert!(profile.contains("/.jade\")"), "{profile}");
            assert!(profile.contains("/.topaz\")"), "{profile}");
            assert!(
                profile.contains("(deny network-outbound (remote unix-socket (path-literal"),
                "{profile}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
