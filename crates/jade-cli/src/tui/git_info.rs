//! Lightweight local git branch, worktree, and dirty-state probing for `inputs::Catalog`.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Default, Clone, PartialEq, Eq, Debug)]
pub(crate) struct GitInfo {
    pub(crate) branch: Option<String>,
    pub(crate) worktree: Option<String>,
    pub(crate) dirty: bool,
}

pub(crate) fn probe_git_info(
    cwd: &Path,
    dirty_cache: &mut Option<(PathBuf, bool, Instant)>,
    dirty_ttl: Duration,
) -> GitInfo {
    let (branch, worktree) = probe_git_head(cwd);
    let dirty = if branch.is_some() || worktree.is_some() {
        let cached = dirty_cache
            .as_ref()
            .filter(|(p, _, at)| p == cwd && at.elapsed() < dirty_ttl)
            .map(|(_, d, _)| *d);
        match cached {
            Some(d) => d,
            None => {
                let d = probe_git_dirty(cwd);
                *dirty_cache = Some((cwd.to_path_buf(), d, Instant::now()));
                d
            }
        }
    } else {
        *dirty_cache = None;
        false
    };
    GitInfo {
        branch,
        worktree,
        dirty,
    }
}

pub(crate) fn probe_git_head(cwd: &Path) -> (Option<String>, Option<String>) {
    for dir in cwd.ancestors() {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            let branch = read_head_branch(&dot_git.join("HEAD"));
            return (branch, None);
        }
        if dot_git.is_file() {
            let Ok(raw) = std::fs::read_to_string(&dot_git) else {
                return (None, None);
            };
            let Some(gitdir) = parse_gitdir(dir, &raw) else {
                return (None, None);
            };
            let worktree = worktree_name_from_gitdir(&gitdir);
            let branch = read_head_branch(&gitdir.join("HEAD"));
            return (branch, worktree);
        }
    }
    (None, None)
}

pub(crate) fn probe_git_dirty(cwd: &Path) -> bool {
    let out = Command::new("git")
        .args([
            "status",
            "--porcelain",
            "--untracked-files=no",
            "--ignore-submodules=dirty",
        ])
        .current_dir(cwd)
        .output();
    matches!(out, Ok(o) if o.status.success() && !o.stdout.is_empty())
}

fn parse_gitdir(repo_dir: &Path, content: &str) -> Option<PathBuf> {
    let raw = content
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))?
        .trim();
    if raw.is_empty() {
        return None;
    }
    let path = Path::new(raw);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_dir.join(path)
    })
}

fn worktree_name_from_gitdir(gitdir: &Path) -> Option<String> {
    let parent_name = gitdir.parent()?.file_name()?.to_str()?;
    if parent_name == "worktrees" {
        return gitdir
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    None
}

fn read_head_branch(head_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(head_path).ok()?;
    let trimmed = content.trim();
    if let Some(rest) = trimmed.strip_prefix("ref:") {
        let target = rest.trim();
        let name = target
            .strip_prefix("refs/heads/")
            .or_else(|| target.strip_prefix("refs/"))
            .unwrap_or(target);
        return (!name.is_empty()).then(|| name.to_string());
    }
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(7).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::probe_git_head;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "jade-git-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn probes_non_git_branch_detached_and_worktree() {
        let dir = tmpdir("probe");
        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(probe_git_head(&plain), (None, None));

        // Standard .git directory with branch ref and nested subdir
        let repo = dir.join("repo");
        let nested = repo.join("crates/cli");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feat/tui-bars\n").unwrap();
        assert_eq!(
            probe_git_head(&nested),
            (Some("feat/tui-bars".to_string()), None)
        );

        // Detached HEAD
        std::fs::write(
            repo.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(probe_git_head(&repo), (Some("0123456".to_string()), None));

        // Linked worktree via `.git` file pointing to `.../worktrees/<name>`
        let wt_gitdir = repo.join(".git/worktrees/feature-x");
        std::fs::create_dir_all(&wt_gitdir).unwrap();
        std::fs::write(wt_gitdir.join("HEAD"), "ref: refs/heads/feature-x\n").unwrap();
        let wt = dir.join("repo.worktrees/feature-x");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .unwrap();
        assert_eq!(
            probe_git_head(&wt),
            (Some("feature-x".to_string()), Some("feature-x".to_string()))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
