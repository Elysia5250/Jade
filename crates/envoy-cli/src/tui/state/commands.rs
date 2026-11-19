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
            Command::OpenRewind => {
                self.open_rewind_modal();
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
            Command::OpenModePicker => {
                self.open_mode_picker();
                Action::None
            }
            Command::OpenPermissionPicker => {
                self.open_permission_picker();
                Action::None
            }
            Command::OpenThemePicker => {
                self.open_theme_picker();
                Action::None
            }
            Command::OpenTrustPicker => {
                self.open_trust_picker();
                Action::None
            }
            Command::OpenSidebarPicker => {
                self.open_sidebar_picker();
                Action::None
            }
            Command::OpenMousePicker => {
                self.open_mouse_picker();
                Action::None
            }
            Command::OpenBudgetPicker => {
                self.open_budget_picker();
                Action::None
            }
            Command::SetMode(mode) => {
                self.mode = mode;
                self.set_toast(format!("mode: {}", mode.label()), false);
                Action::None
            }
            Command::SetPermission(mode) => {
                self.set_permission_mode(mode);
                Action::None
            }
            Command::SetTheme(idx) => {
                self.theme = idx % THEMES.len();
                self.save_ui_prefs();
                self.set_toast(format!("theme: {}", THEMES[self.theme].0), false);
                Action::None
            }
            Command::SetTrust(on) => {
                match set_workspace_trusted(&self.data_dir, &self.session.cwd, on) {
                    Ok(()) => self.set_toast(
                        format!("workspace {}", if on { "trusted" } else { "untrusted" }),
                        false,
                    ),
                    Err(e) => self.set_toast(format!("{e:#}"), true),
                }
                Action::None
            }
            Command::SetSidebar(shown) => {
                self.show_sidebar = shown;
                self.inspector.force_open = shown;
                self.save_ui_prefs();
                self.set_toast(
                    if shown {
                        "inspector: shown"
                    } else {
                        "inspector: hidden"
                    },
                    false,
                );
                Action::None
            }
            Command::ResetSidebar => {
                self.inspector.dragged_ratio = None;
                self.show_sidebar = true;
                self.inspector.force_open = true;
                self.set_toast("inspector width reset", false);
                Action::None
            }
            Command::SetMouse(on) => {
                self.mouse_capture = on;
                self.set_toast(
                    if on {
                        "mouse capture: on (drag to OSC52 copy, click links)"
                    } else {
                        "mouse capture: off (native terminal selection)"
                    },
                    false,
                );
                Action::SetMouseCapture(on)
            }
            Command::SetBudget(tier) => {
                self.run_budget_command(tier);
                Action::None
            }
            Command::ToggleExpand => {
                self.toggle_expand_cards();
                Action::None
            }
            Command::Cancel => Action::Cancel,
            Command::ToggleSidebar => {
                self.show_sidebar = !self.show_sidebar;
                self.inspector.force_open = self.show_sidebar;
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
            matches!(
                i,
                TranscriptItem::Tool {
                    collapsed: true,
                    ..
                }
            ) || matches!(
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
                                self.set_toast(format!("logged out of {id}"), false);
                            }
                            Ok(false) => {
                                self.set_toast(
                                    format!("{id} comes from env var; unset env to remove"),
                                    true,
                                );
                            }
                            Err(e) => self.set_toast(format!("{e:#}"), true),
                        }
                    } else {
                        self.set_toast("no provider logged in", true);
                    }
                } else {
                    match remove_provider(&self.data_dir, arg) {
                        Ok(true) => {
                            self.set_toast(format!("logged out of {arg}"), false);
                        }
                        Ok(false) => {
                            self.set_toast(format!("unknown stored provider `{arg}`"), true)
                        }
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
                    self.set_toast("no provider connected - run /login first", true);
                }
                Action::None
            }
            "compact" => {
                self.request_compact(arg, false);
                Action::None
            }
            "handoff" => {
                self.request_compact(arg, true);
                Action::None
            }
            "budget" => {
                self.run_budget_command(arg);
                Action::None
            }
            "delegate" => {
                self.run_delegate_command(arg);
                Action::None
            }
            "effort" => {
                self.run_effort_command(arg);
                Action::None
            }
            "mode" => {
                if arg.is_empty() {
                    self.open_mode_picker();
                } else {
                    let mode = match arg.to_ascii_lowercase().as_str() {
                        "agent" => Some(InputMode::Agent),
                        "shell" => Some(InputMode::Shell),
                        _ => None,
                    };
                    match mode {
                        Some(mode) => {
                            self.mode = mode;
                            self.set_toast(format!("mode: {}", mode.label()), false);
                        }
                        None => self.set_toast("usage: /mode [agent|shell]", true),
                    }
                }
                Action::None
            }
            "permissions" | "permission" | "perm" => {
                if arg.is_empty() {
                    self.open_permission_picker();
                } else {
                    self.run_permissions_command(arg);
                }
                Action::None
            }
            "plan" => {
                self.set_permission_mode(PermissionMode::Plan);
                Action::None
            }
            "trust" => {
                let target = match arg.trim().to_ascii_lowercase().as_str() {
                    "" => return self.run_command(Command::OpenTrustPicker),
                    "on" | "true" | "yes" | "1" => true,
                    "off" | "false" | "no" | "0" => false,
                    _ => {
                        self.set_toast("usage: /trust [on|off]", true);
                        return Action::None;
                    }
                };
                match set_workspace_trusted(&self.data_dir, &self.session.cwd, target) {
                    Ok(()) => self.set_toast(
                        format!("workspace {}", if target { "trusted" } else { "untrusted" }),
                        false,
                    ),
                    Err(e) => self.set_toast(format!("{e:#}"), true),
                }
                Action::None
            }
            "mcp" => {
                self.run_mcp_command(arg);
                Action::None
            }
            "skill" | "skills" => {
                self.run_skill_command(arg);
                Action::None
            }
            "worktree" => {
                self.run_worktree_command(arg);
                Action::None
            }
            "cd" => {
                if arg.is_empty() {
                    self.set_toast(format!("cwd: {}", self.cwd_label()), false);
                    return Action::None;
                }
                let candidate = resolve_cd_path(&self.session.cwd, arg);
                match std::fs::canonicalize(&candidate) {
                    Ok(path) if path.is_dir() => {
                        self.session_cwd_set(path);
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
            "sidebar" | "panel" => {
                if arg.trim() == "reset" {
                    self.run_command(Command::ResetSidebar)
                } else if arg.is_empty() {
                    self.open_sidebar_picker();
                    Action::None
                } else {
                    self.set_toast("usage: /sidebar [reset]", true);
                    Action::None
                }
            }
            "diff" => {
                let target = arg.trim();
                if target.is_empty() {
                    self.show_sidebar = true;
                    self.inspector.force_open = true;
                    self.inspector.tab = InspectorTab::Diff;
                    self.inspector.scroll = 0;
                    if self.inspector.file_path.is_none() {
                        if let Some(first) = self.session.changed_files.last() {
                            self.inspector.selected_diff = self.session.changed_files.len() - 1;
                            self.inspector.file_path = Some(first.path.clone());
                        }
                    }
                } else {
                    self.open_file_in_inspector(target, None);
                    self.inspector.tab = InspectorTab::Diff;
                }
                Action::None
            }
            "open" | "view" => {
                let target = arg.trim();
                if target.is_empty() {
                    self.set_toast("usage: /open <path[:line]>", true);
                } else {
                    self.open_file_in_inspector(target, None);
                    self.inspector.tab = InspectorTab::File;
                }
                Action::None
            }
            "mouse" => self.run_command(Command::OpenMousePicker),
            "jobs" => {
                self.show_sidebar = true;
                self.inspector.force_open = true;
                self.inspector.tab = InspectorTab::Overview;
                self.focus = Focus::Sidebar;
                Action::None
            }
            "logs" | "output" => {
                self.show_sidebar = true;
                self.inspector.force_open = true;
                self.inspector.tab = InspectorTab::Output;
                self.focus = Focus::Sidebar;
                Action::None
            }
            "cancel" => Action::Cancel,
            "monitor" | "monitors" | "mon" => {
                self.run_monitor_command(arg);
                Action::None
            }
            "rewind" | "undo" => {
                self.run_rewind_command(arg);
                Action::None
            }
            "clear" => {
                self.session.items.clear();
                self.session.messages.clear();
                self.session.changed_files.clear();
                self.expanded_explore_groups.clear();
                self.transcript_cache.clear();
                self.selected_card = None;
                self.transcript_scroll = 0;
                self.set_toast("transcript cleared", false);
                Action::None
            }
            "theme" | "themes" => self.run_command(Command::OpenThemePicker),
            "help" => {
                self.push_notice(
                    "Shortcuts: Click underlined paths to review in right Inspector - Drag divider to resize - Alt+1..5 switch Inspector tabs - Ctrl+B toggle panel - Ctrl+O expand/collapse tools & thinking - Shift+Tab cycle permissions - /monitor [list|stop <id>] - /rewind [id|last] - /mouse - /diff /open - /delegate [off|low|normal|high] - /effort [level] - /worktree [name|merge|remove]",
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
