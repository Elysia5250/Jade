//! Startup home screen: workspace, account, skills, and memory.
use super::*;

mod folder;
mod memory;
mod skills;

impl App {
    pub(in crate::tui) fn open_welcome(&mut self) {
        let here =
            std::fs::canonicalize(&self.session.cwd).unwrap_or_else(|_| self.session.cwd.clone());
        let here_resumes = exact_session_for_cwd(&self.data_dir, &here).is_some();
        let mut rows = vec![WelcomeRow::Here(here.clone(), here_resumes)];
        for hit in recent_workspaces(&self.data_dir) {
            if hit.cwd == here {
                continue;
            }
            let label = if hit.title.is_empty() {
                collapse_home(&hit.cwd)
            } else {
                format!("{}  {}", collapse_home(&hit.cwd), hit.title)
            };
            rows.push(WelcomeRow::Recent(hit.cwd, label));
        }
        rows.push(WelcomeRow::OpenPath);
        rows.push(WelcomeRow::NewFolder);
        rows.push(WelcomeRow::Account);
        rows.push(WelcomeRow::Skills);
        rows.push(WelcomeRow::Memory);
        self.welcome = Some(Welcome {
            rows,
            selected: 0,
            path_mode: false,
            path: String::new(),
            path_selected: 0,
            error: None,
            folder_mode: false,
            skills_mode: false,
            memory_mode: false,
            naming: false,
            folder_parent: None,
            draft: String::new(),
            list_selected: 0,
            skills: Vec::new(),
            notes: Vec::new(),
            skill_count: 0,
            memory_count: 0,
        });
        self.recount_welcome();
    }

    pub(in crate::tui) fn handle_welcome_paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !matches!(c, '\r' | '\n')).collect();
        if clean.is_empty() {
            return;
        }
        if let Some(welcome) = self.welcome.as_mut() {
            if welcome.naming {
                welcome.draft.push_str(&clean);
                welcome.error = None;
            } else if welcome.path_mode || welcome.folder_mode {
                welcome.path.push_str(&clean);
                welcome.path_selected = 0;
                welcome.error = None;
            }
        }
    }

    pub(in crate::tui) fn handle_welcome_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        let (naming, path_mode, folder_mode, skills_mode, memory_mode) = self
            .welcome
            .as_ref()
            .map(|w| {
                (
                    w.naming,
                    w.path_mode,
                    w.folder_mode,
                    w.skills_mode,
                    w.memory_mode,
                )
            })
            .unwrap_or((false, false, false, false, false));
        if naming {
            return self.handle_welcome_naming(key);
        }
        if path_mode || folder_mode {
            return self.handle_path_keys(key);
        }
        if skills_mode {
            return self.handle_welcome_skills(key);
        }
        if memory_mode {
            return self.handle_welcome_memory(key);
        }
        self.handle_welcome_home(key)
    }

    pub(super) fn enter_workspace_path(&mut self, raw: &str) -> Action {
        let candidate = resolve_cd_path(&self.session.cwd, raw.trim());
        let path = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        if !path.is_dir() {
            if let Some(welcome) = self.welcome.as_mut() {
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
                if let Some(welcome) = self.welcome.as_mut() {
                    welcome.naming = false;
                    welcome.draft.clear();
                    welcome.error = None;
                }
                Action::None
            }
            KeyCode::Backspace => {
                if let Some(welcome) = self.welcome.as_mut() {
                    welcome.draft.pop();
                    welcome.error = None;
                }
                Action::None
            }
            KeyCode::Enter => self.submit_welcome_name(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(welcome) = self.welcome.as_mut() {
                    welcome.draft.push(c);
                    welcome.error = None;
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn submit_welcome_name(&mut self) -> Action {
        let Some(welcome) = self.welcome.as_ref() else {
            return Action::None;
        };
        let folder = welcome.folder_mode;
        let skills = welcome.skills_mode;
        let memory = welcome.memory_mode;
        let draft = welcome.draft.clone();
        let parent = welcome.folder_parent.clone();
        if folder {
            let Some(parent) = parent else {
                if let Some(welcome) = self.welcome.as_mut() {
                    welcome.error = Some("not a directory".into());
                }
                return Action::None;
            };
            return match folder::create_workspace_dir(&parent, &draft) {
                Ok(path) => {
                    self.activate_workspace_with_mode(path, false);
                    Action::None
                }
                Err(err) => {
                    if let Some(welcome) = self.welcome.as_mut() {
                        welcome.error = Some(err);
                    }
                    Action::None
                }
            };
        }
        if skills {
            return match envoy_agent::skills::prefs::create_user_skill(&self.data_dir, &draft) {
                Ok(path) => {
                    let name = draft.trim().to_string();
                    if let Some(welcome) = self.welcome.as_mut() {
                        welcome.naming = false;
                        welcome.draft.clear();
                        welcome.error = None;
                    }
                    self.reload_welcome_skills();
                    if let Some(welcome) = self.welcome.as_mut() {
                        if let Some(idx) =
                            welcome.skills.iter().position(|skill| skill.name == name)
                        {
                            welcome.list_selected = idx;
                        }
                    }
                    Action::EditFile(path)
                }
                Err(err) => {
                    if let Some(welcome) = self.welcome.as_mut() {
                        welcome.error = Some(err);
                    }
                    Action::None
                }
            };
        }
        if memory {
            return match envoy_agent::memory::create(&self.data_dir, &draft) {
                Ok(note) => {
                    let id = note.id.clone();
                    let path = note.path.clone();
                    if let Some(welcome) = self.welcome.as_mut() {
                        welcome.naming = false;
                        welcome.draft.clear();
                        welcome.error = None;
                    }
                    self.reload_welcome_notes();
                    if let Some(welcome) = self.welcome.as_mut() {
                        if let Some(idx) = welcome.notes.iter().position(|item| item.id == id) {
                            welcome.list_selected = idx;
                        }
                    }
                    Action::EditFile(path)
                }
                Err(err) => {
                    if let Some(welcome) = self.welcome.as_mut() {
                        welcome.error = Some(err);
                    }
                    Action::None
                }
            };
        }
        Action::None
    }

    fn handle_welcome_home(&mut self, key: KeyEvent) -> Action {
        let len = self
            .welcome
            .as_ref()
            .map(|w| w.rows.len())
            .unwrap_or(1)
            .max(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(welcome) = self.welcome.as_mut() {
                    welcome.selected = welcome.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(welcome) = self.welcome.as_mut() {
                    welcome.selected = (welcome.selected + 1).min(len - 1);
                }
            }
            KeyCode::Char('n') => {
                let row = self
                    .welcome
                    .as_ref()
                    .and_then(|w| w.rows.get(w.selected))
                    .cloned();
                if let Some(WelcomeRow::Here(path, _) | WelcomeRow::Recent(path, _)) = row {
                    self.activate_workspace_with_mode(path, false);
                }
            }
            KeyCode::Enter => return self.activate_welcome_row(),
            _ => {}
        }
        Action::None
    }

    fn activate_welcome_row(&mut self) -> Action {
        let row = self
            .welcome
            .as_ref()
            .and_then(|w| w.rows.get(w.selected))
            .cloned();
        match row {
            Some(WelcomeRow::OpenPath) => {
                if let Some(welcome) = self.welcome.as_mut() {
                    welcome.path_mode = true;
                    welcome.folder_mode = false;
                    welcome.path_selected = 0;
                    welcome.error = None;
                }
            }
            Some(WelcomeRow::NewFolder) => {
                if let Some(welcome) = self.welcome.as_mut() {
                    welcome.folder_mode = true;
                    welcome.path_mode = false;
                    welcome.path.clear();
                    welcome.path_selected = 0;
                    welcome.naming = false;
                    welcome.error = None;
                }
            }
            Some(WelcomeRow::Account) => self.open_login_modal(),
            Some(WelcomeRow::Skills) => {
                if let Some(welcome) = self.welcome.as_mut() {
                    welcome.skills_mode = true;
                    welcome.list_selected = 0;
                    welcome.naming = false;
                    welcome.draft.clear();
                    welcome.error = None;
                }
                self.reload_welcome_skills();
            }
            Some(WelcomeRow::Memory) => {
                if let Some(welcome) = self.welcome.as_mut() {
                    welcome.memory_mode = true;
                    welcome.list_selected = 0;
                    welcome.naming = false;
                    welcome.draft.clear();
                    welcome.error = None;
                }
                self.reload_welcome_notes();
            }
            Some(WelcomeRow::Here(path, _) | WelcomeRow::Recent(path, _)) => {
                self.activate_workspace(path);
            }
            None => {}
        }
        Action::None
    }

    pub(super) fn begin_welcome_name(&mut self) {
        if let Some(welcome) = self.welcome.as_mut() {
            welcome.naming = true;
            welcome.draft.clear();
            welcome.error = None;
        }
    }

    pub(super) fn leave_welcome_panel(&mut self) {
        if let Some(welcome) = self.welcome.as_mut() {
            welcome.skills_mode = false;
            welcome.memory_mode = false;
            welcome.naming = false;
            welcome.draft.clear();
            welcome.error = None;
        }
        self.recount_welcome();
    }

    pub(super) fn bump_welcome_list(&mut self, delta: isize, len: usize) {
        let Some(welcome) = self.welcome.as_mut() else {
            return;
        };
        if len == 0 {
            welcome.list_selected = 0;
            return;
        }
        if delta < 0 {
            welcome.list_selected = welcome.list_selected.saturating_sub(1);
        } else {
            welcome.list_selected = (welcome.list_selected + 1).min(len - 1);
        }
    }

    fn recount_welcome(&mut self) {
        let skill_count = envoy_agent::skills::discover_skills(&self.data_dir, &self.session.cwd)
            .into_iter()
            .filter(|skill| skill.scope == envoy_agent::skills::SkillScope::User)
            .count();
        let memory_count = envoy_agent::memory::list(&self.data_dir).len();
        if let Some(welcome) = self.welcome.as_mut() {
            welcome.skill_count = skill_count;
            welcome.memory_count = memory_count;
        }
    }

    fn activate_workspace(&mut self, path: PathBuf) {
        self.activate_workspace_with_mode(path, true);
    }

    fn activate_workspace_with_mode(&mut self, path: PathBuf, resume: bool) {
        self.welcome = None;
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
            let target = self.session.target.clone();
            self.session = Session::new(path, target);
            self.selected_card = None;
            self.follow = true;
            self.expanded_explore_groups.clear();
            self.transcript_cache.clear();
            self.inspector = InspectorState::default();
            self.workspace_index.invalidate();
            self.reset_agent_view();
        }
        self.spawn_agent_attach();
    }
}
