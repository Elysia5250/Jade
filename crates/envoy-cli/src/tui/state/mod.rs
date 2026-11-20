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
        let prefs = super::ui_prefs::load(&data_dir);
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
            notice_tx,
            notice_rx,
            inputs: inputs::Inputs::default(),
            agent: AgentState {
                running: false,
                stream: None,
                retry_at: Instant::now(),
                offline: false,
                ready: false,
                daemon_incarnation: 0,
                snapshot: None,
                skip_replay_tokens: false,
                pending_ask: None,
                queued: VecDeque::new(),
                outbox: VecDeque::new(),
                active_assistant_id: None,
                active_tool_items: HashMap::new(),
                active_monitors: Vec::new(),
                daemon_tx: std::cell::RefCell::new(None),
            },
            session: SessionState {
                current: session,
                perm_mode,
                workspace_index: links::WorkspaceIndex::default(),
                expanded_explore_groups: HashSet::new(),
                transcript_cache: HashMap::new(),
                persist_hold: std::cell::Cell::new(false),
                persist_due: std::cell::Cell::new(false),
            },
            jobs: JobState {
                target,
                status: None,
                watch: None,
                list: Vec::new(),
                selected: 0,
                connected: false,
                interval: Duration::from_millis(600),
                last_refresh: Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .unwrap_or_else(Instant::now),
                shell_watches: Vec::new(),
                last_shell_poll: Instant::now(),
            },
            login: LoginState::default(),
            ui: UiState {
                focus: Focus::Input,
                mode,
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
                welcome: None,
                inspector: InspectorState::default(),
                hit_regions: Vec::new(),
                transcript_area: Rect::default(),
                mouse_capture: true,
                selection: None,
                drag_start: None,
                screen_lines: Vec::new(),
            },
        }
    }

    pub(in crate::tui) fn theme(&self) -> Theme {
        THEMES[self.ui.theme].1
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
        self.session.current = loaded;
        self.reset_session_view();
    }

    pub(in crate::tui) fn reset_session_view(&mut self) {
        self.ui.selected_card = None;
        self.ui.follow = true;
        self.ui.transcript_scroll = 0;
        self.ui.transcript_max_scroll = 0;
        self.session.expanded_explore_groups.clear();
        self.session.transcript_cache.clear();
        self.ui.hit_regions.clear();
        self.ui.selection = None;
        self.ui.drag_start = None;
        self.ui.screen_lines.clear();
        self.ui.inspector = InspectorState::default();
        self.session.workspace_index.invalidate();
        self.inputs.invalidate_catalog();
        self.inputs.preview_key_clear();
        self.reset_agent_view();
    }

    pub(in crate::tui) fn reset_agent_view(&mut self) {
        self.agent.running = false;
        self.agent.active_assistant_id = None;
        self.agent.active_tool_items.clear();
        self.agent.active_monitors.clear();
        self.agent.pending_ask = None;
        self.agent.queued.clear();
        self.agent.outbox.clear();
        self.agent.stream = None;
        self.agent.ready = false;
        self.agent.retry_at = Instant::now();
    }

    pub(in crate::tui) fn session_cwd_set(&mut self, cwd: PathBuf) {
        self.session.current.cwd = cwd;
        self.session.workspace_index.invalidate();
        self.session.transcript_cache.clear();
        self.inputs.invalidate_catalog();
        self.inputs.preview_key_clear();
    }

    pub(in crate::tui) fn open_file_in_inspector(&mut self, raw_path: &str, line: Option<usize>) {
        let local = inputs::target_is_local(&self.jobs.target);
        if local {
            self.session.workspace_index.ensure_fresh(&self.session.current.cwd);
        }
        let (rel, parsed_line) = if local {
            self.session.workspace_index
                .resolve_path(&self.session.current.cwd, &self.session.current.changed_files, raw_path)
                .unwrap_or_else(|| {
                    (
                        links::normalize_display_path(
                            &self.session.current.cwd,
                            std::path::Path::new(raw_path),
                        ),
                        None,
                    )
                })
        } else {
            self.session.workspace_index
                .resolve_path_cached(&self.session.current.cwd, &self.session.current.changed_files, raw_path)
                .unwrap_or_else(|| {
                    (
                        links::display_rel(&self.session.current.cwd, std::path::Path::new(raw_path)),
                        links::split_path_and_line(raw_path).and_then(|(_, ln)| ln),
                    )
                })
        };
        let target_line = line.or(parsed_line);

        self.ui.show_sidebar = true;
        self.ui.inspector.force_open = true;
        self.ui.inspector.file_path = Some(rel.clone());
        self.ui.inspector.file_line = target_line;

        if target_line.is_none() {
            if let Some(idx) = self.find_changed_file_index(&rel) {
                self.ui.inspector.selected_diff = idx;
                self.ui.inspector.tab = InspectorTab::Diff;
                self.ui.inspector.scroll = 0;
                return;
            }
            if local && diff::load_git_pair(&self.session.current.cwd, &rel).is_some() {
                self.ui.inspector.tab = InspectorTab::Diff;
                self.ui.inspector.scroll = 0;
                return;
            }
        }

        self.ui.inspector.tab = InspectorTab::File;
        self.ui.inspector.scroll = target_line.map(|l| l.saturating_sub(6)).unwrap_or(0);
    }

    pub(in crate::tui) fn find_changed_file_index_cached(&self, rel_or_abs: &str) -> Option<usize> {
        let cwd = &self.session.current.cwd;
        let norm = links::display_rel(cwd, std::path::Path::new(rel_or_abs));
        self.session.current.changed_files.iter().position(|fc| {
            fc.path == rel_or_abs || links::display_rel(cwd, std::path::Path::new(&fc.path)) == norm
        })
    }

    pub(in crate::tui) fn find_changed_file_index(&self, rel_or_abs: &str) -> Option<usize> {
        let norm =
            links::normalize_display_path(&self.session.current.cwd, std::path::Path::new(rel_or_abs));
        self.session.current.changed_files.iter().position(|fc| {
            fc.path == rel_or_abs
                || links::normalize_display_path(&self.session.current.cwd, std::path::Path::new(&fc.path))
                    == norm
        })
    }

    pub(in crate::tui) fn open_diff_index(&mut self, idx: usize) {
        if idx < self.session.current.changed_files.len() {
            let rel = links::normalize_display_path(
                &self.session.current.cwd,
                std::path::Path::new(&self.session.current.changed_files[idx].path),
            );
            self.ui.inspector.selected_diff = idx;
            self.ui.inspector.file_path = Some(rel);
            self.ui.inspector.file_line = None;
            self.ui.inspector.tab = InspectorTab::Diff;
            self.ui.inspector.scroll = 0;
            self.ui.show_sidebar = true;
            self.ui.inspector.force_open = true;
        }
    }

    pub(in crate::tui) fn open_tool_output(&mut self, item_id: u64) {
        self.ui.inspector.output_item_id = Some(item_id);
        self.ui.inspector.output_job_id = None;
        self.ui.inspector.tab = InspectorTab::Output;
        self.ui.inspector.scroll = 0;
        self.ui.show_sidebar = true;
        self.ui.inspector.force_open = true;
    }

    pub(in crate::tui) fn open_job_output(&mut self, job_id: JobId) {
        self.ui.inspector.output_job_id = Some(job_id);
        self.ui.inspector.output_item_id = None;
        self.ui.inspector.tab = InspectorTab::Output;
        self.ui.inspector.scroll = 0;
        self.ui.show_sidebar = true;
        self.ui.inspector.force_open = true;
    }

    pub(in crate::tui) fn open_tool_inspect(&mut self, item_id: u64) {
        self.ui.inspector.inspect_item_id = Some(item_id);
        self.ui.inspector.tab = InspectorTab::Inspect;
        self.ui.inspector.scroll = 0;
        self.ui.show_sidebar = true;
        self.ui.inspector.force_open = true;
    }

    pub(in crate::tui) fn persist_session(&self) {
        if self.session.persist_hold.get() {
            self.session.persist_due.set(true);
            return;
        }
        let _ = save_session(&self.data_dir, &self.session.current);
    }

    pub(in crate::tui) fn flush_session_now(&self) {
        self.session.persist_due.set(false);
        let _ = save_session(&self.data_dir, &self.session.current);
    }

    pub(in crate::tui) fn release_persist(&self) {
        self.session.persist_hold.set(false);
        if self.session.persist_due.take() {
            self.persist_session();
        }
    }

    pub(in crate::tui) fn save_ui_prefs(&self) {
        let prefs = super::ui_prefs::ConsolePrefs {
            theme: self.ui.theme,
            show_sidebar: self.ui.show_sidebar,
        };
        let _ = super::ui_prefs::save(&self.data_dir, &prefs);
    }

    pub(in crate::tui) fn set_toast(&mut self, msg: impl Into<String>, err: bool) {
        self.ui.toast = Some((msg.into(), err, Instant::now()));
    }

    pub(in crate::tui) fn active_toast(&self) -> Option<(&str, bool)> {
        let (msg, err, at) = self.ui.toast.as_ref()?;
        if at.elapsed() < Duration::from_secs(6) {
            Some((msg.as_str(), *err))
        } else {
            None
        }
    }

    pub(in crate::tui) fn push_notice(&mut self, text: impl Into<String>, is_error: bool) {
        let id = self.session.current.alloc_id();
        self.session.current.items.push(TranscriptItem::Notice {
            id,
            text: text.into(),
            is_error,
        });
        self.session.current.touch();
        self.ui.follow = true;
    }

    pub(in crate::tui) fn cwd_label(&self) -> String {
        collapse_home(&self.session.current.cwd)
    }

    pub(in crate::tui) fn own_device_id(&self) -> String {
        self.jobs.status
            .as_ref()
            .map(|s| s.device_id.clone())
            .unwrap_or_default()
    }

    pub(in crate::tui) fn switch_to(&mut self, target: String) {
        self.jobs.target = target.clone();
        self.session.current.target = target.clone();
        self.jobs.list.clear();
        self.jobs.selected = 0;
        self.jobs.shell_watches.clear();
        self.inputs.clear_for_target_switch();
        let id = self.session.current.id.clone();
        self.sync_agent(AgentReq::SetTarget { id, target: target.clone() });
        self.set_toast(format!("target: {target}"), false);
    }
}
