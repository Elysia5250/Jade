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
            Command::CyclePermission => {
                self.set_permission_mode(self.perm_mode.cycle());
                Action::None
            }
            Command::ToggleExpand => {
                self.toggle_expand_cards();
                Action::None
            }
            Command::ToggleMouseCapture => Action::ToggleMouseCapture,
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
            "mode" => self.run_command(Command::ToggleMode),
            "permissions" | "perm" => {
                self.run_permissions_command(arg);
                Action::None
            }
            "plan" => {
                self.set_permission_mode(PermissionMode::Plan);
                Action::None
            }
            "trust" => {
                let target = match arg.trim().to_ascii_lowercase().as_str() {
                    "" => !is_workspace_trusted(&self.data_dir, &self.session.cwd),
                    "on" | "true" | "yes" | "1" => true,
                    "off" | "false" | "no" | "0" => false,
                    _ => {
                        self.set_toast("usage: /trust [on|off]", true);
                        return Action::None;
                    }
                };
                match set_workspace_trusted(&self.data_dir, &self.session.cwd, target) {
                    Ok(()) => self.set_toast(
                        format!(
                            "workspace {}",
                            if target { "trusted" } else { "untrusted" }
                        ),
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
                    self.inspector.dragged_ratio = None;
                    self.show_sidebar = true;
                    self.set_toast("inspector width reset", false);
                    Action::None
                } else {
                    self.run_command(Command::ToggleSidebar)
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
            "mouse" => Action::ToggleMouseCapture,
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
            "theme" | "themes" => self.run_command(Command::NextTheme),
            "help" => {
                self.push_notice(
                    "Shortcuts: Click underlined paths to review in right Inspector · Drag divider to resize · Alt+1..5 switch Inspector tabs · Ctrl+B toggle panel · Ctrl+O expand/collapse tools & thinking · Shift+Tab cycle permissions · /mouse toggle mouse capture · /diff /open",
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

    fn run_permissions_command(&mut self, arg: &str) {
        let trimmed = arg.trim();
        if trimmed.is_empty() || trimmed == "status" {
            let eng = PermissionEngine::new(
                self.data_dir.clone(),
                self.session.cwd.clone(),
                self.session.id.clone(),
                self.target.clone(),
                self.perm_mode,
            );
            let loaded = eng.loaded_policies();
            let trusted = eng.is_trusted();
            let sandbox = if self.target == "local" || self.target.is_empty() {
                envoy_core::sandbox::backend_label()
            } else {
                "remote"
            };
            self.push_notice(
                format!(
                    "Permissions: mode={} · sandbox={} · workspace_trusted={} · rules={} · extra_rw={}",
                    self.perm_mode.label(),
                    sandbox,
                    trusted,
                    loaded.rules.len(),
                    loaded.extra_rw.len(),
                ),
                false,
            );
            return;
        }
        if let Some(mode) = PermissionMode::parse(trimmed) {
            self.set_permission_mode(mode);
            return;
        }
        if let Some(pat) = trimmed.strip_prefix("allow ").map(str::trim).filter(|s| !s.is_empty()) {
            let scope = if is_workspace_trusted(&self.data_dir, &self.session.cwd) {
                RuleScope::Project
            } else {
                RuleScope::User
            };
            match crate::permissions::rules::save_rule(
                &self.data_dir,
                &self.session.cwd,
                scope,
                crate::permissions::Effect::Allow,
                pat,
            ) {
                Ok(()) => self.set_toast(format!("allowed: {pat}"), false),
                Err(e) => self.set_toast(format!("{e:#}"), true),
            }
            return;
        }
        if let Some(pat) = trimmed.strip_prefix("deny ").map(str::trim).filter(|s| !s.is_empty()) {
            let scope = if is_workspace_trusted(&self.data_dir, &self.session.cwd) {
                RuleScope::Project
            } else {
                RuleScope::User
            };
            match crate::permissions::rules::save_rule(
                &self.data_dir,
                &self.session.cwd,
                scope,
                crate::permissions::Effect::Deny,
                pat,
            ) {
                Ok(()) => self.set_toast(format!("denied: {pat}"), false),
                Err(e) => self.set_toast(format!("{e:#}"), true),
            }
            return;
        }
        self.set_toast(
            "usage: /permissions [default|accept-edits|plan|auto|allow <rule>|deny <rule>]",
            true,
        );
    }

    fn run_mcp_command(&mut self, arg: &str) {
        let report = crate::mcp::config::load_mcp_configs(&self.data_dir, &self.session.cwd);
        if arg.trim() == "probe" {
            let data_dir = self.data_dir.clone();
            let cwd = self.session.cwd.clone();
            let notice = self.notice_tx.clone();
            self.set_toast("probing MCP servers…", false);
            tokio::spawn(async move {
                let mgr = crate::mcp::client::McpManager::connect_configured(&data_dir, &cwd).await;
                let mut lines = Vec::new();
                for p in mgr.skipped_untrusted() {
                    lines.push(format!("skipped untrusted project MCP config: {}", p.display()));
                }
                for s in mgr.statuses() {
                    if let Some(err) = &s.error {
                        lines.push(format!("{} [{}] error: {}", s.name, s.transport_label, err));
                    } else {
                        lines.push(format!("{} [{}] connected ({} tools)", s.name, s.transport_label, s.tool_count));
                    }
                }
                if lines.is_empty() {
                    lines.push("no MCP servers configured".into());
                }
                let _ = notice.send(UiNotice::Notice(lines.join("\n"), false));
            });
            return;
        }
        let mut lines = Vec::new();
        for p in &report.skipped_untrusted {
            lines.push(format!("skipped untrusted: {} (run `/trust on` to enable)", p.display()));
        }
        for s in &report.servers {
            let kind = match s.transport {
                crate::mcp::config::McpTransportConfig::Stdio { .. } => "stdio",
                crate::mcp::config::McpTransportConfig::Http { .. } => "http",
            };
            lines.push(format!(
                "{} [{} · {}]{}",
                s.name,
                s.scope.label(),
                kind,
                if s.disabled { " (disabled)" } else { "" }
            ));
        }
        if lines.is_empty() {
            lines.push("MCP: no servers configured (~/.envoy/mcp.json or .mcp.json)".into());
        } else {
            lines.push("Tip: run `/mcp probe` to connect and count tools.".into());
        }
        self.push_notice(lines.join("\n"), false);
    }

    fn run_skill_command(&mut self, arg: &str) {
        let name = arg.trim();
        if name.is_empty() {
            let skills = crate::skills::discover_skills(&self.data_dir, &self.session.cwd);
            if skills.is_empty() {
                self.push_notice(
                    "Skills: none found (add ~/.envoy/skills/<name>/SKILL.md or trust project .envoy/skills)",
                    false,
                );
                return;
            }
            let lines: Vec<String> = skills
                .iter()
                .map(|s| format!("{} [{}] — {}", s.name, s.scope.label(), s.description))
                .collect();
            self.push_notice(lines.join("\n"), false);
            return;
        }
        match crate::skills::load_skill(&self.data_dir, &self.session.cwd, name) {
            Ok(loaded) => {
                let preview: String = loaded.body.chars().take(600).collect();
                self.push_notice(
                    format!(
                        "Skill `{}` ({})\n{}",
                        loaded.summary.name,
                        loaded.summary.skill_file.display(),
                        preview
                    ),
                    false,
                );
            }
            Err(e) => self.set_toast(format!("{e:#}"), true),
        }
    }

    fn run_budget_command(&mut self, arg: &str) {
        let Some((prov, model)) = resolve_active_provider(&self.data_dir) else {
            self.set_toast("no provider connected — run /login first", true);
            return;
        };
        if arg.is_empty() {
            let plan =
                crate::agent::context::resolve_budget(&self.data_dir, &prov.id, &model.model_id);
            let (used, est) = context_used(&self.session);
            let mark = if est { "~" } else { "" };
            self.push_notice(
                format!(
                    "Context budget for {}: tier={} ({}) · compact_at={} · prune_at={} · reserve={} · hard_window={} ({}) · used={mark}{}",
                    model.display(),
                    plan.tier,
                    plan.harness_profile,
                    fmt_tokens(plan.compact_at),
                    fmt_tokens(plan.prune_at),
                    fmt_tokens(plan.reserve_output),
                    fmt_tokens(plan.hard_window),
                    plan.source,
                    fmt_tokens(used),
                ),
                false,
            );
            return;
        }
        let Some(tier) = crate::agent::CompactionTier::parse(arg) else {
            self.set_toast(
                "usage: /budget [native|focused|balanced|max|400k]",
                true,
            );
            return;
        };
        if let Err(e) =
            crate::agent::context::set_budget_tier(&self.data_dir, tier, Some(&model))
        {
            self.set_toast(format!("{e:#}"), true);
            return;
        }
        let plan =
            crate::agent::context::resolve_budget(&self.data_dir, &prov.id, &model.model_id);
        self.set_toast(
            format!(
                "budget: {} -> compact at {} (hard {})",
                plan.tier,
                fmt_tokens(plan.compact_at),
                fmt_tokens(plan.hard_window)
            ),
            false,
        );
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
