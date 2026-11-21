//! Hub session & workspace lists, live daemon status merging, and session actions.
use super::super::*;
use std::collections::BTreeMap;

impl App {
    pub(in crate::tui) fn reload_hub_lists(&mut self) {
        let here = std::fs::canonicalize(&self.session.current.cwd)
            .unwrap_or_else(|_| self.session.current.cwd.clone());
        let live: HashMap<String, jade_agent::runtime::SessionInfo> = self
            .inputs
            .live_sessions
            .iter()
            .cloned()
            .map(|info| (info.id.clone(), info))
            .collect();
        let current_id = self.session.current.id.clone();
        let has_current = self.ui.welcome.as_ref().is_some_and(|w| w.can_return);

        let mut sessions = build_session_rows(&self.data_dir, &live, &current_id, has_current);
        let workspaces = build_workspace_rows(&here, &sessions, &self.data_dir);

        // Sort: running/queued first, then most recently updated.
        sessions.sort_by(|a, b| {
            let a_live = a.running || a.queued > 0;
            let b_live = b.running || b.queued > 0;
            b_live.cmp(&a_live).then(b.updated_ms.cmp(&a.updated_ms))
        });

        if let Some(welcome) = self.ui.welcome.as_mut() {
            welcome.sessions = sessions;
            welcome.workspaces = workspaces;
            clamp_hub_selection(welcome);
        }
    }

    pub(in crate::tui) fn maybe_poll_hub_live_sessions(&mut self) {
        let due = self
            .ui
            .welcome
            .as_ref()
            .map(|w| {
                w.live_refreshed_at
                    .map(|at| at.elapsed() >= Duration::from_secs(2))
                    .unwrap_or(true)
            })
            .unwrap_or(false);
        if !due || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if let Some(w) = self.ui.welcome.as_mut() {
            w.live_refreshed_at = Some(Instant::now());
        }
        let data_dir = self.data_dir.clone();
        let notice = self.notice_tx.clone();
        tokio::spawn(async move {
            if let Ok(AgentResp::Sessions(list)) =
                runtime::request(&data_dir, &AgentReq::List).await
            {
                let _ = notice.send(UiNotice::LiveSessions(list));
            }
        });
    }

    pub(in crate::tui) fn apply_hub_live_sessions(
        &mut self,
        list: Vec<jade_agent::runtime::SessionInfo>,
    ) {
        self.inputs.live_sessions = list;
        if self.ui.welcome.is_some() {
            self.reload_hub_lists();
        }
    }

    pub(in crate::tui) fn filtered_hub_sessions(&self) -> Vec<usize> {
        let Some(welcome) = self.ui.welcome.as_ref() else {
            return Vec::new();
        };
        let query = welcome.search.trim().to_ascii_lowercase();
        welcome
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                query.is_empty()
                    || row.title.to_ascii_lowercase().contains(&query)
                    || collapse_home(&row.cwd)
                        .to_ascii_lowercase()
                        .contains(&query)
                    || row.target.to_ascii_lowercase().contains(&query)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    pub(in crate::tui) fn filtered_hub_workspaces(&self) -> Vec<usize> {
        let Some(welcome) = self.ui.welcome.as_ref() else {
            return Vec::new();
        };
        let query = welcome.search.trim().to_ascii_lowercase();
        welcome
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                query.is_empty()
                    || collapse_home(&row.cwd)
                        .to_ascii_lowercase()
                        .contains(&query)
                    || row.title.to_ascii_lowercase().contains(&query)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    pub(in crate::tui) fn selected_hub_workspace(&self) -> Option<&HubWorkspaceRow> {
        let welcome = self.ui.welcome.as_ref()?;
        let filtered = self.filtered_hub_workspaces();
        let idx = *filtered.get(welcome.selected_workspace)?;
        welcome.workspaces.get(idx)
    }

    pub(in crate::tui) fn selected_hub_session(&self) -> Option<&HubSessionRow> {
        let welcome = self.ui.welcome.as_ref()?;
        let filtered = self.filtered_hub_sessions();
        let idx = *filtered.get(welcome.selected_session)?;
        welcome.sessions.get(idx)
    }

    pub(in crate::tui) fn selected_hub_cwd(&self) -> PathBuf {
        let Some(welcome) = self.ui.welcome.as_ref() else {
            return self.session.current.cwd.clone();
        };
        match welcome.tab {
            HubTab::Sessions => self
                .selected_hub_session()
                .map(|s| s.cwd.clone())
                .unwrap_or_else(|| self.session.current.cwd.clone()),
            HubTab::Workspaces => self
                .selected_hub_workspace()
                .map(|w| w.cwd.clone())
                .unwrap_or_else(|| self.session.current.cwd.clone()),
        }
    }

    pub(super) fn activate_selected_session(&mut self) {
        let Some(row) = self.selected_hub_session().cloned() else {
            let cwd = self.session.current.cwd.clone();
            self.activate_workspace_with_mode(cwd, true);
            return;
        };
        self.open_hub_session_row(&row);
    }

    pub(super) fn open_hub_session_row(&mut self, row: &HubSessionRow) {
        let can_return = self.ui.welcome.as_ref().is_some_and(|w| w.can_return);
        if can_return && row.id == self.session.current.id {
            self.ui.welcome = None;
            return;
        }
        self.ui.welcome = None;
        match load_session(&self.data_dir, &row.id) {
            Ok(loaded) => {
                self.load_existing_session(loaded);
                self.spawn_agent_attach();
            }
            Err(_) => {
                self.activate_workspace_with_mode(row.cwd.clone(), false);
            }
        }
    }

    pub(super) fn start_new_session_from_hub(&mut self) {
        let cwd = self.selected_hub_cwd();
        self.activate_workspace_with_mode(cwd, false);
    }

    pub(super) fn begin_rename_selected_session(&mut self) {
        let Some(row) = self.selected_hub_session().cloned() else {
            return;
        };
        if let Some(welcome) = self.ui.welcome.as_mut() {
            welcome.rename_target = Some(row.id);
            welcome.draft_cursor = row.title.chars().count();
            welcome.draft = row.title;
            welcome.mode = WelcomeMode::RenameSession;
            welcome.confirm_delete = None;
            welcome.error = None;
        }
    }

    pub(super) fn interrupt_selected_session(&mut self) {
        let Some(row) = self.selected_hub_session().cloned() else {
            return;
        };
        if !row.running {
            if let Some(w) = self.ui.welcome.as_mut() {
                w.error = Some("selected session is not running".into());
            }
            return;
        }
        self.sync_agent(AgentReq::Interrupt { id: row.id });
        self.set_toast(format!("interrupted {}", row.title), false);
        if let Some(w) = self.ui.welcome.as_mut() {
            w.live_refreshed_at = None;
            w.error = None;
        }
    }

    pub(super) fn request_delete_selected_session(&mut self) {
        let Some(row) = self.selected_hub_session().cloned() else {
            return;
        };
        let can_return = self.ui.welcome.as_ref().is_some_and(|w| w.can_return);
        if (can_return && row.id == self.session.current.id) || row.running {
            if let Some(w) = self.ui.welcome.as_mut() {
                w.error = Some("cannot delete an active or running session".into());
                w.confirm_delete = None;
            }
            return;
        }
        let already_armed = self
            .ui
            .welcome
            .as_ref()
            .and_then(|w| w.confirm_delete.as_deref())
            == Some(row.id.as_str());
        if !already_armed {
            if let Some(w) = self.ui.welcome.as_mut() {
                w.confirm_delete = Some(row.id);
                w.error = None;
            }
            return;
        }
        let _ = delete_session(&self.data_dir, &row.id);
        self.sync_agent(AgentReq::CloseSession { id: row.id.clone() });
        if let Some(w) = self.ui.welcome.as_mut() {
            w.confirm_delete = None;
            w.error = None;
        }
        self.reload_hub_lists();
        self.set_toast(format!("deleted {}", row.title), false);
    }

    pub(super) fn activate_selected_workspace(&mut self, resume: bool) {
        let path = self
            .selected_hub_workspace()
            .map(|row| row.cwd.clone())
            .unwrap_or_else(|| self.session.current.cwd.clone());
        self.activate_workspace_with_mode(path, resume);
    }
}

fn build_session_rows(
    data_dir: &std::path::Path,
    live: &HashMap<String, jade_agent::runtime::SessionInfo>,
    current_id: &str,
    has_current: bool,
) -> Vec<HubSessionRow> {
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for s in list_sessions(data_dir) {
        seen.insert(s.id.clone());
        let info = live.get(&s.id);
        rows.push(HubSessionRow {
            is_current: has_current && s.id == current_id,
            running: info.is_some_and(|i| i.running),
            queued: info.map(|i| i.queued).unwrap_or(0),
            id: s.id,
            title: s.title,
            cwd: s.cwd,
            target: s.target,
            updated_ms: s.updated_ms,
            item_count: s.item_count,
            prompt_tokens: s.prompt_tokens,
            changed_files: s.changed_files,
            last_preview: s.last_preview,
        });
    }
    for info in live.values() {
        if seen.contains(&info.id) {
            continue;
        }
        rows.push(HubSessionRow {
            id: info.id.clone(),
            title: "Active daemon session".to_string(),
            cwd: info.cwd.clone(),
            target: info.target.clone(),
            updated_ms: jade_core::now_ms(),
            item_count: 0,
            prompt_tokens: 0,
            changed_files: 0,
            last_preview: String::new(),
            running: info.running,
            queued: info.queued,
            is_current: has_current && info.id == current_id,
        });
    }
    rows
}

fn build_workspace_rows(
    here: &std::path::Path,
    sessions: &[HubSessionRow],
    data_dir: &std::path::Path,
) -> Vec<HubWorkspaceRow> {
    let mut map: BTreeMap<PathBuf, HubWorkspaceRow> = BTreeMap::new();
    let here_resumes = exact_session_for_cwd(data_dir, here).is_some();
    map.insert(
        here.to_path_buf(),
        HubWorkspaceRow {
            cwd: here.to_path_buf(),
            title: String::new(),
            updated_ms: 0,
            session_count: 0,
            running_count: 0,
            is_here: true,
            resumes: here_resumes,
        },
    );
    for s in sessions {
        let key = std::fs::canonicalize(&s.cwd).unwrap_or_else(|_| s.cwd.clone());
        let entry = map.entry(key.clone()).or_insert_with(|| HubWorkspaceRow {
            cwd: key,
            title: String::new(),
            updated_ms: 0,
            session_count: 0,
            running_count: 0,
            is_here: false,
            resumes: true,
        });
        entry.session_count += 1;
        if s.running {
            entry.running_count += 1;
        }
        if s.updated_ms >= entry.updated_ms {
            entry.updated_ms = s.updated_ms;
            entry.title = s.title.clone();
            entry.resumes = true;
        }
    }
    let mut rows: Vec<_> = map.into_values().collect();
    rows.sort_by(|a, b| {
        b.is_here
            .cmp(&a.is_here)
            .then(b.running_count.cmp(&a.running_count))
            .then(b.updated_ms.cmp(&a.updated_ms))
    });
    rows
}

pub(super) fn clamp_hub_selection(welcome: &mut Welcome) {
    let query = welcome.search.trim().to_ascii_lowercase();
    let visible_sessions = welcome
        .sessions
        .iter()
        .filter(|row| {
            query.is_empty()
                || row.title.to_ascii_lowercase().contains(&query)
                || collapse_home(&row.cwd)
                    .to_ascii_lowercase()
                    .contains(&query)
                || row.target.to_ascii_lowercase().contains(&query)
        })
        .count();
    if visible_sessions == 0 {
        welcome.selected_session = 0;
    } else if welcome.selected_session >= visible_sessions {
        welcome.selected_session = visible_sessions - 1;
    }
    let visible_workspaces = welcome
        .workspaces
        .iter()
        .filter(|row| {
            query.is_empty()
                || collapse_home(&row.cwd)
                    .to_ascii_lowercase()
                    .contains(&query)
                || row.title.to_ascii_lowercase().contains(&query)
        })
        .count();
    if visible_workspaces == 0 {
        welcome.selected_workspace = 0;
    } else if welcome.selected_workspace >= visible_workspaces {
        welcome.selected_workspace = visible_workspaces - 1;
    }
    welcome.selected_control = welcome
        .selected_control
        .min(HubControl::ALL.len().saturating_sub(1));
}
