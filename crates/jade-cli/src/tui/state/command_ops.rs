//! Argument handlers for /permissions, /mcp, /skill, /budget, /delegate, and /worktree.
use super::*;

impl App {
    pub(in crate::tui) fn run_permissions_command(&mut self, arg: &str) {
        let trimmed = arg.trim();
        if trimmed.is_empty() || trimmed == "status" {
            if self.agent.offline
                || self.agent.stream.is_none()
                || !self.agent.ready
                || tokio::runtime::Handle::try_current().is_err()
            {
                self.set_toast("daemon offline - cannot query permissions status", true);
                return;
            }
            let data_dir = self.data_dir.clone();
            let sid = self.session.current.id.clone();
            let notice = self.notice_tx.clone();
            self.set_toast("querying permissions status...", false);
            tokio::spawn(async move {
                let (msg, err) =
                    match runtime::request(&data_dir, &AgentReq::GetPermissions { id: sid.clone() })
                        .await
                    {
                        Ok(AgentResp::PermissionsStatus {
                            mode,
                            sandbox,
                            workspace_trusted,
                            rules,
                            extra_rw,
                        }) => (
                            format!(
                                "Permissions (daemon): mode={} - sandbox={} - workspace_trusted={} - rules={} - extra_rw={}",
                                mode.label(),
                                sandbox,
                                workspace_trusted,
                                rules,
                                extra_rw,
                            ),
                            false,
                        ),
                        Ok(AgentResp::Error(reason)) => (reason, true),
                        Ok(_) => ("unexpected daemon reply to permissions query".into(), true),
                        Err(err) => (format!("permissions status failed: {err:#}"), true),
                    };
                let _ = notice.send(UiNotice::Notice {
                    session_id: sid,
                    msg,
                    err,
                });
            });
            return;
        }
        if let Some(mode) = PermissionMode::parse(trimmed) {
            self.set_permission_mode(mode);
            return;
        }
        if let Some(pat) = trimmed
            .strip_prefix("allow ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let scope = if is_workspace_trusted(&self.data_dir, &self.session.current.cwd) {
                RuleScope::Project
            } else {
                RuleScope::User
            };
            match jade_agent::permissions::rules::save_rule(
                &self.data_dir,
                &self.session.current.cwd,
                scope,
                jade_agent::permissions::Effect::Allow,
                pat,
            ) {
                Ok(()) => self.set_toast(format!("allowed: {pat}"), false),
                Err(e) => self.set_toast(format!("{e:#}"), true),
            }
            return;
        }
        if let Some(pat) = trimmed
            .strip_prefix("deny ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let scope = if is_workspace_trusted(&self.data_dir, &self.session.current.cwd) {
                RuleScope::Project
            } else {
                RuleScope::User
            };
            match jade_agent::permissions::rules::save_rule(
                &self.data_dir,
                &self.session.current.cwd,
                scope,
                jade_agent::permissions::Effect::Deny,
                pat,
            ) {
                Ok(()) => self.set_toast(format!("denied: {pat}"), false),
                Err(e) => self.set_toast(format!("{e:#}"), true),
            }
            return;
        }
        self.set_toast(
            "usage: /permissions [default|accept-edits|plan|auto|turbo|allow <rule>|deny <rule>]",
            true,
        );
    }

    pub(in crate::tui) fn run_ps_command(&mut self, arg: &str) {
        let trimmed = arg.trim();
        let data_dir = self.data_dir.clone();
        let notice = self.notice_tx.clone();
        let sid = self.session.current.id.clone();

        if trimmed == "sleep" {
            self.set_toast("entering L2 cool sleep.", false);
            tokio::spawn(async move {
                let (msg, err) = match runtime::request(&data_dir, &AgentReq::Sleep).await {
                    Ok(AgentResp::Slept {
                        sessions,
                        mcp_servers,
                    }) => (
                        format!(
                            "daemon entered L2 cool sleep: dehydrated {sessions} session(s), stopped {mcp_servers} MCP server(s)"
                        ),
                        false,
                    ),
                    Ok(AgentResp::Error(reason)) => (reason, true),
                    Ok(_) => ("unexpected daemon reply to sleep".into(), true),
                    Err(err) => (format!("daemon sleep failed: {err:#}"), true),
                };
                let _ = notice.send(UiNotice::Notice {
                    session_id: sid,
                    msg,
                    err,
                });
            });
            return;
        }

        if let Some(rest) = trimmed.strip_prefix("stop-mcp") {
            let target_arg = rest.trim();
            if target_arg.is_empty() {
                self.set_toast("usage: /ps stop-mcp <name|all>", true);
                return;
            }
            let name = if target_arg == "all" {
                None
            } else {
                Some(target_arg.to_string())
            };
            self.set_toast("stopping MCP server(s).", false);
            tokio::spawn(async move {
                let (msg, err) =
                    match runtime::request(&data_dir, &AgentReq::StopMcp { name }).await {
                        Ok(AgentResp::Stopped { count }) => {
                            (format!("stopped {count} MCP server process(es)"), false)
                        }
                        Ok(AgentResp::Error(reason)) => (reason, true),
                        Ok(_) => ("unexpected daemon reply to stop-mcp".into(), true),
                        Err(err) => (format!("stop-mcp failed: {err:#}"), true),
                    };
                let _ = notice.send(UiNotice::Notice {
                    session_id: sid,
                    msg,
                    err,
                });
            });
            return;
        }

        if !trimmed.is_empty() {
            self.set_toast("usage: /ps [sleep|stop-mcp <name|all>]", true);
            return;
        }

        self.set_toast("loading daemon process list.", false);
        tokio::spawn(async move {
            let (msg, err) = match runtime::request(&data_dir, &AgentReq::ProcessList).await {
                Ok(AgentResp::ProcessList(list)) => {
                    (crate::ps_cli::format_process_list(&list).trim_end().to_string(), false)
                }
                Ok(AgentResp::Error(reason)) => (reason, true),
                Ok(_) => ("unexpected daemon reply to process list".into(), true),
                Err(err) => (format!("ps failed: {err:#}"), true),
            };
            let _ = notice.send(UiNotice::Notice {
                session_id: sid,
                msg,
                err,
            });
        });
    }

    pub(in crate::tui) fn run_mcp_command(&mut self, arg: &str) {
        let report =
            jade_agent::mcp::config::load_mcp_configs(&self.data_dir, &self.session.current.cwd);
        if arg.trim() == "probe" {
            let data_dir = self.data_dir.clone();
            let cwd = self.session.current.cwd.clone();
            let notice = self.notice_tx.clone();
            let sid = self.session.current.id.clone();
            self.set_toast("probing MCP servers.", false);
            tokio::spawn(async move {
                let (msg, err) =
                    match runtime::request(&data_dir, &AgentReq::ProbeMcp { cwd }).await {
                        Ok(AgentResp::McpProbe(lines)) => (lines.join("\n"), false),
                        Ok(AgentResp::Error(reason)) => (reason, true),
                        Ok(_) => ("unexpected daemon reply to MCP probe".into(), true),
                        Err(err) => (format!("mcp probe failed: {err:#}"), true),
                    };
                let _ = notice.send(UiNotice::Notice {
                    session_id: sid,
                    msg,
                    err,
                });
            });
            return;
        }
        let mut lines = Vec::new();
        for p in &report.skipped_untrusted {
            lines.push(format!(
                "skipped untrusted: {} (run `/trust on` to enable)",
                p.display()
            ));
        }
        for s in &report.servers {
            let kind = match s.transport {
                jade_agent::mcp::config::McpTransportConfig::Stdio { .. } => "stdio",
                jade_agent::mcp::config::McpTransportConfig::Http { .. } => "http",
            };
            lines.push(format!(
                "{} [{} - {}]{}",
                s.name,
                s.scope.label(),
                kind,
                if s.disabled { " (disabled)" } else { "" }
            ));
        }
        if lines.is_empty() {
            lines.push("MCP: no servers configured (~/.jade/mcp.json or .mcp.json)".into());
        } else {
            lines.push("Tip: run `/mcp probe` to connect and count tools.".into());
        }
        self.push_notice(lines.join("\n"), false);
    }

    pub(in crate::tui) fn run_plugin_command(&mut self, arg: &str) {
        let trimmed = arg.trim();
        let (sub, rest) = match trimmed.split_once(char::is_whitespace) {
            Some((s, r)) => (s, r.trim()),
            None => (trimmed, ""),
        };
        match sub {
            "" | "list" => {
                let report = jade_agent::plugins::discover_plugins(
                    &self.data_dir,
                    &self.session.current.cwd,
                );
                let mut lines = Vec::new();
                for p in &report.skipped_untrusted {
                    lines.push(format!(
                        "skipped untrusted plugin: {} (run `/trust on` to enable)",
                        p.display()
                    ));
                }
                for p in &report.plugins {
                    lines.push(format!(
                        "{} v{} [{} · {}] {}",
                        p.id,
                        p.version,
                        p.scope.label(),
                        if p.enabled { "enabled" } else { "disabled" },
                        p.description
                    ));
                }
                if lines.is_empty() {
                    lines.push("Plugins: none installed (~/.jade/plugins or .jade/plugins)".into());
                }
                self.push_notice(lines.join("\n"), false);
            }
            "enable" if !rest.is_empty() => {
                match jade_agent::plugins::set_plugin_enabled(&self.data_dir, rest, true) {
                    Ok(()) => {
                        self.inputs.invalidate_catalog();
                        self.set_toast(format!("plugin enabled: {rest}"), false);
                    }
                    Err(e) => self.set_toast(format!("{e:#}"), true),
                }
            }
            "disable" if !rest.is_empty() => {
                let report = jade_agent::plugins::discover_plugins(
                    &self.data_dir,
                    &self.session.current.cwd,
                );
                if report.plugins.iter().any(|p| {
                    p.id.eq_ignore_ascii_case(rest)
                        && p.scope == jade_agent::plugins::PluginScope::Managed
                }) {
                    self.set_toast(format!("cannot disable managed plugin `{rest}`"), true);
                    return;
                }
                match jade_agent::plugins::set_plugin_enabled(&self.data_dir, rest, false) {
                    Ok(()) => {
                        self.inputs.invalidate_catalog();
                        self.set_toast(format!("plugin disabled: {rest}"), false);
                    }
                    Err(e) => self.set_toast(format!("{e:#}"), true),
                }
            }
            "inspect" if !rest.is_empty() => {
                let report = jade_agent::plugins::discover_plugins(
                    &self.data_dir,
                    &self.session.current.cwd,
                );
                let Some(p) = report
                    .plugins
                    .into_iter()
                    .find(|p| p.id.eq_ignore_ascii_case(rest))
                else {
                    self.set_toast(format!("unknown plugin: {rest}"), true);
                    return;
                };
                let mut lines = vec![format!(
                    "{} ({}) v{} [{}]",
                    p.name,
                    p.id,
                    p.version,
                    p.scope.label()
                )];
                if p.skills_dir.is_some() {
                    lines.push("  component: skills".into());
                }
                if p.agents_dir.is_some() {
                    lines.push("  component: agents".into());
                }
                if p.commands_dir.is_some() {
                    lines.push("  component: commands".into());
                }
                if p.hooks_file.is_some() {
                    lines.push("  component: hooks".into());
                }
                if p.mcp_file.is_some() {
                    lines.push("  component: mcp".into());
                }
                if p.permissions_file.is_some() {
                    lines.push("  component: permissions (deny/ask active)".into());
                }
                if !p.suggested_allows.is_empty() {
                    lines.push(format!(
                        "  suggested allows (not auto-applied): {}",
                        p.suggested_allows.join(", ")
                    ));
                }
                self.push_notice(lines.join("\n"), false);
            }
            _ => self.set_toast("usage: /plugin [list|inspect <id>|enable <id>|disable <id>]", true),
        }
    }

    pub(in crate::tui) fn run_skill_command(&mut self, arg: &str) {
        let name = arg.trim();
        if name.is_empty() {
            let skills =
                jade_agent::skills::discover_skills(&self.data_dir, &self.session.current.cwd);
            if skills.is_empty() {
                self.push_notice(
                    "Skills: none found (add ~/.jade/skills/<name>/SKILL.md or trust project .jade/skills)",
                    false,
                );
                return;
            }
            let lines: Vec<String> = skills
                .iter()
                .map(|s| format!("{} [{}] - {}", s.name, s.scope.label(), s.description))
                .collect();
            self.push_notice(lines.join("\n"), false);
            return;
        }
        match jade_agent::skills::load_skill(&self.data_dir, &self.session.current.cwd, name) {
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

    pub(in crate::tui) fn run_budget_command(&mut self, arg: &str) {
        let Some((prov, model)) = resolve_active_provider(&self.data_dir) else {
            self.set_toast("no provider connected - run /login first", true);
            return;
        };
        if arg.is_empty() {
            self.open_budget_picker();
            return;
        }
        let Some(tier) = jade_agent::agent::CompactionTier::parse(arg) else {
            self.set_toast("usage: /budget [native|focused|balanced|max|400k]", true);
            return;
        };
        if let Err(e) =
            jade_agent::agent::context::set_budget_tier(&self.data_dir, tier, Some(&model))
        {
            self.set_toast(format!("{e:#}"), true);
            return;
        }
        let plan =
            jade_agent::agent::context::resolve_budget(&self.data_dir, &prov.id, &model.model_id);
        self.inputs.invalidate_catalog();
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

    pub(in crate::tui) fn run_delegate_command(&mut self, arg: &str) {
        use jade_agent::agent::delegate_pref::{self, DelegateLevel};
        if arg.is_empty() {
            let level = delegate_pref::load(&self.data_dir);
            self.set_toast(format!("delegate: {level} (off, low, normal, high)"), false);
            return;
        }
        let Some(level) = DelegateLevel::parse(arg) else {
            self.set_toast("usage: /delegate [off|low|normal|high]", true);
            return;
        };
        if let Err(err) = delegate_pref::set(&self.data_dir, level) {
            self.set_toast(err, true);
            return;
        }
        self.set_toast(
            format!("delegate: {level} (applies on the next turn)"),
            false,
        );
    }

    pub(in crate::tui) fn run_voice_command(&mut self, arg: &str) {
        use jade_agent::agent::voice_pref::{self, VoiceStyle};
        if arg.is_empty() {
            let style = voice_pref::load(&self.data_dir);
            self.set_toast(format!("voice: {style} (expert, engineer, plain)"), false);
            return;
        }
        let Some(style) = VoiceStyle::parse(arg) else {
            self.set_toast("usage: /voice [expert|engineer|plain]", true);
            return;
        };
        if let Err(err) = voice_pref::set(&self.data_dir, style) {
            self.set_toast(err, true);
            return;
        }
        self.inputs.invalidate_catalog();
        self.set_toast(
            format!("voice: {style} (applies on the next turn)"),
            false,
        );
    }

    pub(in crate::tui) fn run_effort_command(&mut self, arg: &str) {
        use jade_agent::agent::effort;
        let level = effort::load(&self.data_dir);
        let spec = resolve_active_provider(&self.data_dir).and_then(|(prov, model)| {
            effort::advertised(&self.data_dir, &prov.id, &model.model_id).map(|spec| (prov, spec))
        });
        if arg.is_empty() {
            let detail = match &spec {
                Some((_, spec)) => effort::describe(Some(spec)),
                None => effort::describe(None),
            };
            self.set_toast(format!("effort: {level} ({detail})"), false);
            return;
        }
        let Some(level) = effort::normalize(arg) else {
            self.set_toast("usage: /effort [level]", true);
            return;
        };
        if let Some((prov, spec)) = &spec {
            if !effort::accepts(spec, prov.protocol, &level) {
                self.set_toast(effort::describe(Some(spec)), true);
                return;
            }
        }
        if let Err(err) = effort::set(&self.data_dir, &level) {
            self.set_toast(err, true);
            return;
        }
        self.set_toast(
            format!("effort: {level} (applies on the next request)"),
            false,
        );
    }

    pub(in crate::tui) fn run_worktree_command(&mut self, arg: &str) {
        if !self.jobs.target.is_empty() && self.jobs.target != "local" {
            self.set_toast("worktree is local only", true);
            return;
        }
        if self.agent.running {
            self.set_toast("wait for the active turn to finish", true);
            return;
        }
        match jade_agent::worktree::execute(&self.session.current.cwd, arg) {
            Ok(jade_agent::worktree::Effect::Listed(text)) => self.push_notice(text, false),
            Ok(jade_agent::worktree::Effect::Toast(text)) => self.set_toast(text, false),
            Ok(jade_agent::worktree::Effect::Enter {
                path,
                resume,
                notice,
                error,
            }) => {
                self.enter_linked_tree(path, resume);
                self.set_toast(notice, error);
            }
            Err(err) => self.set_toast(err, true),
        }
    }

    fn enter_linked_tree(&mut self, path: PathBuf, resume: bool) {
        let loaded = if resume {
            exact_session_for_cwd(&self.data_dir, &path)
        } else {
            None
        };
        if let Some(loaded) = loaded {
            self.load_existing_session(loaded);
            self.spawn_agent_attach();
            return;
        }
        self.persist_session();
        let target = self.session.current.target.clone();
        self.session.current = Session::new(path, target);
        self.reset_session_view();
        self.spawn_agent_attach();
    }
}
