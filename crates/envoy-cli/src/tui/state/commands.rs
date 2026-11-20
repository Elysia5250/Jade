//! Slash commands, command palette, and completion queries.
use super::*;

impl App {
    pub(in crate::tui) fn run_command(&mut self, command: Command) -> Action {
        match command {
            Command::FocusInput => {
                self.ui.focus = Focus::Input;
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
                self.session.current =
                    Session::new(self.session.current.cwd.clone(), self.jobs.target.clone());
                self.reset_session_view();
                self.spawn_agent_attach();
                self.set_toast("started new session", false);
                Action::None
            }
            Command::ToggleMode => {
                self.ui.mode = match self.ui.mode {
                    InputMode::Agent => InputMode::Shell,
                    InputMode::Shell => InputMode::Agent,
                };
                self.set_toast(format!("mode: {}", self.ui.mode.label()), false);
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
                self.ui.mode = mode;
                self.set_toast(format!("mode: {}", mode.label()), false);
                Action::None
            }
            Command::SetPermission(mode) => {
                self.set_permission_mode(mode);
                Action::None
            }
            Command::SetTheme(idx) => {
                self.ui.theme = idx % THEMES.len();
                self.save_ui_prefs();
                self.set_toast(format!("theme: {}", THEMES[self.ui.theme].0), false);
                Action::None
            }
            Command::SetTrust(on) => {
                match set_workspace_trusted(&self.data_dir, &self.session.current.cwd, on) {
                    Ok(()) => {
                        self.inputs.invalidate_catalog();
                        self.set_toast(
                            format!("workspace {}", if on { "trusted" } else { "untrusted" }),
                            false,
                        );
                    }
                    Err(e) => self.set_toast(format!("{e:#}"), true),
                }
                Action::None
            }
            Command::SetSidebar(shown) => {
                self.ui.show_sidebar = shown;
                self.ui.inspector.force_open = shown;
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
                self.ui.inspector.dragged_ratio = None;
                self.ui.show_sidebar = true;
                self.ui.inspector.force_open = true;
                self.set_toast("inspector width reset", false);
                Action::None
            }
            Command::SetMouse(on) => {
                self.ui.mouse_capture = on;
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
                self.ui.show_sidebar = !self.ui.show_sidebar;
                self.ui.inspector.force_open = self.ui.show_sidebar;
                self.save_ui_prefs();
                Action::None
            }
            Command::NextTheme => {
                self.ui.theme = (self.ui.theme + 1) % THEMES.len();
                self.save_ui_prefs();
                self.set_toast(format!("theme: {}", THEMES[self.ui.theme].0), false);
                Action::None
            }
            Command::Refresh => Action::Refresh,
            Command::Switch(target) => Action::Switch(target),
            Command::Quit => Action::Quit,
        }
    }

    pub(in crate::tui) fn toggle_expand_cards(&mut self) {
        if let Some(idx) = self.ui.selected_card {
            if let Some(item) = self.session.current.items.get_mut(idx) {
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
        let any_collapsed = self.session.current.items.iter().any(|i| {
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
        for item in &mut self.session.current.items {
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
        let Some(kind) = lookup_slash(name) else {
            self.set_toast(format!("unknown command: /{name} (try /help)"), true);
            return Action::None;
        };
        match kind {
            SlashCmdKind::Login => {
                self.open_login_modal();
                Action::None
            }
            SlashCmdKind::Logout => {
                if arg.is_empty() {
                    if let Some((prov, _)) = resolve_active_provider(&self.data_dir) {
                        let id = prov.id.clone();
                        match remove_provider(&self.data_dir, &id) {
                            Ok(true) => {
                                self.inputs.invalidate_catalog();
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
                            self.inputs.invalidate_catalog();
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
            SlashCmdKind::Models => {
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
                        self.inputs.invalidate_catalog();
                        self.set_toast(format!("model: {pid}/{mid}"), false);
                    }
                } else {
                    self.set_toast("no provider connected - run /login first", true);
                }
                Action::None
            }
            SlashCmdKind::Compact => {
                self.request_compact(arg, false);
                Action::None
            }
            SlashCmdKind::Handoff => {
                self.request_compact(arg, true);
                Action::None
            }
            SlashCmdKind::Budget => {
                self.run_budget_command(arg);
                Action::None
            }
            SlashCmdKind::Delegate => {
                self.run_delegate_command(arg);
                Action::None
            }
            SlashCmdKind::Effort => {
                self.run_effort_command(arg);
                Action::None
            }
            SlashCmdKind::Mode => {
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
                            self.ui.mode = mode;
                            self.set_toast(format!("mode: {}", mode.label()), false);
                        }
                        None => self.set_toast("usage: /mode [agent|shell]", true),
                    }
                }
                Action::None
            }
            SlashCmdKind::Permissions => {
                if arg.is_empty() {
                    self.open_permission_picker();
                } else {
                    self.run_permissions_command(arg);
                }
                Action::None
            }
            SlashCmdKind::Plan => {
                self.set_permission_mode(PermissionMode::Plan);
                Action::None
            }
            SlashCmdKind::Trust => {
                let target = match arg.trim().to_ascii_lowercase().as_str() {
                    "" => return self.run_command(Command::OpenTrustPicker),
                    "on" | "true" | "yes" | "1" => true,
                    "off" | "false" | "no" | "0" => false,
                    _ => {
                        self.set_toast("usage: /trust [on|off]", true);
                        return Action::None;
                    }
                };
                match set_workspace_trusted(&self.data_dir, &self.session.current.cwd, target) {
                    Ok(()) => self.set_toast(
                        format!("workspace {}", if target { "trusted" } else { "untrusted" }),
                        false,
                    ),
                    Err(e) => self.set_toast(format!("{e:#}"), true),
                }
                Action::None
            }
            SlashCmdKind::Mcp => {
                self.run_mcp_command(arg);
                Action::None
            }
            SlashCmdKind::Skill => {
                self.run_skill_command(arg);
                Action::None
            }
            SlashCmdKind::Worktree => {
                self.run_worktree_command(arg);
                Action::None
            }
            SlashCmdKind::Cd => {
                if arg.is_empty() {
                    self.set_toast(format!("cwd: {}", self.cwd_label()), false);
                    return Action::None;
                }
                let candidate = resolve_cd_path(&self.session.current.cwd, arg);
                match std::fs::canonicalize(&candidate) {
                    Ok(path) if path.is_dir() => {
                        let id = self.session.current.id.clone();
                        let cwd = path.clone();
                        self.session_cwd_set(path);
                        self.persist_session();
                        self.sync_agent(AgentReq::SetCwd { id, cwd });
                        self.set_toast(format!("cwd: {}", self.cwd_label()), false);
                    }
                    Ok(_) => self.set_toast(format!("not a directory: {arg}"), true),
                    Err(e) => self.set_toast(format!("{arg}: {e}"), true),
                }
                Action::None
            }
            SlashCmdKind::Machine => {
                if arg.is_empty() {
                    let items = self.machine_items();
                    self.open_picker("machines", items);
                    Action::None
                } else {
                    Action::Switch(arg.to_string())
                }
            }
            SlashCmdKind::New => self.run_command(Command::NewSession),
            SlashCmdKind::Sessions => {
                self.open_sessions_modal();
                Action::None
            }
            SlashCmdKind::Expand => {
                self.toggle_expand_cards();
                Action::None
            }
            SlashCmdKind::Sidebar => {
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
            SlashCmdKind::Diff => {
                let target = arg.trim();
                if target.is_empty() {
                    self.ui.show_sidebar = true;
                    self.ui.inspector.force_open = true;
                    self.ui.inspector.tab = InspectorTab::Diff;
                    self.ui.inspector.scroll = 0;
                    if self.ui.inspector.file_path.is_none() {
                        if let Some(first) = self.session.current.changed_files.last() {
                            self.ui.inspector.selected_diff =
                                self.session.current.changed_files.len() - 1;
                            self.ui.inspector.file_path = Some(first.path.clone());
                        }
                    }
                } else {
                    self.open_file_in_inspector(target, None);
                    self.ui.inspector.tab = InspectorTab::Diff;
                }
                Action::None
            }
            SlashCmdKind::Open => {
                let target = arg.trim();
                if target.is_empty() {
                    self.set_toast("usage: /open <path[:line]>", true);
                } else {
                    self.open_file_in_inspector(target, None);
                    self.ui.inspector.tab = InspectorTab::File;
                }
                Action::None
            }
            SlashCmdKind::Mouse => self.run_command(Command::OpenMousePicker),
            SlashCmdKind::Jobs => {
                self.ui.show_sidebar = true;
                self.ui.inspector.force_open = true;
                self.ui.inspector.tab = InspectorTab::Overview;
                self.ui.focus = Focus::Sidebar;
                Action::None
            }
            SlashCmdKind::Logs => {
                self.ui.show_sidebar = true;
                self.ui.inspector.force_open = true;
                self.ui.inspector.tab = InspectorTab::Output;
                self.ui.focus = Focus::Sidebar;
                Action::None
            }
            SlashCmdKind::Cancel => Action::Cancel,
            SlashCmdKind::Monitor => {
                self.run_monitor_command(arg);
                Action::None
            }
            SlashCmdKind::Rewind => {
                self.run_rewind_command(arg);
                Action::None
            }
            SlashCmdKind::Clear => {
                let id = self.session.current.id.clone();
                self.session.current.items.clear();
                self.session.current.messages.clear();
                self.session.current.changed_files.clear();
                self.session.expanded_explore_groups.clear();
                self.session.transcript_cache.clear();
                self.ui.selected_card = None;
                self.ui.transcript_scroll = 0;
                self.ui.transcript_max_scroll = 0;
                self.ui.hit_regions.clear();
                self.ui.selection = None;
                self.ui.drag_start = None;
                self.ui.screen_lines.clear();
                self.ui.inspector = InspectorState::default();
                self.sync_agent(AgentReq::Clear { id });
                self.set_toast("transcript cleared", false);
                Action::None
            }
            SlashCmdKind::Theme => self.run_command(Command::OpenThemePicker),
            SlashCmdKind::Help => {
                self.push_notice(
                    "Shortcuts: Click underlined paths to review in right Inspector - Drag divider to resize - Alt+1..5 switch Inspector tabs - Ctrl+B toggle panel - Ctrl+O expand/collapse tools & thinking - Shift+Tab cycle permissions - /monitor [list|stop <id>] - /rewind [id|last] - /mouse - /diff /open - /delegate [off|low|normal|high] - /effort [level] - /worktree [name|merge|remove]",
                    false,
                );
                Action::None
            }
            SlashCmdKind::Quit => Action::Quit,
        }
    }

    /// Returns either matching slash commands OR `/cd` directory completions.
    pub(in crate::tui) fn slash_query(&self) -> Option<&str> {
        if !self.ui.input.starts_with('/') || self.ui.input.contains('\n') {
            return None;
        }
        let rest = &self.ui.input[1..];
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
        if self.ui.input.contains('\n') {
            return None;
        }
        self.ui.input.strip_prefix("/cd ")
    }

    pub(in crate::tui) fn cd_completions(&self) -> Vec<(String, String)> {
        let Some(raw) = self.cd_query() else {
            return Vec::new();
        };
        complete_directories(&self.session.current.cwd, raw)
    }
}
