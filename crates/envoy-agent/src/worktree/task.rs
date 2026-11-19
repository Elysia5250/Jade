//! Worktrees used by a parallel `delegate` batch.
//!
//! Creating, committing, and merging happen in the daemon process. The child
//! agent only receives the new directory as its cwd.

use super::{
    branch_exists, branch_of, canon, dirty, git, git_text, linked_path, main_root, output_error,
    validate_name,
};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct Created {
    pub path: PathBuf,
    pub branch: String,
}

#[derive(Debug)]
pub(crate) enum CommitAll {
    Clean,
    Committed,
    Failed(String),
}

#[derive(Debug)]
pub(crate) enum MergeInto {
    Merged,
    Conflict(String),
    Failed(String),
}

#[derive(Debug)]
pub(crate) struct PendingTree {
    pub index: usize,
    pub path: PathBuf,
    pub branch: String,
    pub message: String,
    /// Only a successful child report is committed and merged.
    pub ok: bool,
}

#[derive(Debug)]
pub(crate) struct TreeSettlement {
    pub index: usize,
    pub path: PathBuf,
    pub detail: String,
    pub removed: bool,
    pub failed: bool,
}

pub(crate) fn repo_ready(cwd: &Path) -> bool {
    main_root(cwd).is_ok() && branch_of(cwd).is_ok()
}

/// A real git checkout that is dirty or detached must not host parallel writers.
/// A directory that is not a repository returns `None` so the batch can run in place.
pub(crate) fn isolation_refusal(cwd: &Path) -> Option<String> {
    if main_root(cwd).is_err() {
        return None;
    }
    if branch_of(cwd).is_err() {
        return Some("refusing parallel writers on a detached HEAD".into());
    }
    match dirty(cwd) {
        Ok(true) => Some("refusing parallel writers on a dirty worktree".into()),
        Ok(false) => None,
        Err(error) => Some(error),
    }
}

pub(crate) fn create_branch_tree(cwd: &Path, index: usize) -> Result<Created, String> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let base = format!("w{index}-{millis}");
    for suffix in 0..20 {
        let name = if suffix == 0 {
            base.clone()
        } else {
            format!("{base}-{suffix}")
        };
        match try_create(cwd, &name) {
            Ok(path) => return Ok(Created { path, branch: name }),
            Err(err) if name_taken(&err) => continue,
            Err(err) => return Err(err),
        }
    }
    Err("could not allocate a worktree name".into())
}

pub(crate) fn commit_all(cwd: &Path, message: &str) -> Result<CommitAll, String> {
    if !dirty(cwd)? {
        return Ok(CommitAll::Clean);
    }
    git_text(cwd, &["add", "-A"])?;
    let committed = git(cwd, &["commit", "-m", message])?;
    if committed.status.success() {
        return Ok(CommitAll::Committed);
    }
    Ok(CommitAll::Failed(output_error(&committed)))
}

pub(crate) fn merge_into(cwd: &Path, branch: &str) -> Result<MergeInto, String> {
    let merged = git(cwd, &["merge", "--no-edit", branch])?;
    if merged.status.success() {
        return Ok(MergeInto::Merged);
    }
    let marker = git(cwd, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]);
    let message = output_error(&merged);
    if marker.map(|out| out.status.success()).unwrap_or(false) {
        Ok(MergeInto::Conflict(message))
    } else {
        Ok(MergeInto::Failed(message))
    }
}

pub(crate) fn remove_tree_at(path: &Path) -> Result<(), String> {
    let main = main_root(path)?;
    git_text(
        &main,
        &[
            "worktree",
            "remove",
            "--force",
            &path.display().to_string(),
        ],
    )?;
    Ok(())
}

pub(crate) fn settle_trees(session: &Path, trees: &[PendingTree]) -> Vec<TreeSettlement> {
    let mut merge_at = Vec::new();
    let mut out = Vec::with_capacity(trees.len());
    for (ordinal, tree) in trees.iter().enumerate() {
        if !tree.ok {
            out.push(abandon(tree, "task failed"));
            continue;
        }
        match commit_all(&tree.path, &tree.message) {
            Ok(CommitAll::Clean) => out.push(finish_clean(session, tree)),
            Ok(CommitAll::Committed) => merge_at.push(ordinal),
            Ok(CommitAll::Failed(error)) => out.push(abandon(tree, &format!("commit failed: {error}"))),
            Err(error) => out.push(abandon(tree, &format!("commit failed: {error}"))),
        }
    }
    let mut stop: Option<String> = None;
    for ordinal in merge_at {
        let tree = &trees[ordinal];
        if let Some(reason) = &stop {
            out.push(abandon(tree, &format!("not merged ({reason})")));
            continue;
        }
        match merge_into(session, &tree.branch) {
            Ok(MergeInto::Merged) => out.push(finish_merged(session, tree)),
            Ok(MergeInto::Conflict(message)) => {
                abort_merge(session);
                stop = Some(format!("merge conflict on {}", tree.branch));
                out.push(abandon(tree, &format!("merge conflict: {message}")));
            }
            Ok(MergeInto::Failed(message)) => {
                stop = Some(format!("merge failed on {}", tree.branch));
                out.push(abandon(tree, &format!("merge failed: {message}")));
            }
            Err(message) => {
                stop = Some(format!("merge failed on {}", tree.branch));
                out.push(abandon(tree, &format!("merge failed: {message}")));
            }
        }
    }
    out
}

fn abort_merge(cwd: &Path) {
    let _ = git(cwd, &["merge", "--abort"]);
}

fn delete_branch(repo: &Path, branch: &str) -> Result<(), String> {
    let main = main_root(repo).unwrap_or_else(|_| repo.to_path_buf());
    git_text(&main, &["branch", "-D", branch])?;
    Ok(())
}

fn abandon(tree: &PendingTree, why: &str) -> TreeSettlement {
    let (removed, extra) = match remove_tree_at(&tree.path) {
        Ok(()) => (
            true,
            format!("worktree removed; branch {} kept", tree.branch),
        ),
        Err(error) => (
            false,
            format!("left at {}: {error}", tree.path.display()),
        ),
    };
    TreeSettlement {
        index: tree.index,
        path: tree.path.clone(),
        detail: format!("{why}; {extra}"),
        removed,
        failed: true,
    }
}

fn finish_clean(session: &Path, tree: &PendingTree) -> TreeSettlement {
    let removed = remove_tree_at(&tree.path).is_ok();
    let _ = delete_branch(session, &tree.branch);
    TreeSettlement {
        index: tree.index,
        path: tree.path.clone(),
        detail: "no changes; worktree removed".into(),
        removed,
        failed: false,
    }
}

fn finish_merged(session: &Path, tree: &PendingTree) -> TreeSettlement {
    let removed = remove_tree_at(&tree.path).is_ok();
    let _ = delete_branch(session, &tree.branch);
    TreeSettlement {
        index: tree.index,
        path: tree.path.clone(),
        detail: format!("merged {} into the current branch", tree.branch),
        removed,
        failed: false,
    }
}

fn try_create(cwd: &Path, name: &str) -> Result<PathBuf, String> {
    validate_name(name)?;
    let main = main_root(cwd)?;
    let path = linked_path(&main, name)?;
    if path.exists() || branch_exists(&main, name)? {
        return Err(format!("worktree name `{name}` is taken"));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("cannot create: {err}"))?;
    }
    git_text(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            name,
            &path.display().to_string(),
            "HEAD",
        ],
    )?;
    Ok(canon(&path))
}

fn name_taken(err: &str) -> bool {
    err.contains("is taken") || err.contains("already exists") || err.contains("already used")
}

#[cfg(test)]
mod tests {
    use super::super::{git, git_text, linked_path};
    use super::*;

    fn git_ok(dir: &Path, args: &[&str]) {
        let output = git(dir, args).unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }

    fn init_repo() -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let root = std::env::temp_dir().join(format!(
            "envoy-task-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&root).unwrap();
        git_ok(&root, &["init", "-b", "main"]);
        git_ok(&root, &["config", "user.email", "test@example.com"]);
        git_ok(&root, &["config", "user.name", "test"]);
        git_ok(&root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("note.txt"), "base\n").unwrap();
        git_ok(&root, &["add", "note.txt"]);
        git_ok(&root, &["commit", "-m", "base"]);
        root
    }

    fn cleanup(root: &Path) {
        if let Ok(parent) = linked_path(root, "placeholder") {
            if let Some(dir) = parent.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn two_trees_share_the_object_store_and_a_clean_tree_is_removed() {
        let root = init_repo();
        let first = create_branch_tree(&root, 1).unwrap();
        let second = create_branch_tree(&root, 2).unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(
            std::fs::read_to_string(first.path.join("note.txt")).unwrap(),
            "base\n"
        );
        assert_eq!(
            std::fs::read_to_string(second.path.join("note.txt")).unwrap(),
            "base\n"
        );
        let common = git_text(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .unwrap();
        let other = git_text(
            &first.path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .unwrap();
        assert_eq!(PathBuf::from(common), PathBuf::from(other));
        assert!(matches!(
            commit_all(&first.path, "subagent: empty"),
            Ok(CommitAll::Clean)
        ));
        remove_tree_at(&first.path).unwrap();
        assert!(!first.path.exists());
        cleanup(&root);
    }

    #[test]
    fn commits_and_merges_two_files_back() {
        let root = init_repo();
        let left = create_branch_tree(&root, 1).unwrap();
        let right = create_branch_tree(&root, 2).unwrap();
        std::fs::write(left.path.join("left.txt"), "L\n").unwrap();
        std::fs::write(right.path.join("right.txt"), "R\n").unwrap();
        let settled = settle_trees(
            &root,
            &[
                PendingTree {
                    index: 1,
                    path: left.path.clone(),
                    branch: left.branch.clone(),
                    message: "subagent: left".into(),
                    ok: true,
                },
                PendingTree {
                    index: 2,
                    path: right.path.clone(),
                    branch: right.branch,
                    message: "subagent: right".into(),
                    ok: true,
                },
            ],
        );
        assert!(settled.iter().all(|item| !item.failed), "{settled:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("left.txt")).unwrap(),
            "L\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("right.txt")).unwrap(),
            "R\n"
        );
        assert!(!left.path.exists());
        assert!(!right.path.exists());
        cleanup(&root);
    }

    #[test]
    fn a_conflict_stops_the_remaining_branch() {
        let root = init_repo();
        let first = create_branch_tree(&root, 1).unwrap();
        let second = create_branch_tree(&root, 2).unwrap();
        let third = create_branch_tree(&root, 3).unwrap();
        std::fs::write(first.path.join("note.txt"), "one\n").unwrap();
        std::fs::write(second.path.join("note.txt"), "two\n").unwrap();
        std::fs::write(third.path.join("other.txt"), "three\n").unwrap();
        let settled = settle_trees(
            &root,
            &[
                pending(1, &first, "subagent: one"),
                pending(2, &second, "subagent: two"),
                pending(3, &third, "subagent: three"),
            ],
        );
        assert!(!settled[0].failed, "{settled:?}");
        assert!(
            settled[1].failed && settled[1].detail.contains("conflict"),
            "{settled:?}"
        );
        assert!(
            settled[2].failed && settled[2].detail.contains("not merged"),
            "{settled:?}"
        );
        let note = std::fs::read_to_string(root.join("note.txt")).unwrap();
        assert_eq!(note, "one\n");
        assert!(!git(&root, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .unwrap()
            .status
            .success());
        assert!(
            git_text(&root, &["status", "--porcelain"])
                .unwrap()
                .is_empty()
        );
        assert!(!root.join("other.txt").exists());
        assert!(!third.path.exists());
        cleanup(&root);
    }

    #[test]
    fn only_the_successful_tree_is_merged() {
        let root = init_repo();
        let ok_tree = create_branch_tree(&root, 1).unwrap();
        let bad = create_branch_tree(&root, 2).unwrap();
        std::fs::write(ok_tree.path.join("ok.txt"), "yes\n").unwrap();
        std::fs::write(bad.path.join("bad.txt"), "no\n").unwrap();
        let settled = settle_trees(
            &root,
            &[
                pending_ok(1, &ok_tree, "subagent: ok", true),
                pending_ok(2, &bad, "subagent: bad", false),
            ],
        );
        let good = settled.iter().find(|item| item.index == 1).unwrap();
        let failed = settled.iter().find(|item| item.index == 2).unwrap();
        assert!(!good.failed && good.removed, "{settled:?}");
        assert!(failed.failed && failed.removed, "{settled:?}");
        assert_eq!(std::fs::read_to_string(root.join("ok.txt")).unwrap(), "yes\n");
        assert!(!root.join("bad.txt").exists());
        assert!(!ok_tree.path.exists());
        assert!(!bad.path.exists());
        assert!(!super::branch_exists(&root, &ok_tree.branch).unwrap());
        assert!(super::branch_exists(&root, &bad.branch).unwrap());
        cleanup(&root);
    }

    #[test]
    fn dirty_and_detached_roots_refuse_parallel_writers() {
        let root = init_repo();
        assert!(isolation_refusal(&root).is_none());
        std::fs::write(root.join("note.txt"), "dirty\n").unwrap();
        let reason = isolation_refusal(&root).unwrap();
        assert!(reason.contains("dirty"), "{reason}");
        git_ok(&root, &["checkout", "--", "note.txt"]);
        git_ok(&root, &["checkout", "--detach"]);
        let reason = isolation_refusal(&root).unwrap();
        assert!(reason.contains("detached"), "{reason}");
        let missing = std::env::temp_dir().join(format!("envoy-not-a-repo-{}", std::process::id()));
        std::fs::create_dir_all(&missing).unwrap();
        assert!(isolation_refusal(&missing).is_none());
        let _ = std::fs::remove_dir_all(&missing);
        cleanup(&root);
    }

    fn pending(index: usize, created: &Created, message: &str) -> PendingTree {
        pending_ok(index, created, message, true)
    }

    fn pending_ok(index: usize, created: &Created, message: &str, ok: bool) -> PendingTree {
        PendingTree {
            index,
            path: created.path.clone(),
            branch: created.branch.clone(),
            message: message.into(),
            ok,
        }
    }
}
