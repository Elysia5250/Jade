//! Console handlers for `/monitor`, `/rewind`, and checkpoint rewind state sync.
use super::*;
use crate::session::{compute_diff_counts, ChatMessage};
use std::collections::HashSet;
use std::path::Path;

impl App {
    pub(in crate::tui) fn run_monitor_command(&mut self, arg: &str) {
        let trimmed = arg.trim();
        if trimmed.is_empty() || trimmed == "list" {
            if self.active_monitors.is_empty() {
                self.push_notice(
                    "Monitors: none active in this session (start one via agent `monitor` tool).",
                    false,
                );
                return;
            }
            let lines: Vec<String> = self
                .active_monitors
                .iter()
                .map(|m| {
                    let state = if m.active { "active" } else { "stopped" };
                    let cap = m
                        .max_triggers
                        .map(|max| format!("{}/{}", m.trigger_count, max))
                        .unwrap_or_else(|| format!("{}", m.trigger_count));
                    format!(
                        "#{} [{}] {} (job {}, pattern `{}`, triggers {})",
                        m.id, state, m.label, m.job_id, m.pattern, cap
                    )
                })
                .collect();
            self.push_notice(lines.join("\n"), false);
            return;
        }

        if let Some(target) = trimmed.strip_prefix("stop ").map(str::trim) {
            if target == "all" {
                let ids: Vec<u64> = self
                    .active_monitors
                    .iter()
                    .filter(|m| m.active)
                    .map(|m| m.id)
                    .collect();
                if ids.is_empty() {
                    self.set_toast("no active monitors to stop", false);
                    return;
                }
                for id in ids {
                    self.stop_monitor_by_id(id);
                }
                return;
            }
            let raw = target.strip_prefix('#').unwrap_or(target);
            match raw.parse::<u64>() {
                Ok(id) => self.stop_monitor_by_id(id),
                Err(_) => self.set_toast("usage: /monitor [list|stop <id>|stop all]", true),
            }
            return;
        }

        self.set_toast("usage: /monitor [list|stop <id>|stop all]", true);
    }

    pub(in crate::tui) fn stop_monitor_by_id(&mut self, monitor_id: u64) {
        if self.agent_offline || self.agent_stream.is_none() || !self.agent_ready {
            self.set_toast("daemon offline - cannot stop monitor", true);
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let data_dir = self.data_dir.clone();
        let id = self.session.id.clone();
        let notice = self.notice_tx.clone();
        self.set_toast(format!("stopping monitor #{monitor_id}..."), false);
        tokio::spawn(async move {
            match runtime::request(&data_dir, &AgentReq::StopMonitor { id, monitor_id }).await {
                Ok(AgentResp::Ack) => {
                    let _ = notice.send(UiNotice::Toast(
                        format!("monitor #{monitor_id} stopped"),
                        false,
                    ));
                }
                Ok(AgentResp::Error(e)) => {
                    let _ = notice.send(UiNotice::Toast(e, true));
                }
                Err(e) => {
                    let _ = notice.send(UiNotice::Toast(format!("{e:#}"), true));
                }
                _ => {}
            }
        });
    }

    pub(in crate::tui) fn run_rewind_command(&mut self, arg: &str) {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            self.open_rewind_modal();
            return;
        }
        if trimmed == "last" {
            self.request_rewind(None);
            return;
        }
        let raw = trimmed.strip_prefix('#').unwrap_or(trimmed);
        match raw.parse::<u64>() {
            Ok(id) => self.request_rewind(Some(id)),
            Err(_) => self.set_toast("usage: /rewind [id|last]", true),
        }
    }

    pub(in crate::tui) fn open_rewind_modal(&mut self) {
        if self.agent_offline || self.agent_stream.is_none() || !self.agent_ready {
            self.set_toast("daemon offline - cannot list checkpoints", true);
            return;
        }
        if self.agent_running {
            self.set_toast("turn in progress; wait or interrupt first", true);
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let data_dir = self.data_dir.clone();
        let id = self.session.id.clone();
        let notice = self.notice_tx.clone();
        tokio::spawn(async move {
            match runtime::request(&data_dir, &AgentReq::ListCheckpoints { id }).await {
                Ok(AgentResp::Checkpoints(list)) => {
                    let _ = notice.send(UiNotice::Checkpoints(list));
                }
                Ok(AgentResp::Error(e)) => {
                    let _ = notice.send(UiNotice::Toast(e, true));
                }
                Err(e) => {
                    let _ = notice.send(UiNotice::Toast(format!("{e:#}"), true));
                }
                _ => {}
            }
        });
    }

    pub(in crate::tui) fn request_rewind(&mut self, checkpoint_id: Option<u64>) {
        if self.agent_offline || self.agent_stream.is_none() || !self.agent_ready {
            self.set_toast("daemon offline - cannot rewind", true);
            return;
        }
        if self.agent_running {
            self.set_toast("turn in progress; wait or interrupt first", true);
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let data_dir = self.data_dir.clone();
        let id = self.session.id.clone();
        let notice = self.notice_tx.clone();
        self.set_toast("rewinding checkpoint...", false);
        tokio::spawn(async move {
            match runtime::request(
                &data_dir,
                &AgentReq::Rewind {
                    id,
                    checkpoint_id,
                },
            )
            .await
            {
                Ok(AgentResp::Ack) => {}
                Ok(AgentResp::Error(e)) => {
                    let _ = notice.send(UiNotice::Toast(e, true));
                }
                Err(e) => {
                    let _ = notice.send(UiNotice::Toast(format!("{e:#}"), true));
                }
                _ => {}
            }
        });
    }

    pub(in crate::tui) fn apply_checkpoint_rewound(
        &mut self,
        checkpoint_id: u64,
        label: String,
        messages: Vec<ChatMessage>,
        remaining_user_turns: usize,
        restored_files: Vec<String>,
        deleted_files: Vec<String>,
        skipped_files: Vec<String>,
    ) {
        self.session.messages = messages;
        self.session.prompt_tokens = 0;

        let mut seen_turns = 0usize;
        let mut cut_at = self.session.items.len();
        for (idx, item) in self.session.items.iter().enumerate() {
            if let TranscriptItem::User { mode, .. } = item {
                if mode != "steer" {
                    if seen_turns == remaining_user_turns {
                        cut_at = idx;
                        break;
                    }
                    seen_turns += 1;
                }
            }
        }
        self.session.items.truncate(cut_at);

        self.reconcile_changed_files_after_rewind(&restored_files, &deleted_files);

        self.agent_running = false;
        self.active_assistant_id = None;
        self.active_tool_items.clear();
        self.pending_ask = None;
        self.queued.clear();
        self.selected_card = None;
        self.transcript_cache.clear();
        self.workspace_index.invalidate();
        self.inspector.selected_diff = self
            .inspector
            .selected_diff
            .min(self.session.changed_files.len().saturating_sub(1));

        let mut summary = format!(
            "rewound to #{checkpoint_id} `{label}`: {} restored, {} removed",
            restored_files.len(),
            deleted_files.len(),
        );
        if !skipped_files.is_empty() {
            summary.push_str(&format!(", {} skipped", skipped_files.len()));
        }
        self.push_notice(summary.clone(), !skipped_files.is_empty());
        self.set_toast(
            format!("rewound to #{checkpoint_id}"),
            !skipped_files.is_empty(),
        );
        self.session.touch();
        self.persist_session();
    }

    fn reconcile_changed_files_after_rewind(
        &mut self,
        restored_files: &[String],
        deleted_files: &[String],
    ) {
        let local = self.target.is_empty() || self.target == "local";
        let deleted_norm: HashSet<String> = deleted_files
            .iter()
            .map(|p| links::normalize_display_path(&self.session.cwd, Path::new(p)))
            .collect();
        let restored_norm: HashSet<String> = restored_files
            .iter()
            .map(|p| links::normalize_display_path(&self.session.cwd, Path::new(p)))
            .collect();

        let cwd = self.session.cwd.clone();
        self.session.changed_files.retain_mut(|fc| {
            let norm = links::normalize_display_path(&cwd, Path::new(&fc.path));
            if deleted_norm.contains(&norm) || deleted_files.contains(&fc.path) {
                return false;
            }
            if restored_norm.contains(&norm) || restored_files.contains(&fc.path) {
                if local {
                    let abs = if Path::new(&fc.path).is_absolute() {
                        PathBuf::from(&fc.path)
                    } else {
                        cwd.join(&fc.path)
                    };
                    if let Ok(current) = std::fs::read_to_string(&abs) {
                        fc.after = current;
                        let (added, removed) = compute_diff_counts(&fc.before, &fc.after);
                        fc.added = added;
                        fc.removed = removed;
                        return added > 0 || removed > 0;
                    }
                }
                return false;
            }
            true
        });
    }
}
