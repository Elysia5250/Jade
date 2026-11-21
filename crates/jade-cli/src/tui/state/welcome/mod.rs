//! Jade Hub: always-returnable mission control for sessions, workspaces, and settings.
use super::*;
use crate::tui::line_edit::{self, EditOutcome};

mod controls;
mod folder;
mod mcp;
mod memory;
mod panels;
mod sessions;
mod skills;

pub(in crate::tui) use folder::build_path_browser_entries;

impl App {
    pub(in crate::tui) fn open_welcome(&mut self) {
        self.open_hub_inner(false);
    }

    pub(in crate::tui) fn return_to_hub(&mut self) {
        self.persist_session();
        self.open_hub_inner(true);
    }

    pub(in crate::tui) fn close_hub_to_current(&mut self) {
        if self.ui.welcome.as_ref().is_some_and(|w| w.can_return) {
            self.ui.welcome = None;
        }
    }

    fn open_hub_inner(&mut self, can_return: bool) {
        self.ui.welcome = Some(Welcome {
            can_return,
            focus: HubFocus::Left,
            tab: HubTab::Sessions,
            mode: WelcomeMode::Home,
            sessions: Vec::new(),
            workspaces: Vec::new(),
            selected_session: 0,
            selected_workspace: 0,
            selected_control: 0,
            search: String::new(),
            search_cursor: 0,
            searching: false,
            confirm_delete: None,
            rename_target: None,
            browse_dir: std::fs::canonicalize(&self.session.current.cwd)
                .unwrap_or_else(|_| self.session.current.cwd.clone()),
            path: String::new(),
            path_cursor: 0,
            path_selected: 0,
            error: None,
            folder_parent: None,
            draft: String::new(),
            draft_cursor: 0,
            list_selected: 0,
            skills: Vec::new(),
            notes: Vec::new(),
            skill_count: 0,
            skill_injected_count: 0,
            memory_count: 0,
            memory_enabled_count: 0,
            mcp_probe: Vec::new(),
            mcp_probing: false,
            live_refreshed_at: None,
        });
        self.reload_hub_lists();
        self.recount_welcome();
        self.inputs.invalidate_catalog();
        self.maybe_poll_hub_live_sessions();
    }

    pub(in crate::tui) fn handle_welcome_paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !matches!(c, '\r' | '\n')).collect();
        if clean.is_empty() {
            return;
        }
        if let Some(welcome) = self.ui.welcome.as_mut() {
            if welcome.mode.is_naming() {
                line_edit::insert_str(&mut welcome.draft, &mut welcome.draft_cursor, &clean);
                welcome.error = None;
            } else if welcome.mode.is_path_input() {
                line_edit::insert_str(&mut welcome.path, &mut welcome.path_cursor, &clean);
                welcome.path_selected = 0;
                welcome.error = None;
            } else if welcome.searching {
                line_edit::insert_str(&mut welcome.search, &mut welcome.search_cursor, &clean);
                sessions::clamp_hub_selection(welcome);
            }
        }
    }

    pub(in crate::tui) fn handle_welcome_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        let mode = self
            .ui
            .welcome
            .as_ref()
            .map(|w| w.mode)
            .unwrap_or(WelcomeMode::Home);
        if mode.is_naming() {
            return self.handle_welcome_naming(key);
        }
        if mode.is_path_input() {
            return self.handle_path_keys(key);
        }
        match mode {
            WelcomeMode::Skills => self.handle_welcome_skills(key),
            WelcomeMode::Memory => self.handle_welcome_memory(key),
            WelcomeMode::Mcp => self.handle_welcome_mcp(key),
            _ => self.handle_hub_home(key),
        }
    }

    fn handle_hub_home(&mut self, key: KeyEvent) -> Action {
        if self.ui.welcome.as_ref().is_some_and(|w| w.searching) {
            return self.handle_hub_search_key(key);
        }
        match key.code {
            KeyCode::Esc => {
                let mut handled = false;
                if let Some(w) = self.ui.welcome.as_mut() {
                    if w.confirm_delete.take().is_some() {
                        handled = true;
                    } else if !w.search.is_empty() {
                        w.search.clear();
                        w.search_cursor = 0;
                        sessions::clamp_hub_selection(w);
                        handled = true;
                    }
                }
                if !handled {
                    self.close_hub_to_current();
                }
                return Action::None;
            }
            KeyCode::Tab => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.focus = match w.focus {
                        HubFocus::Left => HubFocus::Controls,
                        HubFocus::Controls => HubFocus::Left,
                    };
                    w.confirm_delete = None;
                    w.error = None;
                }
                return Action::None;
            }
            KeyCode::Char('1') => {
                self.hub_select_tab(HubTab::Sessions);
                return Action::None;
            }
            KeyCode::Char('2') => {
                self.hub_select_tab(HubTab::Workspaces);
                return Action::None;
            }
            KeyCode::Char('3') => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.focus = HubFocus::Controls;
                    w.confirm_delete = None;
                }
                return Action::None;
            }
            KeyCode::Char('o') => {
                self.hub_open_path_panel();
                return Action::None;
            }
            KeyCode::Char('N') => {
                self.hub_open_new_folder_panel();
                return Action::None;
            }
            _ => {}
        }

        let focus = self
            .ui
            .welcome
            .as_ref()
            .map(|w| w.focus)
            .unwrap_or(HubFocus::Left);
        match focus {
            HubFocus::Left => self.handle_hub_left_key(key),
            HubFocus::Controls => self.handle_hub_controls_key(key),
        }
    }

    fn handle_hub_search_key(&mut self, key: KeyEvent) -> Action {
        if let Some(w) = self.ui.welcome.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    w.searching = false;
                    w.search.clear();
                    w.search_cursor = 0;
                    sessions::clamp_hub_selection(w);
                }
                KeyCode::Enter => {
                    w.searching = false;
                }
                _ => {
                    if line_edit::handle_key(&mut w.search, &mut w.search_cursor, key)
                        == EditOutcome::Changed
                    {
                        sessions::clamp_hub_selection(w);
                    }
                }
            }
        }
        Action::None
    }

    fn handle_hub_left_key(&mut self, key: KeyEvent) -> Action {
        let tab = self
            .ui
            .welcome
            .as_ref()
            .map(|w| w.tab)
            .unwrap_or(HubTab::Sessions);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_hub(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_hub(1),
            KeyCode::Char('n') => self.start_new_session_from_hub(),
            KeyCode::Char('/') => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.searching = true;
                    w.search_cursor = w.search.chars().count();
                    w.confirm_delete = None;
                    w.error = None;
                }
            }
            KeyCode::Char('r') if tab == HubTab::Sessions => {
                self.begin_rename_selected_session();
            }
            KeyCode::Char('x') if tab == HubTab::Sessions => {
                self.interrupt_selected_session();
            }
            KeyCode::Char('d') if tab == HubTab::Sessions => {
                self.request_delete_selected_session();
            }
            KeyCode::Enter => match tab {
                HubTab::Sessions => self.activate_selected_session(),
                HubTab::Workspaces => self.activate_selected_workspace(true),
            },
            _ => {}
        }
        Action::None
    }

    fn handle_hub_controls_key(&mut self, key: KeyEvent) -> Action {
        let len = HubControl::ALL.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.selected_control = w.selected_control.saturating_sub(1);
                    w.error = None;
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.selected_control = (w.selected_control + 1).min(len - 1);
                    w.error = None;
                }
                Action::None
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let ctrl = self.selected_hub_control();
                self.activate_hub_control(ctrl, -1)
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                let ctrl = self.selected_hub_control();
                self.activate_hub_control(ctrl, 1)
            }
            _ => Action::None,
        }
    }

    fn selected_hub_control(&self) -> HubControl {
        let idx = self
            .ui
            .welcome
            .as_ref()
            .map(|w| w.selected_control)
            .unwrap_or(0);
        HubControl::ALL[idx.min(HubControl::ALL.len() - 1)]
    }

    pub(in crate::tui) fn scroll_hub(&mut self, delta: isize) {
        let filtered_sessions = self.filtered_hub_sessions().len();
        let filtered_workspaces = self.filtered_hub_workspaces().len();
        let Some(w) = self.ui.welcome.as_mut() else {
            return;
        };
        w.confirm_delete = None;
        w.error = None;
        let moved_left = match w.focus {
            HubFocus::Controls => {
                let len = HubControl::ALL.len();
                w.selected_control = step_index(w.selected_control, delta, len);
                false
            }
            HubFocus::Left => {
                match w.tab {
                    HubTab::Sessions => {
                        w.selected_session =
                            step_index(w.selected_session, delta, filtered_sessions);
                    }
                    HubTab::Workspaces => {
                        w.selected_workspace =
                            step_index(w.selected_workspace, delta, filtered_workspaces);
                    }
                }
                true
            }
        };
        if moved_left {
            self.recount_welcome();
        }
    }

    pub(in crate::tui) fn hub_select_tab(&mut self, tab: HubTab) {
        if let Some(w) = self.ui.welcome.as_mut() {
            w.focus = HubFocus::Left;
            w.tab = tab;
            w.searching = false;
            w.confirm_delete = None;
            w.error = None;
        }
        self.recount_welcome();
    }

    pub(in crate::tui) fn hub_click_session(&mut self, filtered_idx: usize) -> Option<Action> {
        let already = self.ui.welcome.as_ref().is_some_and(|w| {
            w.focus == HubFocus::Left
                && w.tab == HubTab::Sessions
                && w.selected_session == filtered_idx
        });
        if let Some(w) = self.ui.welcome.as_mut() {
            w.focus = HubFocus::Left;
            w.tab = HubTab::Sessions;
            w.selected_session = filtered_idx;
            w.confirm_delete = None;
            w.error = None;
        }
        if already {
            self.activate_selected_session();
        } else {
            self.recount_welcome();
        }
        None
    }

    pub(in crate::tui) fn hub_click_workspace(&mut self, idx: usize) {
        let already = self.ui.welcome.as_ref().is_some_and(|w| {
            w.focus == HubFocus::Left && w.tab == HubTab::Workspaces && w.selected_workspace == idx
        });
        if let Some(w) = self.ui.welcome.as_mut() {
            w.focus = HubFocus::Left;
            w.tab = HubTab::Workspaces;
            w.selected_workspace = idx;
            w.error = None;
        }
        if already {
            self.activate_selected_workspace(true);
        } else {
            self.recount_welcome();
        }
    }

    pub(in crate::tui) fn hub_click_control(&mut self, ctrl: HubControl) -> Option<Action> {
        if let Some(idx) = HubControl::ALL.iter().position(|c| *c == ctrl) {
            if let Some(w) = self.ui.welcome.as_mut() {
                w.focus = HubFocus::Controls;
                w.selected_control = idx;
                w.error = None;
            }
        }
        Some(self.activate_hub_control(ctrl, 1))
    }

    pub(in crate::tui) fn hub_open_path_panel(&mut self) {
        let base = self.selected_hub_cwd();
        let browse = std::fs::canonicalize(&base).unwrap_or(base);
        if let Some(w) = self.ui.welcome.as_mut() {
            w.mode = WelcomeMode::OpenPath;
            w.browse_dir = browse;
            w.path.clear();
            w.path_cursor = 0;
            w.path_selected = 0;
            w.confirm_delete = None;
            w.error = None;
        }
    }

    pub(in crate::tui) fn hub_open_new_folder_panel(&mut self) {
        let base = self.selected_hub_cwd();
        let browse = std::fs::canonicalize(&base).unwrap_or(base);
        if let Some(w) = self.ui.welcome.as_mut() {
            w.mode = WelcomeMode::NewFolderPickParent;
            w.browse_dir = browse;
            w.path.clear();
            w.path_cursor = 0;
            w.path_selected = 0;
            w.confirm_delete = None;
            w.error = None;
        }
    }

    pub(super) fn begin_welcome_name(&mut self) {
        if let Some(welcome) = self.ui.welcome.as_mut() {
            welcome.mode = match welcome.mode {
                WelcomeMode::NewFolderPickParent | WelcomeMode::NewFolderName => {
                    WelcomeMode::NewFolderName
                }
                WelcomeMode::Skills | WelcomeMode::SkillsNew => WelcomeMode::SkillsNew,
                WelcomeMode::Memory | WelcomeMode::MemoryNew => WelcomeMode::MemoryNew,
                other => other,
            };
            welcome.draft.clear();
            welcome.draft_cursor = 0;
            welcome.error = None;
        }
    }

    pub(super) fn leave_welcome_panel(&mut self) {
        if let Some(welcome) = self.ui.welcome.as_mut() {
            welcome.mode = WelcomeMode::Home;
            welcome.draft.clear();
            welcome.draft_cursor = 0;
            welcome.confirm_delete = None;
            welcome.error = None;
        }
        self.recount_welcome();
    }

    pub(super) fn bump_welcome_list(&mut self, delta: isize, len: usize) {
        let Some(welcome) = self.ui.welcome.as_mut() else {
            return;
        };
        welcome.list_selected = step_index(welcome.list_selected, delta, len);
    }

    fn recount_welcome(&mut self) {
        let cwd = self.selected_hub_cwd();
        let skills: Vec<_> = jade_agent::skills::discover_skills(&self.data_dir, &cwd)
            .into_iter()
            .filter(|skill| skill.scope == jade_agent::skills::SkillScope::User)
            .collect();
        let skill_count = skills.len();
        let skill_injected_count = skills
            .iter()
            .filter(|s| jade_agent::skills::skill_is_injected(&self.data_dir, &cwd, s))
            .count();
        let notes = jade_agent::memory::list_for(&self.data_dir, &cwd);
        let memory_count = notes.len();
        let memory_enabled_count = notes.iter().filter(|n| n.enabled).count();
        if let Some(welcome) = self.ui.welcome.as_mut() {
            welcome.skill_count = skill_count;
            welcome.skill_injected_count = skill_injected_count;
            welcome.memory_count = memory_count;
            welcome.memory_enabled_count = memory_enabled_count;
        }
    }

    fn activate_workspace(&mut self, path: PathBuf) {
        self.activate_workspace_with_mode(path, true);
    }

    fn activate_workspace_with_mode(&mut self, path: PathBuf, resume: bool) {
        self.ui.welcome = None;
        let loaded = if resume {
            exact_session_for_cwd(&self.data_dir, &path)
        } else {
            None
        };
        if let Some(loaded) = loaded {
            self.load_existing_session(loaded);
        } else {
            // A new id so the daemon cannot keep the launch directory from an
            // earlier Resume of the session created before the picker closed.
            let target = self.session.current.target.clone();
            self.session.current = Session::new(path, target);
            self.reset_session_view();
        }
        self.spawn_agent_attach();
    }
}

fn step_index(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(len - 1)
    }
}
