//! Argument handlers for /permissions, /mcp, /skill, /budget, /delegate, and /worktree.
use super::*;

impl App {
    pub(in crate::tui) fn run_permissions_command(&mut self, arg: &str) {
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
                "peer"
            };
            self.push_notice(
                format!(
                    "Permissions: mode={} - sandbox={} - workspace_trusted={} - rules={} - extra_rw={}",
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
        if let Some(pat) = trimmed
            .strip_prefix("allow ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let scope = if is_workspace_trusted(&self.data_dir, &self.session.cwd) {
                RuleScope::Project
            } else {
                RuleScope::User
            };
            match envoy_agent::permissions::rules::save_rule(
                &self.data_dir,
                &self.session.cwd,
                scope,
                envoy_agent::permissions::Effect::Allow,
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
            let scope = if is_workspace_trusted(&self.data_dir, &self.session.cwd) {
                RuleScope::Project
            } else {
                RuleScope::User
            };
            match envoy_agent::permissions::rules::save_rule(
                &self.data_dir,
                &self.session.cwd,
                scope,
                envoy_agent::permissions::Effect::Deny,
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

    pub(in crate::tui) fn run_mcp_command(&mut self, arg: &str) {
        let report = envoy_agent::mcp::config::load_mcp_configs(&self.data_dir, &self.session.cwd);
        if arg.trim() == "probe" {
            let data_dir = self.data_dir.clone();
            let cwd = self.session.cwd.clone();
            let notice = self.notice_tx.clone();
            self.set_toast("probing MCP servers.", false);
            tokio::spawn(async move {
                let mgr =
                    envoy_agent::mcp::client::McpManager::connect_configured(&data_dir, &cwd).await;
                let mut lines = Vec::new();
                for p in mgr.skipped_untrusted() {
                    lines.push(format!(
                        "skipped untrusted project MCP config: {}",
                        p.display()
                    ));
                }
                for s in mgr.statuses() {
                    if let Some(err) = &s.error {
                        lines.push(format!("{} [{}] error: {}", s.name, s.transport_label, err));
                    } else {
                        lines.push(format!(
                            "{} [{}] connected ({} tools)",
                            s.name, s.transport_label, s.tool_count
                        ));
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
            lines.push(format!(
                "skipped untrusted: {} (run `/trust on` to enable)",
                p.display()
            ));
        }
        for s in &report.servers {
            let kind = match s.transport {
                envoy_agent::mcp::config::McpTransportConfig::Stdio { .. } => "stdio",
                envoy_agent::mcp::config::McpTransportConfig::Http { .. } => "http",
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
            lines.push("MCP: no servers configured (~/.envoy/mcp.json or .mcp.json)".into());
        } else {
            lines.push("Tip: run `/mcp probe` to connect and count tools.".into());
        }
        self.push_notice(lines.join("\n"), false);
    }

    pub(in crate::tui) fn run_skill_command(&mut self, arg: &str) {
        let name = arg.trim();
        if name.is_empty() {
            let skills = envoy_agent::skills::discover_skills(&self.data_dir, &self.session.cwd);
            if skills.is_empty() {
                self.push_notice(
                    "Skills: none found (add ~/.envoy/skills/<name>/SKILL.md or trust project .envoy/skills)",
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
        match envoy_agent::skills::load_skill(&self.data_dir, &self.session.cwd, name) {
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
        let Some(tier) = envoy_agent::agent::CompactionTier::parse(arg) else {
            self.set_toast("usage: /budget [native|focused|balanced|max|400k]", true);
            return;
        };
        if let Err(e) =
            envoy_agent::agent::context::set_budget_tier(&self.data_dir, tier, Some(&model))
        {
            self.set_toast(format!("{e:#}"), true);
            return;
        }
        let plan =
            envoy_agent::agent::context::resolve_budget(&self.data_dir, &prov.id, &model.model_id);
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
        use envoy_agent::agent::delegate_pref::{self, DelegateLevel};
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

    pub(in crate::tui) fn run_effort_command(&mut self, arg: &str) {
        use envoy_agent::agent::effort;
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
        if !self.target.is_empty() && self.target != "local" {
            self.set_toast("worktree is local only", true);
            return;
        }
        if self.agent_running {
            self.set_toast("wait for the active turn to finish", true);
            return;
        }
        match envoy_agent::worktree::execute(&self.session.cwd, arg) {
            Ok(envoy_agent::worktree::Effect::Listed(text)) => self.push_notice(text, false),
            Ok(envoy_agent::worktree::Effect::Toast(text)) => self.set_toast(text, false),
            Ok(envoy_agent::worktree::Effect::Enter {
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
        let target = self.session.target.clone();
        self.session = Session::new(path, target);
        self.selected_card = None;
        self.follow = true;
        self.expanded_explore_groups.clear();
        self.transcript_cache.clear();
        self.inspector = InspectorState::default();
        self.workspace_index.invalidate();
        self.reset_agent_view();
        self.spawn_agent_attach();
    }
}
