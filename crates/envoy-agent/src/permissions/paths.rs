//! Path canonicalization, symlink escape checks, and protected-path guards.

use std::path::{Component, Path, PathBuf};

/// Directory or file basenames that always require protection.
const PROTECTED_BASENAMES: &[&str] = &[
    ".git", ".ssh", ".env", ".envoy", ".codex", ".agents", ".aws", ".netrc", ".curlrc", ".wgetrc",
];

#[derive(Debug, Clone)]
pub struct ResolvedPath {
    pub requested: PathBuf,
    pub canonical: PathBuf,
    pub escaped_via_symlink: bool,
}

/// Expand leading `~` or `$HOME` to the active home directory.
pub fn expand_tilde(raw: &str) -> String {
    let trimmed = raw.trim();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    if trimmed == "~" || trimmed == "$HOME" {
        return home;
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return PathBuf::from(home).join(rest).display().to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("$HOME/") {
        return PathBuf::from(home).join(rest).display().to_string();
    }
    trimmed.to_string()
}

/// Resolve `raw_path` relative to `cwd`, canonicalizing existing ancestors even
/// when the final file does not yet exist, so symlinks are never bypassed.
pub fn resolve_path_pair(cwd: &Path, raw_path: &str) -> ResolvedPath {
    let expanded = expand_tilde(raw_path);
    let candidate = PathBuf::from(&expanded);
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    };
    let requested = lexical_normalize(&joined);
    let canonical = canonicalize_with_missing_tail(&requested);
    let cwd_canon = canonicalize_with_missing_tail(cwd);
    let escaped_via_symlink =
        is_under(&requested, &lexical_normalize(cwd)) && !is_under(&canonical, &cwd_canon);
    ResolvedPath {
        requested,
        canonical,
        escaped_via_symlink,
    }
}

/// True if `path` or any component matches protected secret/repo directories or
/// `.env*` files. Checks both requested and canonical paths.
pub fn is_protected(resolved: &ResolvedPath) -> bool {
    path_has_protected_component(&resolved.requested)
        || path_has_protected_component(&resolved.canonical)
}

/// True if `resolved` canonical path points inside `/tmp`, `/var/tmp`, or the active temp dir.
pub fn is_temp_path(resolved: &ResolvedPath) -> bool {
    let sys_tmp = canonicalize_with_missing_tail(&std::env::temp_dir());
    let tmp = canonicalize_with_missing_tail(Path::new("/tmp"));
    let var_tmp = canonicalize_with_missing_tail(Path::new("/var/tmp"));
    is_under(&resolved.canonical, &tmp)
        || is_under(&resolved.canonical, &var_tmp)
        || is_under(&resolved.canonical, &sys_tmp)
}

/// Canonicalize and filter `extra_rw` roots, dropping any path that targets or
/// traverses a protected secret/config directory, system root, or `$HOME` and
/// its ancestors. Legitimate external workspace directories and `/tmp` pass.
pub fn sanitize_extra_rw(cwd: &Path, raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for entry in raw {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let resolved = resolve_path_pair(cwd, trimmed);
        if is_protected(&resolved)
            || is_unsafe_extra_rw_root(&resolved.requested)
            || is_unsafe_extra_rw_root(&resolved.canonical)
        {
            continue;
        }
        let canon = resolved.canonical.display().to_string();
        if !out.contains(&canon) {
            out.push(canon);
        }
    }
    out
}

fn path_starts_with_ignore_ascii_case(path: &Path, prefix: &Path) -> bool {
    let mut p_iter = path.components();
    for pre in prefix.components() {
        let Some(cur) = p_iter.next() else {
            return false;
        };
        let matched = match (cur, pre) {
            (Component::Normal(a), Component::Normal(b)) => a
                .to_string_lossy()
                .eq_ignore_ascii_case(&b.to_string_lossy()),
            (a, b) => a == b,
        };
        if !matched {
            return false;
        }
    }
    true
}

fn path_eq_ignore_ascii_case(a: &Path, b: &Path) -> bool {
    path_starts_with_ignore_ascii_case(a, b) && path_starts_with_ignore_ascii_case(b, a)
}

fn is_unsafe_extra_rw_root(path: &Path) -> bool {
    if path == Path::new("/") {
        return true;
    }
    // Reject `$HOME`, any ancestor of `$HOME` (e.g. `/home`, `/Users`), and
    // any other user's home directory under `/home/*` or `/Users/*`.
    let under_home_parent = path_starts_with_ignore_ascii_case(path, Path::new("/home"))
        || path_starts_with_ignore_ascii_case(path, Path::new("/Users"));
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        let raw_home = PathBuf::from(home);
        let canon_home = canonicalize_with_missing_tail(&raw_home);
        if path_starts_with_ignore_ascii_case(&raw_home, path)
            || path_starts_with_ignore_ascii_case(&canon_home, path)
        {
            return true;
        }
        if under_home_parent
            && !path_starts_with_ignore_ascii_case(path, &raw_home)
            && !path_starts_with_ignore_ascii_case(path, &canon_home)
        {
            return true;
        }
    } else if under_home_parent {
        return true;
    }
    // Reject any path that is equal to or an ancestor of a user secret path.
    if envoy_core::sandbox::user_secret_paths()
        .iter()
        .any(|secret| path_starts_with_ignore_ascii_case(secret, path))
    {
        return true;
    }
    // Reject sensitive OS trees, mount roots, and system package prefixes
    // (case-insensitively by component for macOS APFS compatibility).
    const BLOCKED_TREES: &[&str] = &[
        "/etc",
        "/private/etc",
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib32",
        "/lib64",
        "/libx32",
        "/boot",
        "/proc",
        "/sys",
        "/dev",
        "/root",
        "/run",
        "/mnt",
        "/media",
        "/opt/homebrew",
        "/System",
        "/Library",
        "/var/lib",
        "/var/log",
        "/var/run",
        "/var/spool",
        "/var/cache",
        "/var/db",
        "/var/mail",
        "/var/backups",
        "/var/root",
        "/var/empty",
        "/private/var/lib",
        "/private/var/log",
        "/private/var/run",
        "/private/var/spool",
        "/private/var/cache",
        "/private/var/db",
        "/private/var/mail",
        "/private/var/root",
    ];
    if BLOCKED_TREES
        .iter()
        .any(|root| path_starts_with_ignore_ascii_case(path, Path::new(root)))
    {
        return true;
    }
    const BLOCKED_EXACT: &[&str] = &["/var", "/private/var", "/private", "/opt", "/srv", "/data"];
    BLOCKED_EXACT
        .iter()
        .any(|exact| path_eq_ignore_ascii_case(path, Path::new(exact)))
}

pub fn path_has_protected_component(path: &Path) -> bool {
    has_component_in(path, PROTECTED_BASENAMES)
}

fn has_component_in(path: &Path, basenames: &[&str]) -> bool {
    let s = path.to_string_lossy();
    if matches!(s.as_ref(), "/etc/shadow" | "/etc/gshadow" | "/etc/sudoers") {
        return true;
    }
    if envoy_core::sandbox::user_secret_paths()
        .iter()
        .any(|secret| path == secret || path.starts_with(secret))
    {
        return true;
    }
    const WILDCARD_PROTECTED: &[&str] = &[
        ".env",
        ".env.local",
        ".git",
        ".ssh",
        ".aws",
        ".envoy",
        ".conduit",
        ".gnupg",
        ".kube",
        ".codex",
        ".agents",
    ];
    for comp in path.components() {
        let Component::Normal(os) = comp else {
            continue;
        };
        let name = os.to_string_lossy();
        if basenames.iter().any(|b| name.eq_ignore_ascii_case(b))
            || envoy_core::sandbox::is_env_filename(&name)
        {
            return true;
        }
        if name.contains(['*', '?', '[']) {
            let lower = name.to_ascii_lowercase();
            if (lower.starts_with('.') && lower.contains('['))
                || WILDCARD_PROTECTED
                    .iter()
                    .any(|target| super::match_rule::wildcard_match(&lower, target))
            {
                return true;
            }
        }
    }
    false
}

/// True when `candidate` canonicalizes equal to or beneath `workspace_cwd`.
pub fn is_path_under_workspace(workspace_cwd: &Path, candidate: &Path) -> bool {
    let ws_canon = canonicalize_with_missing_tail(workspace_cwd);
    let cand_canon = canonicalize_with_missing_tail(candidate);
    is_under(&cand_canon, &ws_canon)
}

/// True when `resolved` stays inside `workspace_canon`.
pub fn is_in_workspace(cwd: &Path, resolved: &ResolvedPath) -> bool {
    let cwd_canon = canonicalize_with_missing_tail(cwd);
    is_under(&resolved.canonical, &cwd_canon)
}

/// Plan mode allows writes ONLY to `<cwd>/.envoy/plans/*.md` or
/// `<data_dir>/plans/*.md`.
pub fn is_plan_file(data_dir: &Path, cwd: &Path, resolved: &ResolvedPath) -> bool {
    let ext_ok = resolved
        .canonical
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md"));
    if !ext_ok {
        return false;
    }
    let cwd_plans = canonicalize_with_missing_tail(&cwd.join(".envoy/plans"));
    let data_plans = canonicalize_with_missing_tail(&data_dir.join("plans"));
    is_under(&resolved.canonical, &cwd_plans) || is_under(&resolved.canonical, &data_plans)
}

pub fn canonicalize_with_missing_tail(path: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    let mut cur = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !cur.exists() {
        let Some(name) = cur.file_name() else {
            break;
        };
        tail.push(name.to_os_string());
        let Some(parent) = cur.parent() else {
            break;
        };
        cur = parent.to_path_buf();
    }
    let mut base = std::fs::canonicalize(&cur).unwrap_or_else(|_| lexical_normalize(&cur));
    for seg in tail.into_iter().rev() {
        base.push(seg);
    }
    base
}

fn is_under(child: &Path, parent: &Path) -> bool {
    child == parent || child.starts_with(parent)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-perm-path-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn resolves_symlinks_escaping_workspace() {
        let dir = tmpdir("symlink");
        let work = dir.join("work");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, work.join("escape")).unwrap();

        let res = resolve_path_pair(&work, "escape/newfile.txt");
        assert!(res.escaped_via_symlink);
        assert!(!is_in_workspace(&work, &res));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn protects_sensitive_dirs_and_env_variants() {
        let work = PathBuf::from("/tmp/proj");
        for p in [
            ".env",
            ".env.local",
            ".git/config",
            ".envoy/permissions.json",
            "~/.ssh/id_ed25519",
            ".aws/credentials",
            ".codex/auth.json",
            ".agents/a",
        ] {
            let r = resolve_path_pair(&work, p);
            assert!(is_protected(&r), "{p} should be protected");
        }
        let normal = resolve_path_pair(&work, "src/main.rs");
        assert!(!is_protected(&normal));
    }

    #[test]
    fn plan_file_only_allows_markdown_inside_plans_dir() {
        let data = PathBuf::from("/tmp/data");
        let work = PathBuf::from("/tmp/proj");
        assert!(is_plan_file(
            &data,
            &work,
            &resolve_path_pair(&work, ".envoy/plans/step1.md")
        ));
        assert!(!is_plan_file(
            &data,
            &work,
            &resolve_path_pair(&work, "src/lib.rs")
        ));
    }
}
