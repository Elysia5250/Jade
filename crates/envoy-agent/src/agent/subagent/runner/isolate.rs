//! Give each writing subagent its own worktree when a batch can merge back.

use super::{
    compact_preview, merge_hook_state_delta, run, PreparedTask, SubagentHost, SubagentTaskReport,
};
use crate::permissions::{is_workspace_trusted, set_workspace_trusted, PermissionMode};
use crate::worktree::task::{self, PendingTree};
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

pub(super) fn isolation_applies(
    mode: PermissionMode,
    target: &str,
    cwd: &std::path::Path,
    writers: usize,
) -> bool {
    mode != PermissionMode::Plan
        && writers >= 2
        && (target.is_empty() || target == "local")
        && task::repo_ready(cwd)
}

pub(super) async fn run_tasks(
    host: &SubagentHost,
    parent_call_id: &str,
    runnable: Vec<PreparedTask>,
    mut hook_state: BTreeMap<String, Value>,
    parent_tx: &mpsc::UnboundedSender<crate::agent::events::AgentEvent>,
    total: usize,
) -> (Vec<SubagentTaskReport>, BTreeMap<String, Value>) {
    let mode = host.permissions.mode();
    let writers = runnable
        .iter()
        .filter(|task| !task.spec.is_readonly())
        .count();
    let local = host.target.is_empty() || host.target == "local";
    if mode != PermissionMode::Plan && writers >= 2 && local {
        if let Some(reason) = task::isolation_refusal(&host.cwd) {
            return refuse_writers(
                host,
                parent_call_id,
                runnable,
                hook_state,
                parent_tx,
                total,
                reason,
            )
            .await;
        }
    }
    if !isolation_applies(mode, &host.target, &host.cwd, writers) {
        return run_plain(host, parent_call_id, runnable, hook_state, parent_tx, total).await;
    }
    let session = host.cwd.clone();
    let data_dir = host.data_dir.clone();
    let trusted = is_workspace_trusted(&data_dir, &session);
    let mut reports = Vec::new();
    let mut jobs = Vec::new();
    let mut pending = Vec::new();
    let mut cleanup = WorktreeCleanup {
        paths: Vec::new(),
        armed: true,
    };
    for task in runnable {
        if task.spec.is_readonly() {
            jobs.push((task, host.clone()));
            continue;
        }
        match task::create_branch_tree(&session, task.index) {
            Ok(created) => {
                if trusted {
                    let _ = set_workspace_trusted(&data_dir, &created.path, true);
                }
                let message = commit_message(&task);
                let mut child = host.clone();
                child.cwd = created.path.clone();
                let mut task = task;
                task.input.prompt.push_str(&isolation_note(&created.path));
                cleanup.paths.push(created.path.clone());
                pending.push(PendingTree {
                    index: task.index,
                    path: created.path,
                    branch: created.branch,
                    message,
                    ok: false,
                });
                jobs.push((task, child));
            }
            Err(error) => reports.push(failed_report(&task, error)),
        }
    }
    let base = hook_state.clone();
    let (ran, states) = run_parallel(jobs, total, parent_call_id, &base, parent_tx).await;
    reports.extend(ran);
    for (_, child_state) in &states {
        merge_hook_state_delta(&base, child_state, &mut hook_state);
    }
    for tree in &mut pending {
        tree.ok = reports
            .iter()
            .find(|report| report.index == tree.index)
            .is_some_and(|report| report.ok);
    }
    let settled = task::settle_trees(&session, &pending);
    cleanup.armed = false;
    for item in settled {
        if item.removed {
            let _ = set_workspace_trusted(&data_dir, &item.path, false);
        }
        if let Some(report) = reports.iter_mut().find(|report| report.index == item.index) {
            report.summary.push('\n');
            report.summary.push_str(&item.detail);
            if item.failed {
                report.ok = false;
            }
        }
    }
    (reports, hook_state)
}

async fn run_plain(
    host: &SubagentHost,
    parent_call_id: &str,
    runnable: Vec<PreparedTask>,
    mut hook_state: BTreeMap<String, Value>,
    parent_tx: &mpsc::UnboundedSender<crate::agent::events::AgentEvent>,
    total: usize,
) -> (Vec<SubagentTaskReport>, BTreeMap<String, Value>) {
    let all_readonly = runnable.iter().all(|task| task.spec.is_readonly());
    if all_readonly && runnable.len() > 1 {
        let base = hook_state.clone();
        let jobs = runnable
            .into_iter()
            .map(|task| (task, host.clone()))
            .collect();
        let (reports, states) = run_parallel(jobs, total, parent_call_id, &base, parent_tx).await;
        for (_, child_state) in &states {
            merge_hook_state_delta(&base, child_state, &mut hook_state);
        }
        return (reports, hook_state);
    }
    let mut reports = Vec::new();
    for task in runnable {
        let (report, next_state) =
            run::run_single_subagent(task, total, host, parent_call_id, hook_state, parent_tx)
                .await;
        hook_state = next_state;
        reports.push(report);
    }
    (reports, hook_state)
}

async fn run_parallel(
    jobs: Vec<(PreparedTask, SubagentHost)>,
    total: usize,
    parent_call_id: &str,
    base: &BTreeMap<String, Value>,
    parent_tx: &mpsc::UnboundedSender<crate::agent::events::AgentEvent>,
) -> (
    Vec<SubagentTaskReport>,
    Vec<(usize, BTreeMap<String, Value>)>,
) {
    let mut set = JoinSet::new();
    for (task, host) in jobs {
        let call_id = parent_call_id.to_string();
        let tx = parent_tx.clone();
        let child_state = base.clone();
        set.spawn(async move {
            run::run_single_subagent(task, total, &host, &call_id, child_state, &tx).await
        });
    }
    let mut reports = Vec::new();
    let mut states = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((report, child_state)) => {
                let index = report.index;
                reports.push(report);
                states.push((index, child_state));
            }
            Err(error) => reports.push(SubagentTaskReport {
                index: usize::MAX,
                agent: "subagent".into(),
                description: "task".into(),
                model: String::new(),
                ok: false,
                duration_ms: 0,
                input_tokens: 0,
                output_tokens: 0,
                summary: format!("subagent task panicked or cancelled: {error}"),
            }),
        }
    }
    states.sort_by_key(|(index, _)| *index);
    (reports, states)
}

fn isolation_note(path: &std::path::Path) -> String {
    format!(
        "\n\nYou are working in an isolated git worktree at {}.\n\
         Edit only this tree. Do not commit or merge. The parent will commit your changes and merge them back.\n",
        path.display()
    )
}

fn commit_message(task: &PreparedTask) -> String {
    let raw = task
        .input
        .description
        .clone()
        .unwrap_or_else(|| compact_preview(&task.input.prompt, 48));
    let flat: String = raw.chars().filter(|ch| !ch.is_control()).take(62).collect();
    let flat = flat.trim();
    if flat.is_empty() {
        "subagent: task".into()
    } else {
        format!("subagent: {flat}")
    }
}

async fn refuse_writers(
    host: &SubagentHost,
    parent_call_id: &str,
    runnable: Vec<PreparedTask>,
    hook_state: BTreeMap<String, Value>,
    parent_tx: &mpsc::UnboundedSender<crate::agent::events::AgentEvent>,
    total: usize,
    reason: String,
) -> (Vec<SubagentTaskReport>, BTreeMap<String, Value>) {
    let mut reports = Vec::new();
    let mut readonly = Vec::new();
    for task in runnable {
        if task.spec.is_readonly() {
            readonly.push(task);
        } else {
            reports.push(failed_report(&task, reason.clone()));
        }
    }
    if readonly.is_empty() {
        return (reports, hook_state);
    }
    let (ran, state) = run_plain(host, parent_call_id, readonly, hook_state, parent_tx, total).await;
    reports.extend(ran);
    (reports, state)
}

struct WorktreeCleanup {
    paths: Vec<std::path::PathBuf>,
    armed: bool,
}

impl Drop for WorktreeCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for path in &self.paths {
            let _ = task::remove_tree_at(path);
        }
    }
}

fn failed_report(task: &PreparedTask, error: String) -> SubagentTaskReport {
    let description = task
        .input
        .description
        .clone()
        .unwrap_or_else(|| compact_preview(&task.input.prompt, 48));
    SubagentTaskReport {
        index: task.index,
        agent: task.spec.name.clone(),
        description,
        model: String::new(),
        ok: false,
        duration_ms: 0,
        input_tokens: 0,
        output_tokens: 0,
        summary: format!("worktree was not created: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let root = std::env::temp_dir().join(format!(
            "envoy-isolate-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "test"]);
        std::fs::write(root.join("note.txt"), "base\n").unwrap();
        git(&root, &["add", "note.txt"]);
        git(&root, &["commit", "-m", "base"]);
        root
    }

    #[test]
    fn isolation_follows_the_repo_the_target_and_the_writer_count() {
        let plain = std::env::temp_dir().join(format!(
            "envoy-isolate-plain-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join(".git"), "x").unwrap();
        assert!(!isolation_applies(PermissionMode::Auto, "local", &plain, 2));
        let _ = std::fs::remove_dir_all(&plain);

        let root = init_repo();
        assert!(!isolation_applies(PermissionMode::Auto, "local", &root, 1));
        assert!(!isolation_applies(PermissionMode::Auto, "laptop", &root, 2));
        assert!(!isolation_applies(PermissionMode::Plan, "local", &root, 2));
        assert!(isolation_applies(PermissionMode::Auto, "local", &root, 2));
        assert!(isolation_applies(PermissionMode::Default, "", &root, 2));
        git(&root, &["checkout", "--detach"]);
        assert!(!isolation_applies(PermissionMode::Auto, "local", &root, 2));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_writer_indexes_get_two_trees() {
        let root = init_repo();
        let first = task::create_branch_tree(&root, 1).unwrap();
        let second = task::create_branch_tree(&root, 2).unwrap();
        assert_ne!(first.path, second.path);
        assert!(first.path.join("note.txt").is_file());
        assert!(second.path.join("note.txt").is_file());
        let _ = task::remove_tree_at(&first.path);
        let _ = task::remove_tree_at(&second.path);
        let _ = std::fs::remove_dir_all(&root);
        if let Some(parent) = first.path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}
