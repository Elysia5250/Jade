//! Argument handlers for /permissions, /mcp, /skill, and /budget.
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
                "remote"
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

    pub(in crate::tui) fn run_mcp_command(&mut self, arg: &str) {
        let report = crate::mcp::config::load_mcp_configs(&self.data_dir, &self.session.cwd);
        if arg.trim() == "probe" {
            let data_dir = self.data_dir.clone();
            let cwd = self.session.cwd.clone();
            let notice = self.notice_tx.clone();
            self.set_toast("probing MCP servers.", false);
            tokio::spawn(async move {
                let mgr = crate::mcp::client::McpManager::connect_configured(&data_dir, &cwd).await;
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
                crate::mcp::config::McpTransportConfig::Stdio { .. } => "stdio",
                crate::mcp::config::McpTransportConfig::Http { .. } => "http",
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
                .map(|s| format!("{} [{}] - {}", s.name, s.scope.label(), s.description))
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

    pub(in crate::tui) fn run_budget_command(&mut self, arg: &str) {
        let Some((prov, model)) = resolve_active_provider(&self.data_dir) else {
            self.set_toast("no provider connected - run /login first", true);
            return;
        };
        if arg.is_empty() {
            self.open_budget_picker();
            return;
        }
        let Some(tier) = crate::agent::CompactionTier::parse(arg) else {
            self.set_toast("usage: /budget [native|focused|balanced|max|400k]", true);
            return;
        };
        if let Err(e) = crate::agent::context::set_budget_tier(&self.data_dir, tier, Some(&model)) {
            self.set_toast(format!("{e:#}"), true);
            return;
        }
        let plan = crate::agent::context::resolve_budget(&self.data_dir, &prov.id, &model.model_id);
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

}
