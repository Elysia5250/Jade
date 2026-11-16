//! Slash commands, command palette, and completion queries.
use super::*;

impl App {

    pub(in crate::tui) fn run_command(&mut self, command: Command) -> Action {
        match command {
            Command::FocusInput => {
                self.focus = Focus::Input;
                Action::None
            }
            Command::OpenLogin => {
                self.open_login_modal();
                Action::None
            }
            Command::OpenModels => {
                self.open_models_modal();
                Action::None
            }
            Command::OpenSessions => {
                self.open_sessions_modal();
                Action::None
            }
            Command::NewSession => {
                self.persist_session();
                self.session = Session::new(self.session.cwd.clone(), self.target.clone());
                self.selected_card = None;
                self.follow = true;
                self.reset_agent_view();
                self.spawn_agent_attach();
                self.set_toast("started new session", false);
                Action::None
            }
            Command::ToggleMode => {
                self.mode = match self.mode {
                    InputMode::Agent => InputMode::Shell,
                    InputMode::Shell => InputMode::Agent,
                };
                self.set_toast(format!("mode: {}", self.mode.label()), false);
                Action::None
            }
            Command::ToggleExpand => {
                self.toggle_expand_cards();
                Action::None
            }
            Command::Cancel => Action::Cancel,
            Command::ToggleSidebar => {
                self.show_sidebar = !self.show_sidebar;
                self.save_ui_prefs();
                Action::None
            }
            Command::NextTheme => {
                self.theme = (self.theme + 1) % THEMES.len();
                self.save_ui_prefs();
                self.set_toast(format!("theme: {}", THEMES[self.theme].0), false);
                Action::None
            }
            Command::Refresh => Action::Refresh,
            Command::Switch(target) => Action::Switch(target),
            Command::Quit => Action::Quit,
        }
    }


    pub(in crate::tui) fn toggle_expand_cards(&mut self) {
        if let Some(idx) = self.selected_card {
            if let Some(item) = self.session.items.get_mut(idx) {
                match item {
                    TranscriptItem::Tool { collapsed, .. } => *collapsed = !*collapsed,
                    TranscriptItem::Assistant {
                        reasoning_collapsed,
                        ..
                    } => *reasoning_collapsed = !*reasoning_collapsed,
                    _ => {}
                }
                return;
            }
        }
        // Otherwise fold/unfold every tool card and reasoning block.
        let any_collapsed = self.session.items.iter().any(|i| {
            matches!(i, TranscriptItem::Tool { collapsed: true, .. })
                || matches!(
                    i,
                    TranscriptItem::Assistant {
                        reasoning_collapsed: true,
                        ..
                    }
                )
        });
        let target_state = !any_collapsed;
        for item in &mut self.session.items {
            match item {
                TranscriptItem::Tool { collapsed, .. } => *collapsed = target_state,
                TranscriptItem::Assistant {
                    reasoning_collapsed,
                    ..
                } => *reasoning_collapsed = target_state,
                _ => {}
            }
        }
        self.persist_session();
    }


    pub(in crate::tui) fn run_slash(&mut self, text: &str) -> Action {
        let body = text.trim_start_matches('/').trim();
        let (name, arg) = match body.split_once(char::is_whitespace) {
            Some((name, arg)) => (name, arg.trim()),
            None => (body, ""),
        };
        match name {
            "login" | "connect" => {
                self.open_login_modal();
                Action::None
            }
            "logout" => {
                if arg.is_empty() {
                    if let Some((prov, _)) = resolve_active_provider(&self.data_dir) {
                        let id = prov.id.clone();
                        match remove_provider(&self.data_dir, &id) {
                            Ok(true) => {
                                if resolve_active_provider(&self.data_dir).is_none() {
                                    self.mode = InputMode::Shell;
                                }
                                self.set_toast(format!("logged out of {id}"), false);
                            }
                            Ok(false) => {
                                self.set_toast(format!("{id} comes from env var; unset env to remove"), true);
                            }
                            Err(e) => self.set_toast(format!("{e:#}"), true),
                        }
                    } else {
                        self.set_toast("no provider logged in", true);
                    }
                } else {
                    match remove_provider(&self.data_dir, arg) {
                        Ok(true) => {
                            if resolve_active_provider(&self.data_dir).is_none() {
                                self.mode = InputMode::Shell;
                            }
                            self.set_toast(format!("logged out of {arg}"), false);
                        }
                        Ok(false) => self.set_toast(format!("unknown stored provider `{arg}`"), true),
                        Err(e) => self.set_toast(format!("{e:#}"), true),
                    }
                }
                Action::None
            }
            "models" | "model" => {
                if arg.is_empty() {
                    self.open_models_modal();
                } else if let Some((prov, _)) = resolve_active_provider(&self.data_dir) {
                    let (pid, mid) = match arg.split_once('/') {
                        Some((p, m)) => (p.to_string(), m.to_string()),
                        None => (prov.id, arg.to_string()),
                    };
                    if let Err(e) = set_active_model(
                        &self.data_dir,
                        ModelRef {
                            provider_id: pid.clone(),
                            model_id: mid.clone(),
                        },
                    ) {
                        self.set_toast(format!("{e:#}"), true);
                    } else {
                        self.set_toast(format!("model: {pid}/{mid}"), false);
                    }
                } else {
                    self.set_toast("no provider connected — run /login first", true);
                }
                Action::None
            }
            "mode" => self.run_command(Command::ToggleMode),
            "cd" => {
                if arg.is_empty() {
                    self.set_toast(format!("cwd: {}", self.cwd_label()), false);
                    return Action::None;
                }
                let candidate = resolve_cd_path(&self.session.cwd, arg);
                match std::fs::canonicalize(&candidate) {
                    Ok(path) if path.is_dir() => {
                        self.session.cwd = path;
                        self.persist_session();
                        self.set_toast(format!("cwd: {}", self.cwd_label()), false);
                    }
                    Ok(_) => self.set_toast(format!("not a directory: {arg}"), true),
                    Err(e) => self.set_toast(format!("{arg}: {e}"), true),
                }
                Action::None
            }
            "machine" | "machines" => {
                if arg.is_empty() {
                    let items = self.machine_items();
                    self.open_picker("machines", items);
                    Action::None
                } else {
                    Action::Switch(arg.to_string())
                }
            }
            "new" => self.run_command(Command::NewSession),
            "sessions" | "resume" => {
                self.open_sessions_modal();
                Action::None
            }
            "expand" => {
                self.toggle_expand_cards();
                Action::None
            }
            "sidebar" => self.run_command(Command::ToggleSidebar),
            "jobs" => {
                self.show_sidebar = true;
                self.focus = Focus::Sidebar;
                Action::None
            }
            "logs" | "output" => {
                self.focus = Focus::Transcript;
                Action::None
            }
            "cancel" => Action::Cancel,
            "clear" => {
                self.session.items.clear();
                self.session.messages.clear();
                self.selected_card = None;
                self.transcript_scroll = 0;
                self.set_toast("transcript cleared", false);
                Action::None
            }
            "theme" | "themes" => self.run_command(Command::NextTheme),
            "help" => {
                self.push_notice(
                    "Shortcuts: Enter submit · Shift+Enter/Ctrl+J newline · !cmd direct shell · Tab toggle Agent/Shell · / commands · ctrl+o collapse/expand · ctrl+p palette · ctrl+x b sidebar · Esc focus transcript / cancel",
                    false,
                );
                Action::None
            }
            "quit" | "q" | "exit" => Action::Quit,
            other => {
                self.set_toast(format!("unknown command: /{other} (try /help)"), true);
                Action::None
            }
        }
    }


    /// Returns either matching slash commands OR `/cd` directory completions.
    pub(in crate::tui) fn slash_query(&self) -> Option<&str> {
        if !self.input.starts_with('/') || self.input.contains('\n') {
            return None;
        }
        let rest = &self.input[1..];
        if rest.contains(char::is_whitespace) {
            return None;
        }
        Some(rest)
    }


    pub(in crate::tui) fn filtered_slash(&self) -> Vec<&'static SlashCommand> {
        let Some(query) = self.slash_query() else {
            return Vec::new();
        };
        let q = query.to_ascii_lowercase();
        SLASH_COMMANDS
            .iter()
            .filter(|cmd| {
                q.is_empty()
                    || cmd.name.starts_with(&q)
                    || cmd.aliases.iter().any(|a| a.starts_with(&q))
            })
            .collect()
    }


    pub(in crate::tui) fn cd_query(&self) -> Option<&str> {
        if self.input.contains('\n') {
            return None;
        }
        self.input.strip_prefix("/cd ")
    }


    pub(in crate::tui) fn cd_completions(&self) -> Vec<(String, String)> {
        let Some(raw) = self.cd_query() else {
            return Vec::new();
        };
        complete_directories(&self.session.cwd, raw)
    }

}
