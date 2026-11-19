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
    git_text(&main, &["worktree", "remove", &path.display().to_string()])?;
    Ok(())
}

pub(crate) fn settle_trees(session: &Path, trees: &[PendingTree]) -> Vec<TreeSettlement> {
    let mut merge_at = Vec::new();
    let mut out = Vec::with_capacity(trees.len());
    for (ordinal, tree) in trees.iter().enumerate() {
        match commit_all(&tree.path, &tree.message) {
            Ok(CommitAll::Clean) => out.push(settled(tree, remove_clean(&tree.path), false)),
            Ok(CommitAll::Committed) => merge_at.push(ordinal),
            Ok(CommitAll::Failed(error)) => {
                out.push(settled(tree, format!("commit failed: {error}"), true));
            }
            Err(error) => out.push(settled(tree, format!("commit failed: {error}"), true)),
        }
    }
    let mut stop: Option<String> = None;
    for ordinal in merge_at {
        let tree = &trees[ordinal];
        if let Some(reason) = &stop {
            out.push(settled(
                tree,
                format!("not merged ({reason}); left at {}", tree.path.display()),
                true,
            ));
            continue;
        }
        match merge_into(session, &tree.branch) {
            Ok(MergeInto::Merged) => out.push(settled(
                tree,
                format!(
                    "merged {} into the current branch; worktree {}",
                    tree.branch,
                    tree.path.display()
                ),
                false,
            )),
            Ok(MergeInto::Conflict(message)) => {
                stop = Some(format!("merge conflict on {}", tree.branch));
                out.push(settled(tree, format!("merge conflict: {message}"), true));
            }
            Ok(MergeInto::Failed(message)) => {
                stop = Some(format!("merge failed on {}", tree.branch));
                out.push(settled(tree, format!("merge failed: {message}"), true));
            }
            Err(message) => {
                stop = Some(format!("merge failed on {}", tree.branch));
                out.push(settled(tree, format!("merge failed: {message}"), true));
            }
        }
    }
    out
}

fn remove_clean(path: &Path) -> String {
    match remove_tree_at(path) {
        Ok(()) => "no changes; worktree removed".into(),
        Err(error) => format!("no changes; left in place: {error}"),
    }
}

fn settled(tree: &PendingTree, detail: String, failed: bool) -> TreeSettlement {
    let removed = detail.starts_with("no changes; worktree removed");
    TreeSettlement {
        index: tree.index,
        path: tree.path.clone(),
        detail,
        removed,
        failed,
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
                },
                PendingTree {
                    index: 2,
                    path: right.path.clone(),
                    branch: right.branch,
                    message: "subagent: right".into(),
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
        assert!(left.path.is_dir());
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
        assert!(note.contains("<<<<<<<"), "{note}");
        assert!(git(&root, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .unwrap()
            .status
            .success());
        assert!(!root.join("other.txt").exists());
        assert!(third.path.is_dir());
        cleanup(&root);
    }

    fn pending(index: usize, created: &Created, message: &str) -> PendingTree {
        PendingTree {
            index,
            path: created.path.clone(),
            branch: created.branch.clone(),
            message: message.into(),
        }
    }
}
