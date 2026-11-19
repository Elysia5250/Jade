//! Local git worktrees for the `/worktree` command.
//!
//! Trees live beside the main checkout at `<repo>.worktrees/<name>`. The
//! command runs `git` directly. It does not go through the agent sandbox.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[derive(Debug)]
pub(crate) enum Effect {
    Listed(String),
    Toast(String),
    Enter {
        path: PathBuf,
        resume: bool,
        notice: String,
        error: bool,
    },
}

pub(crate) fn execute(cwd: &Path, arg: &str) -> Result<Effect, String> {
    match parse_arg(arg)? {
        Cmd::List => list_effect(cwd),
        Cmd::Open { name, rev } => open_tree(cwd, name, rev),
        Cmd::Remove(name) => remove_tree(cwd, name),
        Cmd::Merge(name) => merge_tree(cwd, name),
    }
}

enum Cmd<'a> {
    List,
    Open { name: &'a str, rev: Option<&'a str> },
    Remove(&'a str),
    Merge(Option<&'a str>),
}

fn parse_arg(arg: &str) -> Result<Cmd<'_>, String> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [] => Ok(Cmd::List),
        ["remove"] => Err("usage: /worktree remove <name>".into()),
        ["remove", name] => Ok(Cmd::Remove(name)),
        ["merge"] => Ok(Cmd::Merge(None)),
        ["merge", name] => Ok(Cmd::Merge(Some(name))),
        ["remove", ..] => Err("usage: /worktree remove <name>".into()),
        ["merge", ..] => Err("usage: /worktree merge [name]".into()),
        [name] => Ok(Cmd::Open { name, rev: None }),
        [name, rev] => Ok(Cmd::Open {
            name,
            rev: Some(rev),
        }),
        _ => Err("usage: /worktree [name [rev]|remove <name>|merge [name]]".into()),
    }
}

fn list_effect(cwd: &Path) -> Result<Effect, String> {
    let main = main_root(cwd)?;
    let here = canon(cwd);
    let mut lines = Vec::new();
    for tree in list_worktrees(&main)? {
        let mark = if same_path(&tree.path, &here) {
            "*"
        } else {
            " "
        };
        let branch = tree.branch.as_deref().unwrap_or("detached");
        lines.push(format!("{mark} {branch}  {}", tree.path.display()));
    }
    Ok(Effect::Listed(lines.join("\n")))
}

fn open_tree(cwd: &Path, name: &str, rev: Option<&str>) -> Result<Effect, String> {
    validate_name(name)?;
    let main = main_root(cwd)?;
    let path = linked_path(&main, name)?;
    if let Some(tree) = find_tree(&main, &path)? {
        let found = tree.path;
        let notice = format!("worktree {}", found.display());
        return Ok(enter(found, true, notice, false));
    }
    if path.exists() {
        return Err(format!(
            "path exists and is not a worktree: {}",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("cannot create: {err}"))?;
    }
    let path_text = path.display().to_string();
    if branch_exists(&main, name)? {
        if rev.is_some() {
            return Err(format!("branch `{name}` already exists"));
        }
        git_text(&main, &["worktree", "add", &path_text, name])?;
    } else {
        let start = rev.unwrap_or("HEAD");
        git_text(&main, &["worktree", "add", "-b", name, &path_text, start])?;
    }
    let created = canon(&path);
    Ok(enter(created, false, format!("worktree {name}"), false))
}

fn remove_tree(cwd: &Path, name: &str) -> Result<Effect, String> {
    validate_name(name)?;
    let main = main_root(cwd)?;
    let path = linked_path(&main, name)?;
    let Some(tree) = find_tree(&main, &path)? else {
        return Err(format!("no worktree {name}"));
    };
    if same_path(&tree.path, &main) {
        return Err("cannot remove the main worktree".into());
    }
    let inside = same_path(&canon(cwd), &tree.path);
    git_text(
        &main,
        &["worktree", "remove", &tree.path.display().to_string()],
    )?;
    let notice = format!("removed worktree {name}");
    if inside {
        Ok(enter(main, true, notice, false))
    } else {
        Ok(Effect::Toast(notice))
    }
}

fn merge_tree(cwd: &Path, name: Option<&str>) -> Result<Effect, String> {
    let main = main_root(cwd)?;
    let source = source_tree(&main, cwd, name)?;
    let source_branch = branch_of(&source)?;
    let main_branch = branch_of(&main)?;
    if source_branch == main_branch {
        return Err(format!(
            "`{source_branch}` is already checked out in the main worktree"
        ));
    }
    if dirty(&source)? {
        return Err(format!("commit on {source_branch} first"));
    }
    if dirty(&main)? {
        return Err("main worktree has uncommitted changes".into());
    }
    let merged = git(&main, &["merge", "--no-edit", &source_branch])?;
    if merged.status.success() {
        return Ok(enter(
            main,
            true,
            format!("merged {source_branch} into {main_branch}"),
            false,
        ));
    }
    if main.join(".git").join("MERGE_HEAD").is_file() {
        return Ok(enter(
            main,
            true,
            format!("merge conflict merging {source_branch} into {main_branch}"),
            true,
        ));
    }
    Err(output_error(&merged))
}

fn source_tree(main: &Path, cwd: &Path, name: Option<&str>) -> Result<PathBuf, String> {
    if let Some(name) = name {
        validate_name(name)?;
        let path = linked_path(main, name)?;
        return find_tree(main, &path)?
            .map(|tree| tree.path)
            .ok_or_else(|| format!("no worktree {name}"));
    }
    let here = canon(cwd);
    if same_path(&here, main) {
        return Err("usage: /worktree merge <name>".into());
    }
    find_tree(main, &here)?
        .map(|tree| tree.path)
        .ok_or_else(|| "not a worktree of this repository".to_string())
}

fn enter(path: PathBuf, resume: bool, notice: String, error: bool) -> Effect {
    Effect::Enter {
        path,
        resume,
        notice,
        error,
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('.')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'));
    if !ok {
        return Err(
            "worktree name must be [A-Za-z0-9._-] and must not start with '-' or '.'".into(),
        );
    }
    if crate::permissions::paths::path_has_protected_component(Path::new(name)) {
        return Err("refusing a protected worktree name".into());
    }
    Ok(())
}

pub(crate) fn linked_path(main: &Path, name: &str) -> Result<PathBuf, String> {
    let stem = main
        .file_name()
        .ok_or_else(|| "repository has no directory name".to_string())?;
    let parent = main
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "repository has no parent directory".to_string())?;
    Ok(parent
        .join(format!("{}.worktrees", stem.to_string_lossy()))
        .join(name))
}

struct GitTree {
    path: PathBuf,
    branch: Option<String>,
}

fn find_tree(main: &Path, path: &Path) -> Result<Option<GitTree>, String> {
    let want = canon(path);
    Ok(list_worktrees(main)?
        .into_iter()
        .find(|tree| same_path(&tree.path, &want)))
}

fn list_worktrees(main: &Path) -> Result<Vec<GitTree>, String> {
    let text = git_text(main, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktrees(&text)
        .into_iter()
        .map(|tree| GitTree {
            path: canon(&tree.path),
            branch: tree.branch,
        })
        .collect())
}

struct ParsedTree {
    path: PathBuf,
    branch: Option<String>,
}

fn parse_worktrees(text: &str) -> Vec<ParsedTree> {
    let mut out = Vec::new();
    let mut path = None;
    let mut branch = None;
    let mut saw = false;
    for line in text.lines() {
        if line.is_empty() {
            push_tree(&mut out, &mut path, &mut branch, &mut saw);
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            push_tree(&mut out, &mut path, &mut branch, &mut saw);
            path = Some(PathBuf::from(rest));
            saw = true;
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string());
        } else if line == "detached" {
            branch = None;
        }
    }
    push_tree(&mut out, &mut path, &mut branch, &mut saw);
    out
}

fn push_tree(
    out: &mut Vec<ParsedTree>,
    path: &mut Option<PathBuf>,
    branch: &mut Option<String>,
    saw: &mut bool,
) {
    if !*saw {
        return;
    }
    if let Some(path) = path.take() {
        out.push(ParsedTree {
            path,
            branch: branch.take(),
        });
    }
    *branch = None;
    *saw = false;
}

fn main_root(cwd: &Path) -> Result<PathBuf, String> {
    if git_text(cwd, &["rev-parse", "--is-inside-work-tree"])? != "true" {
        return Err("not a git worktree".into());
    }
    if git_text(cwd, &["rev-parse", "--is-bare-repository"])? == "true" {
        return Err("bare repositories have no main worktree".into());
    }
    let common = git_text(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(common);
    if common.file_name().and_then(|name| name.to_str()) != Some(".git") {
        return Err("cannot find the main worktree".into());
    }
    common
        .parent()
        .map(canon)
        .filter(|path| path.is_dir())
        .ok_or_else(|| "cannot find the main worktree".to_string())
}

fn branch_of(cwd: &Path) -> Result<String, String> {
    let name = git_text(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if name == "HEAD" {
        return Err("detached HEAD".into());
    }
    Ok(name)
}

fn branch_exists(cwd: &Path, name: &str) -> Result<bool, String> {
    let output = git(
        cwd,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )?;
    if output.status.success() {
        return Ok(true);
    }
    let err = String::from_utf8_lossy(&output.stderr);
    if err.trim().is_empty() {
        Ok(false)
    } else {
        Err(err.trim().to_string())
    }
}

fn dirty(cwd: &Path) -> Result<bool, String> {
    Ok(!git_text(cwd, &["status", "--porcelain"])?.is_empty())
}

fn git_text(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = git(dir, args)?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(output_error(&output))
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_EDITOR", "true")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|err| format!("git failed: {err}"))
}

fn output_error(output: &Output) -> String {
    let err = String::from_utf8_lossy(&output.stderr);
    let out = String::from_utf8_lossy(&output.stdout);
    let text = if err.trim().is_empty() {
        out.trim()
    } else {
        err.trim()
    };
    let line = text.lines().next().unwrap_or("git failed");
    line.chars().take(180).collect()
}

fn canon(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn same_path(left: &Path, right: &Path) -> bool {
    canon(left) == canon(right)
}

pub(crate) mod task;

#[cfg(test)]
mod tests;
