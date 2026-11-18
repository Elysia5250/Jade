//! Startup workspace picker.
use super::*;

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
        self.welcome = Some(Welcome {
            rows,
            selected: 0,
            path_mode: false,
            path: String::new(),
            path_selected: 0,
            error: None,
        });
    }

    pub(in crate::tui) fn handle_welcome_paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !matches!(c, '\r' | '\n')).collect();
        if clean.is_empty() {
            return;
        }
        if let Some(w) = self.welcome.as_mut() {
            if w.path_mode {
                w.path.push_str(&clean);
                w.path_selected = 0;
                w.error = None;
            }
        }
    }

    pub(in crate::tui) fn handle_welcome_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        let path_mode = self.welcome.as_ref().is_some_and(|w| w.path_mode);
        if path_mode {
            let mut submit: Option<String> = None;
            let cwd = self.session.cwd.clone();
            if let Some(welcome) = self.welcome.as_mut() {
                match key.code {
                    KeyCode::Esc => {
                        welcome.path_mode = false;
                        welcome.path_selected = 0;
                        welcome.error = None;
                    }
                    KeyCode::Up => {
                        welcome.path_selected = welcome.path_selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        let comps = complete_directories(&cwd, &welcome.path);
                        if !comps.is_empty() {
                            welcome.path_selected =
                                (welcome.path_selected + 1).min(comps.len() - 1);
                        }
                    }
                    KeyCode::Backspace => {
                        welcome.path.pop();
                        welcome.path_selected = 0;
                        welcome.error = None;
                    }
                    KeyCode::Tab => {
                        let comps = complete_directories(&cwd, &welcome.path);
                        let idx = welcome.path_selected.min(comps.len().saturating_sub(1));
                        if let Some((_, arg)) = comps.get(idx) {
                            welcome.path = arg.clone();
                            welcome.path_selected = 0;
                            welcome.error = None;
                        }
                    }
                    KeyCode::Enter => {
                        let comps = complete_directories(&cwd, &welcome.path);
                        let sel = welcome.path_selected.min(comps.len().saturating_sub(1));
                        let typed = welcome.path.trim();
                        let typed_is_dir =
                            !typed.is_empty() && resolve_cd_path(&cwd, typed).is_dir();
                        if welcome.path_selected > 0 {
                            if let Some((_, arg)) = comps.get(sel) {
                                submit = Some(arg.clone());
                            }
                        } else if typed_is_dir || typed.is_empty() {
                            submit = Some(welcome.path.clone());
                        } else if let Some((_, arg)) = comps.get(sel) {
                            submit = Some(arg.clone());
                        } else {
                            submit = Some(welcome.path.clone());
                        }
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        welcome.path.push(c);
                        welcome.path_selected = 0;
                        welcome.error = None;
                    }
                    _ => {}
                }
            }
            if let Some(raw) = submit {
                return self.enter_workspace_path(&raw);
            }
            return Action::None;
        }

        let len = self
            .welcome
            .as_ref()
            .map(|w| w.rows.len())
            .unwrap_or(1)
            .max(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(w) = self.welcome.as_mut() {
                    w.selected = w.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(w) = self.welcome.as_mut() {
                    w.selected = (w.selected + 1).min(len - 1);
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
            KeyCode::Enter => {
                let row = self
                    .welcome
                    .as_ref()
                    .and_then(|w| w.rows.get(w.selected))
                    .cloned();
                match row {
                    Some(WelcomeRow::OpenPath) => {
                        if let Some(w) = self.welcome.as_mut() {
                            w.path_mode = true;
                            w.path_selected = 0;
                            w.error = None;
                        }
                    }
                    Some(WelcomeRow::Here(path, _) | WelcomeRow::Recent(path, _)) => {
                        self.activate_workspace(path);
                    }
                    None => {}
                }
            }
            _ => {}
        }
        Action::None
    }

    fn enter_workspace_path(&mut self, raw: &str) -> Action {
        let candidate = resolve_cd_path(&self.session.cwd, raw.trim());
        let path = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        if !path.is_dir() {
            if let Some(w) = self.welcome.as_mut() {
                w.error = Some(format!("not a directory: {}", path.display()));
            }
            return Action::None;
        }
        self.activate_workspace(path);
        Action::None
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
