//! Envoy Hub: always-returnable mission control for sessions, workspaces, and settings.
use super::*;

mod controls;
mod folder;
mod mcp;
mod memory;
mod sessions;
mod skills;

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
            searching: false,
            confirm_delete: None,
            rename_target: None,
            path: String::new(),
            path_selected: 0,
            error: None,
            folder_parent: None,
            draft: String::new(),
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
                welcome.draft.push_str(&clean);
                welcome.error = None;
            } else if welcome.mode.is_path_input() {
                welcome.path.push_str(&clean);
                welcome.path_selected = 0;
                welcome.error = None;
            } else if welcome.searching {
                welcome.search.push_str(&clean);
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

    pub(super) fn enter_workspace_path(&mut self, raw: &str) -> Action {
        let candidate = resolve_cd_path(&self.session.current.cwd, raw.trim());
        let path = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        if !path.is_dir() {
            if let Some(welcome) = self.ui.welcome.as_mut() {
                welcome.error = Some(format!("not a directory: {}", path.display()));
            }
            return Action::None;
        }
        self.activate_workspace(path);
        Action::None
    }

    fn handle_welcome_naming(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                if let Some(welcome) = self.ui.welcome.as_mut() {
                    welcome.mode = match welcome.mode {
                        WelcomeMode::NewFolderName => WelcomeMode::NewFolderPickParent,
                        WelcomeMode::SkillsNew => WelcomeMode::Skills,
                        WelcomeMode::MemoryNew => WelcomeMode::Memory,
                        _ => WelcomeMode::Home,
                    };
                    welcome.draft.clear();
                    welcome.rename_target = None;
                    welcome.error = None;
                }
                Action::None
            }
            KeyCode::Backspace => {
                if let Some(welcome) = self.ui.welcome.as_mut() {
                    welcome.draft.pop();
                    welcome.error = None;
                }
                Action::None
            }
            KeyCode::Enter => self.submit_welcome_name(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(welcome) = self.ui.welcome.as_mut() {
                    welcome.draft.push(c);
                    welcome.error = None;
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn submit_welcome_name(&mut self) -> Action {
        let Some(welcome) = self.ui.welcome.as_ref() else {
            return Action::None;
        };
        let mode = welcome.mode;
        let draft = welcome.draft.clone();
        let parent = welcome.folder_parent.clone();
        let rename_id = welcome.rename_target.clone();
        match mode {
            WelcomeMode::NewFolderName => self.submit_new_folder(parent, &draft),
            WelcomeMode::RenameSession => self.submit_rename_session(rename_id, &draft),
            WelcomeMode::SkillsNew => self.submit_new_skill(&draft),
            WelcomeMode::MemoryNew => self.submit_new_memory(&draft),
            _ => Action::None,
        }
    }

    fn submit_new_folder(&mut self, parent: Option<PathBuf>, draft: &str) -> Action {
        let Some(parent) = parent else {
            if let Some(w) = self.ui.welcome.as_mut() {
                w.error = Some("not a directory".into());
            }
            return Action::None;
        };
        match folder::create_workspace_dir(&parent, draft) {
            Ok(path) => {
                self.activate_workspace_with_mode(path, false);
                Action::None
            }
            Err(err) => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.error = Some(err);
                }
                Action::None
            }
        }
    }

    fn submit_rename_session(&mut self, id: Option<String>, draft: &str) -> Action {
        let Some(id) = id else {
            return Action::None;
        };
        match rename_session(&self.data_dir, &id, draft) {
            Ok(()) => {
                if self.session.current.id == id {
                    self.session.current.title = draft.trim().chars().take(64).collect();
                }
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.mode = WelcomeMode::Home;
                    w.draft.clear();
                    w.rename_target = None;
                    w.error = None;
                }
                self.reload_hub_lists();
                self.set_toast("session renamed", false);
            }
            Err(err) => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.error = Some(format!("{err:#}"));
                }
            }
        }
        Action::None
    }

    fn submit_new_skill(&mut self, draft: &str) -> Action {
        match envoy_agent::skills::prefs::create_user_skill(&self.data_dir, draft) {
            Ok(path) => {
                let name = draft.trim().to_string();
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.mode = WelcomeMode::Skills;
                    w.draft.clear();
                    w.error = None;
                }
                self.reload_welcome_skills();
                if let Some(w) = self.ui.welcome.as_mut() {
                    if let Some(idx) = w.skills.iter().position(|skill| skill.name == name) {
                        w.list_selected = idx;
                    }
                }
                Action::EditFile(path)
            }
            Err(err) => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.error = Some(err);
                }
                Action::None
            }
        }
    }

    fn submit_new_memory(&mut self, draft: &str) -> Action {
        match envoy_agent::memory::create(&self.data_dir, &self.session.current.cwd, draft) {
            Ok(note) => {
                let id = note.id.clone();
                let path = note.path.clone();
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.mode = WelcomeMode::Memory;
                    w.draft.clear();
                    w.error = None;
                }
                self.reload_welcome_notes();
                if let Some(w) = self.ui.welcome.as_mut() {
                    if let Some(idx) = w.notes.iter().position(|item| item.id == id) {
                        w.list_selected = idx;
                    }
                }
                Action::EditFile(path)
            }
            Err(err) => {
                if let Some(w) = self.ui.welcome.as_mut() {
                    w.error = Some(err);
                }
                Action::None
            }
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
                    sessions::clamp_hub_selection(w);
                }
                KeyCode::Enter => {
                    w.searching = false;
                }
                KeyCode::Backspace => {
                    w.search.pop();
                    sessions::clamp_hub_selection(w);
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    w.search.push(c);
                    sessions::clamp_hub_selection(w);
                }
                _ => {}
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
                    w.tab = HubTab::Sessions;
                    w.searching = true;
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
        let filtered_len = self.filtered_hub_sessions().len();
        let Some(w) = self.ui.welcome.as_mut() else {
            return;
        };
        w.confirm_delete = None;
        w.error = None;
        match w.focus {
            HubFocus::Controls => {
                let len = HubControl::ALL.len();
                w.selected_control = step_index(w.selected_control, delta, len);
            }
            HubFocus::Left => match w.tab {
                HubTab::Sessions => {
                    w.selected_session = step_index(w.selected_session, delta, filtered_len);
                }
                HubTab::Workspaces => {
                    let len = w.workspaces.len();
                    w.selected_workspace = step_index(w.selected_workspace, delta, len);
                }
            },
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
        if let Some(w) = self.ui.welcome.as_mut() {
            w.mode = WelcomeMode::OpenPath;
            w.path_selected = 0;
            w.confirm_delete = None;
            w.error = None;
        }
    }

    pub(in crate::tui) fn hub_open_new_folder_panel(&mut self) {
        if let Some(w) = self.ui.welcome.as_mut() {
            w.mode = WelcomeMode::NewFolderPickParent;
            w.path.clear();
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
            welcome.error = None;
        }
    }

    pub(super) fn leave_welcome_panel(&mut self) {
        if let Some(welcome) = self.ui.welcome.as_mut() {
            welcome.mode = WelcomeMode::Home;
            welcome.draft.clear();
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
        let skills: Vec<_> = envoy_agent::skills::discover_skills(&self.data_dir, &cwd)
            .into_iter()
            .filter(|skill| skill.scope == envoy_agent::skills::SkillScope::User)
            .collect();
        let skill_count = skills.len();
        let skill_injected_count = skills
            .iter()
            .filter(|s| envoy_agent::skills::skill_is_injected(&self.data_dir, &cwd, s))
            .count();
        let notes = envoy_agent::memory::list_for(&self.data_dir, &cwd);
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
