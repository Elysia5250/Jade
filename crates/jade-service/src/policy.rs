//! Server-side security floor for remote `AgentServer` callers.
//!
//! Local same-user callers are already trusted on their own machine. Remote
//! Conduit peers—even when allowlisted—must not disable OS sandboxing or read/
//! mutate host credentials, `.git`/`.env`, or daemon control state.

use jade_core::fs::{home_dir, resolve};
use jade_core::sandbox::{is_env_filename, user_secret_paths};
use jade_proto::{Op, SandboxPolicy, SubmitRequest};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub(crate) const MAX_CONCURRENT_STREAMS: usize = 128;
pub(crate) const MAX_WRITE_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const WRITE_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

const PROTECTED_COMPONENTS: &[&str] = &[".git", ".jade", ".ssh", ".aws", ".codex", ".agents"];
const PROTECTED_DATA_ITEMS: &[&str] = &[
    "auth.json",
    "model.json",
    "agent.json",
    "jade.sock",
    "agent.sock",
    "jade.lock",
    "jade.pid",
    "sessions",
    "checkpoints",
    "agent",
    "jobs",
    "tmp",
    "runtime",
];

pub(crate) fn guard_remote_op(op: Op, data_dir: &Path) -> Result<Op, String> {
    match op {
        Op::Submit(mut req) => {
            enforce_remote_submit(&mut req, data_dir)?;
            Ok(Op::Submit(req))
        }
        Op::Read {
            path,
            offset,
            limit,
        } => {
            check_remote_path(&path, data_dir)?;
            Ok(Op::Read {
                path,
                offset,
                limit,
            })
        }
        Op::Write { path, mode } => {
            check_remote_path(&path, data_dir)?;
            Ok(Op::Write { path, mode })
        }
        Op::List { path } => {
            check_remote_path(&path, data_dir)?;
            Ok(Op::List { path })
        }
        Op::Edit {
            path,
            old,
            new,
            replace_all,
        } => {
            check_remote_path(&path, data_dir)?;
            Ok(Op::Edit {
                path,
                old,
                new,
                replace_all,
            })
        }
        Op::Glob {
            root,
            pattern,
            max_results,
        } => {
            check_remote_path(&root, data_dir)?;
            Ok(Op::Glob {
                root,
                pattern,
                max_results,
            })
        }
        Op::Grep {
            root,
            pattern,
            include,
            max_results,
        } => {
            check_remote_path(&root, data_dir)?;
            Ok(Op::Grep {
                root,
                pattern,
                include,
                max_results,
            })
        }
        other => Ok(other),
    }
}

pub(crate) fn check_remote_path(raw: &str, data_dir: &Path) -> Result<(), String> {
    let resolved = resolve(raw);
    let canon = canonical_with_tail(&resolved);

    for check in [&resolved, &canon] {
        for comp in check.components() {
            if let Component::Normal(part) = comp {
                let name = part.to_string_lossy();
                if PROTECTED_COMPONENTS.contains(&name.as_ref()) || is_env_filename(&name) {
                    return Err(format!("remote access to protected path `{raw}` is denied"));
                }
            }
        }
    }

    for secret in user_secret_paths() {
        let secret_canon = std::fs::canonicalize(&secret).unwrap_or(secret);
        if canon.starts_with(&secret_canon) {
            return Err(format!("remote access to protected path `{raw}` is denied"));
        }
    }

    let data_canon = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    if let Ok(rel) = canon.strip_prefix(&data_canon) {
        if let Some(Component::Normal(first)) = rel.components().next() {
            let first = first.to_string_lossy();
            if PROTECTED_DATA_ITEMS.contains(&first.as_ref()) {
                return Err(format!("remote access to daemon state `{raw}` is denied"));
            }
        }
    }
    Ok(())
}

pub(crate) fn enforce_remote_submit(
    req: &mut SubmitRequest,
    data_dir: &Path,
) -> Result<(), String> {
    if let Some(ref cwd) = req.cwd {
        check_remote_path(cwd, data_dir)?;
    }

    match &req.sandbox {
        SandboxPolicy::Off | SandboxPolicy::DangerFullAccess => {
            let cwd_str = req
                .cwd
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(home_dir);
            let canon = canonical_with_tail(&resolve(&cwd_str));
            let home_canon = canonical_with_tail(&PathBuf::from(home_dir()));
            if req.cwd.is_none() || canon == Path::new("/") || canon == home_canon {
                req.sandbox = SandboxPolicy::ReadOnly {
                    cwd: cwd_str,
                    allow_net: false,
                };
            } else {
                req.sandbox = SandboxPolicy::WorkspaceWrite {
                    cwd: cwd_str,
                    extra_rw: Vec::new(),
                    allow_net: false,
                };
            }
        }
        SandboxPolicy::ReadOnly { cwd, .. } => {
            check_remote_path(cwd, data_dir)?;
        }
        SandboxPolicy::WorkspaceWrite { cwd, extra_rw, .. } => {
            check_remote_path(cwd, data_dir)?;
            if canonical_with_tail(&resolve(cwd)) == Path::new("/") {
                return Err("remote workspace-write sandbox cannot target `/`".into());
            }
            for extra in extra_rw {
                check_remote_path(extra, data_dir)?;
                if canonical_with_tail(&resolve(extra)) == Path::new("/") {
                    return Err("remote extra_rw cannot target `/`".into());
                }
            }
        }
    }
    Ok(())
}

fn canonical_with_tail(path: &Path) -> PathBuf {
    let mut cur = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !cur.exists() {
        let Some(name) = cur.file_name() else {
            break;
        };
        tail.push(name.to_os_string());
        if !cur.pop() {
            break;
        }
    }
    let mut out = if cur.exists() {
        std::fs::canonicalize(&cur).unwrap_or(cur)
    } else {
        cur
    };
    for part in tail.into_iter().rev() {
        let s = part.to_string_lossy();
        if s == "." {
            continue;
        }
        if s == ".." {
            out.pop();
            continue;
        }
        out.push(part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use jade_proto::{TaskSpec, DEFAULT_GROUP};

    fn sample_submit(cwd: Option<&str>, sandbox: SandboxPolicy) -> SubmitRequest {
        SubmitRequest {
            spec: TaskSpec::Shell {
                command: "echo ok".into(),
            },
            cwd: cwd.map(str::to_string),
            env: Vec::new(),
            label: None,
            group: DEFAULT_GROUP.to_string(),
            timeout_s: 30,
            sandbox,
        }
    }

    #[test]
    fn remote_path_blocks_secrets_git_env_and_daemon_state() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data = std::env::temp_dir().join(format!("es-policy-{n}"));
        std::fs::create_dir_all(&data).unwrap();

        assert!(check_remote_path(".ssh/id_rsa", &data).is_err());
        assert!(check_remote_path(".jade/auth.json", &data).is_err());
        assert!(check_remote_path("/tmp/proj/.env.local", &data).is_err());
        assert!(check_remote_path("/tmp/proj/.ENV", &data).is_err());
        assert!(check_remote_path("/tmp/proj/.git/config", &data).is_err());
        assert!(check_remote_path(data.join("auth.json").to_str().unwrap(), &data).is_err());
        assert!(check_remote_path(data.join("sessions/s1.json").to_str().unwrap(), &data).is_err());
        assert!(check_remote_path(data.join("work.txt").to_str().unwrap(), &data).is_ok());

        let _ = std::fs::remove_dir_all(&data);
    }

    #[cfg(unix)]
    #[test]
    fn remote_path_blocks_symlinks_into_protected_dirs() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("es-policy-sym-{n}"));
        let secret = root.join(".git");
        let pubdir = root.join("pub");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::create_dir_all(&pubdir).unwrap();
        std::os::unix::fs::symlink(&secret, pubdir.join("link")).unwrap();

        assert!(check_remote_path(pubdir.join("link/config").to_str().unwrap(), &root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enforce_remote_submit_clamps_off_and_rejects_protected_extra_rw() {
        let data = std::env::temp_dir();
        let mut req = sample_submit(Some("/tmp/project"), SandboxPolicy::Off);
        enforce_remote_submit(&mut req, &data).unwrap();
        assert!(matches!(
            req.sandbox,
            SandboxPolicy::WorkspaceWrite {
                allow_net: false,
                ..
            }
        ));

        let mut no_cwd = sample_submit(None, SandboxPolicy::DangerFullAccess);
        enforce_remote_submit(&mut no_cwd, &data).unwrap();
        assert!(matches!(
            no_cwd.sandbox,
            SandboxPolicy::ReadOnly {
                allow_net: false,
                ..
            }
        ));

        let mut bad_extra = sample_submit(
            Some("/tmp/project"),
            SandboxPolicy::WorkspaceWrite {
                cwd: "/tmp/project".into(),
                extra_rw: vec!["/tmp/project/.env".into()],
                allow_net: false,
            },
        );
        assert!(enforce_remote_submit(&mut bad_extra, &data).is_err());
    }
}
