//! One bounded workspace walk for the console file tree and the model snapshot.
//!
//! Honors `.gitignore` inside a git repository, skips build and secret
//! directories, and does not follow symlinks. The file list feeds link
//! resolution. The tree is a shorter rendering of that same list.

use ignore::WalkBuilder;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "dist",
    "build",
    ".next",
    ".ssh",
    ".aws",
    ".jade",
    ".conduit",
    ".gnupg",
    ".kube",
    ".codex",
    ".agents",
];

const MAX_FILES: usize = 5000;
const MAX_WALK_DEPTH: usize = 16;
const MAX_TREE_LINES: usize = 200;
const MAX_TREE_DEPTH: usize = 8;
const MAX_VISITS: usize = 50_000;

/// Relative paths from one walk, plus the tree rendered from those paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceScan {
    pub files: Vec<String>,
    pub tree: String,
}

struct Limits {
    max_files: usize,
    max_walk_depth: usize,
    max_tree_lines: usize,
    max_tree_depth: usize,
    max_visits: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: MAX_FILES,
            max_walk_depth: MAX_WALK_DEPTH,
            max_tree_lines: MAX_TREE_LINES,
            max_tree_depth: MAX_TREE_DEPTH,
            max_visits: MAX_VISITS,
        }
    }
}

struct Walked {
    files: Vec<String>,
    stopped: bool,
}

#[derive(Default)]
struct Dir {
    dirs: BTreeMap<String, Dir>,
    files: BTreeSet<String>,
}

/// Scan `root` once. Missing directories produce an unavailable tree.
pub fn scan_workspace(root: &Path) -> WorkspaceScan {
    scan_with(root, Limits::default())
}

fn scan_with(root: &Path, limits: Limits) -> WorkspaceScan {
    if !root.is_dir() {
        return WorkspaceScan {
            files: Vec::new(),
            tree: "(unavailable)\n".to_string(),
        };
    }
    let walked = walk_files(root, &limits);
    let tree = render_tree(
        &walked.files,
        limits.max_tree_lines,
        limits.max_tree_depth,
        walked.stopped,
    );
    WorkspaceScan {
        files: walked.files,
        tree,
    }
}

fn walk_files(root: &Path, limits: &Limits) -> Walked {
    let mut builder = WalkBuilder::new(root);
    let max_depth = limits.max_walk_depth;
    builder
        .hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .ignore(false)
        .parents(true)
        .require_git(true)
        .max_depth(Some(max_depth))
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(keep_entry);

    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut visits = 0usize;
    let mut stopped = false;
    for item in builder.build() {
        visits += 1;
        if visits > limits.max_visits {
            stopped = true;
            break;
        }
        let Ok(entry) = item else {
            continue;
        };
        if entry.depth() == 0 || !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if files.len() >= limits.max_files {
            stopped = true;
            break;
        }
        let path = entry.path();
        if !claim_inode(path, &mut seen) {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            files.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    files.sort();
    Walked { files, stopped }
}

fn keep_entry(entry: &ignore::DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let Some(ft) = entry.file_type() else {
        return false;
    };
    if ft.is_symlink() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    if ft.is_dir() {
        return !SKIP_DIRS.contains(&name.as_ref());
    }
    !crate::sandbox::is_env_filename(&name)
}

fn claim_inode(path: &Path, seen: &mut HashSet<(u64, u64)>) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return true;
        };
        if !meta.file_type().is_file() {
            return false;
        }
        seen.insert((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, seen);
        true
    }
}

fn render_tree(files: &[String], max_lines: usize, max_depth: usize, stopped: bool) -> String {
    if files.is_empty() {
        return if stopped {
            "(index stopped at 0 files)\n".to_string()
        } else {
            "(empty)\n".to_string()
        };
    }
    let mut root = Dir::default();
    for file in files {
        insert_file(&mut root, file);
    }
    let mut out = Vec::new();
    let mut budget = max_lines;
    let mut ctx = TreeRender {
        max_depth,
        budget: &mut budget,
        out: &mut out,
    };
    render_children(&root, 0, 0, &mut ctx);
    if stopped {
        out.push(format!("(index stopped at {} files)", files.len()));
    }
    if out.is_empty() {
        "(empty)\n".to_string()
    } else {
        format!("{}\n", out.join("\n"))
    }
}

struct TreeRender<'a> {
    max_depth: usize,
    budget: &'a mut usize,
    out: &'a mut Vec<String>,
}

fn insert_file(root: &mut Dir, rel: &str) {
    let parts: Vec<&str> = rel.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return;
    }
    let mut node = root;
    for (idx, part) in parts.iter().enumerate() {
        if idx + 1 == parts.len() {
            node.files.insert((*part).to_string());
            return;
        }
        node = node.dirs.entry((*part).to_string()).or_default();
    }
}

fn file_count(dir: &Dir) -> usize {
    dir.files.len() + dir.dirs.values().map(file_count).sum::<usize>()
}

fn render_children(dir: &Dir, depth: usize, indent: usize, ctx: &mut TreeRender<'_>) {
    let mut names: Vec<(&str, bool)> = dir
        .dirs
        .keys()
        .map(|name| (name.as_str(), true))
        .chain(dir.files.iter().map(|name| (name.as_str(), false)))
        .collect();
    names.sort_by(|a, b| a.0.cmp(b.0));
    let mut omitted = 0usize;
    for (name, is_dir) in names {
        if *ctx.budget == 0 {
            omitted += if is_dir {
                file_count(&dir.dirs[name])
            } else {
                1
            };
            continue;
        }
        emit_child(dir, name, is_dir, depth, indent, ctx);
    }
    if omitted > 0 {
        ctx.out
            .push(format!("{}({omitted} more)", " ".repeat(indent)));
    }
}

fn emit_child(
    dir: &Dir,
    name: &str,
    is_dir: bool,
    depth: usize,
    indent: usize,
    ctx: &mut TreeRender<'_>,
) {
    let pad = " ".repeat(indent);
    if !is_dir {
        ctx.out.push(format!("{pad}{name}"));
        *ctx.budget -= 1;
        return;
    }
    let Some(child) = dir.dirs.get(name) else {
        return;
    };
    if depth + 1 >= ctx.max_depth {
        let count = file_count(child);
        ctx.out.push(format!("{pad}{name}/ ({count} files)"));
        *ctx.budget -= 1;
        return;
    }
    ctx.out.push(format!("{pad}{name}/"));
    *ctx.budget -= 1;
    render_children(child, depth + 1, indent + 2, ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("jade-tree-{tag}-{n}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn gitignore_and_skipped_dirs_stay_out_of_the_scan() {
        let dir = tmpdir("ignore");
        git(&dir, &["init"]);
        std::fs::write(dir.join(".gitignore"), "skip.log\n").unwrap();
        std::fs::write(dir.join("keep.rs"), "x").unwrap();
        std::fs::write(dir.join("skip.log"), "x").unwrap();
        std::fs::write(dir.join(".env"), "secret").unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules/a.rs"), "x").unwrap();

        let scan = scan_workspace(&dir);
        assert!(scan.files.iter().any(|path| path == "keep.rs"), "{scan:?}");
        assert!(scan.tree.contains("keep.rs"));
        assert!(!scan.files.iter().any(|path| path.contains("skip.log")));
        assert!(!scan.tree.contains("skip.log"));
        assert!(!scan.files.iter().any(|path| path.contains(".env")));
        assert!(!scan.files.iter().any(|path| path.contains("node_modules")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tree_reports_how_many_entries_did_not_fit() {
        let dir = tmpdir("cap");
        for name in ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let scan = scan_with(
            &dir,
            Limits {
                max_tree_lines: 2,
                ..Limits::default()
            },
        );
        assert!(scan.tree.contains("a.txt"), "{scan:?}");
        assert!(scan.tree.contains("(3 more)"), "{scan:?}");
        assert!(!scan.tree.contains("e.txt"), "{scan:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deep_directories_collapse_with_a_file_count() {
        let dir = tmpdir("deep");
        std::fs::create_dir_all(dir.join("deep/nest")).unwrap();
        std::fs::write(dir.join("deep/nest/file.rs"), "x").unwrap();
        let scan = scan_with(
            &dir,
            Limits {
                max_tree_depth: 1,
                ..Limits::default()
            },
        );
        assert!(scan.tree.contains("deep/ (1 files)"), "{scan:?}");
        assert!(!scan.tree.contains("file.rs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loops_do_not_duplicate_inodes() {
        let dir = tmpdir("loop");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.txt"), "hi").unwrap();
        std::os::unix::fs::symlink(dir.join("sub/a.txt"), dir.join("sub/b.txt")).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("sub/loop")).unwrap();
        let started = std::time::Instant::now();
        let scan = scan_workspace(&dir);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let copies = scan
            .files
            .iter()
            .filter(|path| path.ends_with("a.txt") || path.ends_with("b.txt"))
            .count();
        assert_eq!(copies, 1, "{:?}", scan.files);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
