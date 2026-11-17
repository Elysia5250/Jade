//! Startup workspace picker.
use super::*;

impl App {
    pub(in crate::tui) fn open_welcome(&mut self) {
        let here =
            std::fs::canonicalize(&self.session.cwd).unwrap_or_else(|_| self.session.cwd.clone());
        let mut rows = vec![WelcomeRow::Here(here.clone())];
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
            error: None,
        });
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
                        welcome.error = None;
                    }
                    KeyCode::Backspace => {
                        welcome.path.pop();
                        welcome.error = None;
                    }
                    KeyCode::Tab => {
                        let comps = complete_directories(&cwd, &welcome.path);
                        if let Some((_, arg)) = comps.first() {
                            welcome.path = arg.clone();
                        }
                    }
                    KeyCode::Enter => submit = Some(welcome.path.clone()),
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        welcome.path.push(c);
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
                            w.error = None;
                        }
                    }
                    Some(WelcomeRow::Here(path) | WelcomeRow::Recent(path, _)) => {
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
        self.welcome = None;
        let loaded = latest_session_for_cwd(&self.data_dir, &path).filter(|loaded| {
            std::fs::canonicalize(&loaded.cwd).unwrap_or_else(|_| loaded.cwd.clone())
                == std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone())
        });
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
