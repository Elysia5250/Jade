//! Point-in-time git status and directory tree for the environment note.
//!
//! Captured once per turn. The stable system prompt only points at it.

use std::path::Path;
use std::process::{Command, Stdio};

const MAX_STATUS_LINES: usize = 40;
const MAX_COMMITS: usize = 5;
const MAX_SUBJECT: usize = 100;

pub(crate) const ORIENTATION_RULE: &str = "\
- When the environment message includes a workspace snapshot, use that git status and directory tree for orientation. Call `ls` or `git status` only when the snapshot says it was truncated or is missing.\n";

/// `None` on a remote target, where this process cannot see the workspace.
pub(crate) fn for_local(cwd: &Path, local: bool) -> Option<String> {
    if local {
        Some(workspace_note(cwd))
    } else {
        None
    }
}

fn workspace_note(cwd: &Path) -> String {
    let scan = envoy_core::scan_workspace(cwd);
    format!(
        "## Workspace snapshot\n\
         Point-in-time at the start of this turn. It does not update until the next turn.\n\n\
         ### Git\n\
         {}\n\
         ### Tree\n\
         {}",
        git_section(cwd),
        scan.tree
    )
}

fn git_section(cwd: &Path) -> String {
    let Some(status) = git(cwd, &["status", "--porcelain=v1", "-b"]) else {
        return "not a git repository\n".to_string();
    };
    let mut out = format_status(&status);
    if let Some(log) = git(cwd, &["log", "-5", "--pretty=format:%h %s"]) {
        push_recent(&mut out, &log);
    }
    out
}

fn format_status(raw: &str) -> String {
    let mut lines = raw.lines();
    let branch = lines
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches("## ")
        .trim();
    let mut shown = Vec::new();
    let mut extra = 0usize;
    for line in lines {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if shown.len() >= MAX_STATUS_LINES {
            extra += 1;
        } else {
            shown.push(line.to_string());
        }
    }
    let mut out = format!("branch: {branch}\n");
    if shown.is_empty() {
        out.push_str("clean\n");
    } else {
        for line in shown {
            out.push_str(&line);
            out.push('\n');
        }
        if extra > 0 {
            out.push_str(&format!("({extra} more changes)\n"));
        }
    }
    out
}

fn push_recent(out: &mut String, log: &str) {
    let lines: Vec<&str> = log.lines().take(MAX_COMMITS).collect();
    if lines.is_empty() {
        return;
    }
    out.push_str("recent:\n");
    for line in lines {
        out.push_str(&clip(line));
        out.push('\n');
    }
}

fn clip(line: &str) -> String {
    let line = line.lines().next().unwrap_or("");
    line.chars().take(MAX_SUBJECT).collect()
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("envoy-snap-{tag}-{n}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn plain_directory_still_gets_a_tree() {
        let dir = tmpdir("plain");
        std::fs::write(dir.join("plain.txt"), "x").unwrap();
        let note = for_local(&dir, true).unwrap();
        assert!(note.contains("not a git repository"), "{note}");
        assert!(note.contains("plain.txt"), "{note}");
        assert!(note.contains("Point-in-time"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_targets_do_not_get_a_local_snapshot() {
        let dir = tmpdir("remote");
        assert!(for_local(&dir, false).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn note_has_git_status_and_tree_outside_the_system_prompt() {
        let dir = tmpdir("git");
        git_ok(&dir, &["init"]);
        git_ok(&dir, &["config", "user.email", "t@example.com"]);
        git_ok(&dir, &["config", "user.name", "test"]);
        std::fs::write(dir.join("keep.rs"), "fn keep() {}\n").unwrap();
        git_ok(&dir, &["add", "keep.rs"]);
        git_ok(
            &dir,
            &["-c", "commit.gpgsign=false", "commit", "-m", "add keep"],
        );
        std::fs::write(dir.join("keep.rs"), "fn keep() { changed(); }\n").unwrap();

        let note = for_local(&dir, true).unwrap();
        assert!(note.contains("Point-in-time"));
        assert!(note.contains("branch:"));
        assert!(note.contains("keep.rs"), "{note}");
        assert!(note.contains("recent:"));
        assert!(note.contains("add keep"));

        let system = super::super::prompt::build_system_prompt("local", &dir);
        assert!(system.contains("workspace snapshot"));
        assert!(!system.contains("keep.rs"));
        assert!(!system.contains("Point-in-time"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
