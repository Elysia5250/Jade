//! Console state: input, sessions, login, and the agent event stream.
use super::*;

mod agent;
mod agent_events;
mod command_ops;
mod commands;
mod edit;
mod input;
mod jobs;
mod login;
mod modals;
mod ops;
mod welcome;

impl App {
    pub(in crate::tui) fn new(
        data_dir: PathBuf,
        target: String,
        conduit: Option<PathBuf>,
        cwd: PathBuf,
    ) -> Self {
        let prefs = load_prefs(&data_dir);
        let mode = InputMode::Agent;
        let session = Session::new(cwd.clone(), target.clone());
        let perm_mode = PermissionEngine::new(
            data_dir.clone(),
            cwd,
            session.id.clone(),
            target.clone(),
            PermissionMode::Default,
        )
        .mode();
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
        Self {
            data_dir,
            conduit,
            target,
            status: None,
            watch: None,
            jobs: Vec::new(),
            job_selected: 0,
            session,
            focus: Focus::Input,
            mode,
            perm_mode,
            show_sidebar: prefs.show_sidebar,
            follow: true,
            transcript_scroll: 0,
            transcript_height: 0,
            transcript_max_scroll: 0,
            selected_card: None,
            input: String::new(),
            input_cursor: 0,
            history: Vec::new(),
            history_pos: None,
            menu_selected: 0,
            at_dismissed_at: None,
            modal: None,
            theme: prefs.theme % THEMES.len(),
            leader_at: None,
            toast: None,
            connected: false,
            interval: Duration::from_millis(600),
            last_refresh: Instant::now() - Duration::from_secs(1),
            agent_running: false,
            agent_stream: None,
            agent_retry_at: Instant::now(),
            agent_offline: false,
            agent_ready: false,
            daemon_incarnation: 0,
            snapshot: None,
            skip_replay_tokens: false,
            pending_ask: None,
            queued: VecDeque::new(),
            outbox: VecDeque::new(),
            notice_tx,
            notice_rx,
            active_assistant_id: None,
            active_tool_items: HashMap::new(),
            active_monitors: Vec::new(),
            shell_watches: Vec::new(),
            login_rx: None,
            login_task: None,
            login_cancel: None,
            models_rx: None,
            welcome: None,
            inspector: InspectorState::default(),
            workspace_index: links::WorkspaceIndex::default(),
            hit_regions: Vec::new(),
            transcript_area: Rect::default(),
            expanded_explore_groups: HashSet::new(),
            mouse_capture: true,
            selection: None,
            drag_start: None,
            screen_lines: Vec::new(),
            transcript_cache: HashMap::new(),
        }
    }

    pub(in crate::tui) fn theme(&self) -> Theme {
        THEMES[self.theme].1
    }

    pub(in crate::tui) fn load_existing_session(&mut self, mut loaded: Session) {
        self.persist_session();
        if loaded.cwd.is_dir() {
            self.session_cwd_set(loaded.cwd.clone());
        }
        let mut last_notice: Option<String> = None;
        loaded.items.retain_mut(|item| match item {
            TranscriptItem::Tool { name, .. } if name == "say" => false,
            TranscriptItem::Tool { collapsed, .. } => {
                last_notice = None;
                *collapsed = true;
                true
            }
            TranscriptItem::Assistant {
                reasoning_collapsed,
                ..
            } => {
                last_notice = None;
                *reasoning_collapsed = true;
                true
            }
            TranscriptItem::Notice {
                text,
                is_error: false,
                ..
            } => {
                if last_notice.as_deref() == Some(text.as_str()) {
                    false
                } else {
                    last_notice = Some(text.clone());
                    true
                }
            }
            _ => {
                last_notice = None;
                true
            }
        });
        self.session = loaded;
        self.selected_card = None;
        self.follow = true;
        self.expanded_explore_groups.clear();
        self.transcript_cache.clear();
        self.inspector = InspectorState::default();
        self.reset_agent_view();
    }

    pub(in crate::tui) fn reset_agent_view(&mut self) {
        self.agent_running = false;
        self.active_assistant_id = None;
        self.active_tool_items.clear();
        self.active_monitors.clear();
        self.pending_ask = None;
        self.queued.clear();
        self.outbox.clear();
        self.agent_stream = None;
        self.agent_ready = false;
        self.agent_retry_at = Instant::now();
    }

    pub(in crate::tui) fn session_cwd_set(&mut self, cwd: PathBuf) {
        self.session.cwd = cwd;
        self.workspace_index.invalidate();
        self.transcript_cache.clear();
    }

    pub(in crate::tui) fn open_file_in_inspector(&mut self, raw_path: &str, line: Option<usize>) {
        self.workspace_index.ensure_fresh(&self.session.cwd);
        let (rel, parsed_line) = self
            .workspace_index
            .resolve_path(&self.session.cwd, &self.session.changed_files, raw_path)
            .unwrap_or_else(|| {
                (
                    links::normalize_display_path(
                        &self.session.cwd,
                        std::path::Path::new(raw_path),
                    ),
                    None,
                )
            });
        let target_line = line.or(parsed_line);

        self.show_sidebar = true;
        self.inspector.force_open = true;
        self.inspector.file_path = Some(rel.clone());
        self.inspector.file_line = target_line;

        if target_line.is_none() {
            if let Some(idx) = self.find_changed_file_index(&rel) {
                self.inspector.selected_diff = idx;
                self.inspector.tab = InspectorTab::Diff;
                self.inspector.scroll = 0;
                return;
            }
            if diff::load_git_pair(&self.session.cwd, &rel).is_some() {
                self.inspector.tab = InspectorTab::Diff;
                self.inspector.scroll = 0;
                return;
            }
        }

        self.inspector.tab = InspectorTab::File;
        self.inspector.scroll = target_line.map(|l| l.saturating_sub(6) as u16).unwrap_or(0);
    }

    pub(in crate::tui) fn find_changed_file_index(&self, rel_or_abs: &str) -> Option<usize> {
        let norm =
            links::normalize_display_path(&self.session.cwd, std::path::Path::new(rel_or_abs));
        self.session.changed_files.iter().position(|fc| {
            fc.path == rel_or_abs
                || links::normalize_display_path(&self.session.cwd, std::path::Path::new(&fc.path))
                    == norm
        })
    }

    pub(in crate::tui) fn open_diff_index(&mut self, idx: usize) {
        if idx < self.session.changed_files.len() {
            let rel = links::normalize_display_path(
                &self.session.cwd,
                std::path::Path::new(&self.session.changed_files[idx].path),
            );
            self.inspector.selected_diff = idx;
            self.inspector.file_path = Some(rel);
            self.inspector.file_line = None;
            self.inspector.tab = InspectorTab::Diff;
            self.inspector.scroll = 0;
            self.show_sidebar = true;
            self.inspector.force_open = true;
        }
    }

    pub(in crate::tui) fn open_tool_output(&mut self, item_id: u64) {
        self.inspector.output_item_id = Some(item_id);
        self.inspector.output_job_id = None;
        self.inspector.tab = InspectorTab::Output;
        self.inspector.scroll = 0;
        self.show_sidebar = true;
        self.inspector.force_open = true;
    }

    pub(in crate::tui) fn open_job_output(&mut self, job_id: JobId) {
        self.inspector.output_job_id = Some(job_id);
        self.inspector.output_item_id = None;
        self.inspector.tab = InspectorTab::Output;
        self.inspector.scroll = 0;
        self.show_sidebar = true;
        self.inspector.force_open = true;
    }

    pub(in crate::tui) fn open_tool_inspect(&mut self, item_id: u64) {
        self.inspector.inspect_item_id = Some(item_id);
        self.inspector.tab = InspectorTab::Inspect;
        self.inspector.scroll = 0;
        self.show_sidebar = true;
        self.inspector.force_open = true;
    }

    pub(in crate::tui) fn persist_session(&self) {
        let _ = save_session(&self.data_dir, &self.session);
    }

    pub(in crate::tui) fn save_ui_prefs(&self) {
        let mut prefs = load_prefs(&self.data_dir);
        prefs.theme = self.theme;
        prefs.show_sidebar = self.show_sidebar;
        let _ = save_prefs(&self.data_dir, &prefs);
    }

    pub(in crate::tui) fn set_toast(&mut self, msg: impl Into<String>, err: bool) {
        self.toast = Some((msg.into(), err, Instant::now()));
    }

    pub(in crate::tui) fn active_toast(&self) -> Option<(&str, bool)> {
        let (msg, err, at) = self.toast.as_ref()?;
        if at.elapsed() < Duration::from_secs(6) {
            Some((msg.as_str(), *err))
        } else {
            None
        }
    }

    pub(in crate::tui) fn push_notice(&mut self, text: impl Into<String>, is_error: bool) {
        let id = self.session.alloc_id();
        self.session.items.push(TranscriptItem::Notice {
            id,
            text: text.into(),
            is_error,
        });
        self.session.touch();
        self.follow = true;
    }

    pub(in crate::tui) fn cwd_label(&self) -> String {
        collapse_home(&self.session.cwd)
    }

    pub(in crate::tui) fn own_device_id(&self) -> String {
        self.status
            .as_ref()
            .map(|s| s.device_id.clone())
            .unwrap_or_default()
    }

    pub(in crate::tui) fn switch_to(&mut self, target: String) {
        self.target = target.clone();
        self.session.target = target.clone();
        self.jobs.clear();
        self.job_selected = 0;
        self.shell_watches.clear();
        let id = self.session.id.clone();
        self.sync_agent(AgentReq::SetTarget { id, target: target.clone() });
        self.set_toast(format!("target: {target}"), false);
    }
}
