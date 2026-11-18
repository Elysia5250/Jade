//! Picker, models, sessions, and generic modal handling.
use super::*;

impl App {
    pub(in crate::tui) fn open_models_modal(&mut self) {
        if effective_providers(&self.data_dir).is_empty() {
            self.set_toast("no providers connected - run /login first", true);
            self.open_login_modal();
            return;
        }
        let fetching = self.spawn_model_refresh();
        self.modal = Some(Modal::Models(ModelsModal {
            filter: String::new(),
            selected: 0,
            models: available_models(&self.data_dir),
            fetching,
        }));
    }

    /// Ask the active provider for its live model list. Returns false when
    /// there is nothing to ask, or this thread has no async runtime.
    pub(in crate::tui) fn spawn_model_refresh(&mut self) -> bool {
        if tokio::runtime::Handle::try_current().is_err() {
            return false;
        }
        let Some((prov, _)) = resolve_active_provider(&self.data_dir) else {
            return false;
        };
        let dir = self.data_dir.clone();
        tokio::spawn(async move {
            let http = reqwest::Client::new();
            crate::agent::context::refresh_models_dev_if_stale(&http, &dir).await;
        });
        let dir = self.data_dir.clone();
        let pid = prov.id.clone();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let http = reqwest::Client::new();
            let list = crate::agent::context::fetch_provider_models(&http, Some(&dir), &prov).await;
            let _ = tx.send((pid, list));
        });
        self.models_rx = Some(rx);
        true
    }

    pub(in crate::tui) fn drain_model_events(&mut self) {
        let Some(mut rx) = self.models_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok((provider_id, fetched)) => {
                if !provider_id.is_empty() {
                    match crate::agent::context::store_listed_models(
                        &self.data_dir,
                        &provider_id,
                        &fetched,
                    ) {
                        Ok(Some(mref)) => {
                            if fetched.is_empty() {
                                self.set_toast("model list came back empty", true);
                            } else {
                                self.set_toast(format!("models: {}", mref.display()), false);
                            }
                        }
                        Ok(None) if !fetched.is_empty() => {}
                        Ok(None) => self.set_toast("could not refresh the model list", true),
                        Err(e) => self.set_toast(format!("{e:#}"), true),
                    }
                }
                if let Some(Modal::Models(m)) = self.modal.as_mut() {
                    m.fetching = false;
                    m.models = available_models(&self.data_dir);
                    m.selected = m.selected.min(m.models.len().saturating_sub(1));
                }
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                self.models_rx = Some(rx);
            }
            Err(_) => {
                if let Some(Modal::Models(m)) = self.modal.as_mut() {
                    m.fetching = false;
                }
            }
        }
    }

    pub(in crate::tui) fn open_sessions_modal(&mut self) {
        self.persist_session();
        let sessions = list_sessions(&self.data_dir);
        self.modal = Some(Modal::Sessions(SessionsModal {
            sessions,
            selected: 0,
        }));
    }

    pub(in crate::tui) fn open_picker(&mut self, title: &str, items: Vec<(String, Command)>) {
        self.modal = Some(Modal::Picker(Picker {
            title: title.to_string(),
            query: String::new(),
            items,
            selected: 0,
        }));
    }

    fn mark_current(current: bool, label: &str) -> String {
        format!("{}{label}", if current { "* " } else { "  " })
    }

    fn select_picker(&mut self, index: usize) {
        if let Some(Modal::Picker(picker)) = self.modal.as_mut() {
            let last = picker.items.len().saturating_sub(1);
            picker.selected = index.min(last);
        }
    }

    pub(in crate::tui) fn open_mode_picker(&mut self) {
        let current = self.mode;
        self.open_picker(
            "mode",
            vec![
                (
                    Self::mark_current(current == InputMode::Agent, "agent"),
                    Command::SetMode(InputMode::Agent),
                ),
                (
                    Self::mark_current(current == InputMode::Shell, "shell"),
                    Command::SetMode(InputMode::Shell),
                ),
            ],
        );
        self.select_picker(if current == InputMode::Shell { 1 } else { 0 });
    }

    pub(in crate::tui) fn open_permission_picker(&mut self) {
        let current = self.perm_mode;
        let rows = [
            (PermissionMode::Default, "ask", "prompt before tools"),
            (
                PermissionMode::AcceptEdits,
                "accept-edits",
                "approve file edits",
            ),
            (PermissionMode::Plan, "plan", "read-only"),
            (PermissionMode::Auto, "auto", "allow tools"),
        ];
        let selected = rows
            .iter()
            .position(|(mode, _, _)| *mode == current)
            .unwrap_or(0);
        let items = rows
            .into_iter()
            .map(|(mode, name, about)| {
                (
                    Self::mark_current(mode == current, &format!("{name}  {about}")),
                    Command::SetPermission(mode),
                )
            })
            .collect();
        self.open_picker("permissions", items);
        self.select_picker(selected);
    }

    pub(in crate::tui) fn open_theme_picker(&mut self) {
        let current = self.theme;
        let items = THEMES
            .iter()
            .enumerate()
            .map(|(idx, (name, _))| {
                (
                    Self::mark_current(idx == current, name),
                    Command::SetTheme(idx),
                )
            })
            .collect();
        self.open_picker("theme", items);
        self.select_picker(current);
    }

    pub(in crate::tui) fn open_trust_picker(&mut self) {
        let on = is_workspace_trusted(&self.data_dir, &self.session.cwd);
        self.open_picker(
            "trust",
            vec![
                (
                    Self::mark_current(on, "on   trust this workspace"),
                    Command::SetTrust(true),
                ),
                (
                    Self::mark_current(!on, "off  untrust this workspace"),
                    Command::SetTrust(false),
                ),
            ],
        );
        self.select_picker(if on { 0 } else { 1 });
    }

    pub(in crate::tui) fn open_sidebar_picker(&mut self) {
        let shown = self.show_sidebar;
        self.open_picker(
            "inspector",
            vec![
                (Self::mark_current(shown, "show"), Command::SetSidebar(true)),
                (
                    Self::mark_current(!shown, "hide"),
                    Command::SetSidebar(false),
                ),
                ("  reset width".to_string(), Command::ResetSidebar),
            ],
        );
        self.select_picker(if shown { 0 } else { 1 });
    }

    pub(in crate::tui) fn open_mouse_picker(&mut self) {
        let on = self.mouse_capture;
        self.open_picker(
            "mouse",
            vec![
                (
                    Self::mark_current(on, "on   capture mouse"),
                    Command::SetMouse(true),
                ),
                (
                    Self::mark_current(!on, "off  native selection"),
                    Command::SetMouse(false),
                ),
            ],
        );
        self.select_picker(if on { 0 } else { 1 });
    }

    pub(in crate::tui) fn open_budget_picker(&mut self) {
        let Some((prov, model)) = resolve_active_provider(&self.data_dir) else {
            self.set_toast("no provider connected - run /login first", true);
            return;
        };
        let plan = crate::agent::context::resolve_budget(&self.data_dir, &prov.id, &model.model_id);
        let current = plan.tier.label();
        let tiers = ["native", "focused", "balanced", "max"];
        let selected = tiers.iter().position(|tier| *tier == current).unwrap_or(0);
        let items = tiers
            .into_iter()
            .map(|tier| {
                (
                    Self::mark_current(tier == current, tier),
                    Command::SetBudget(tier),
                )
            })
            .collect();
        self.open_picker(
            &format!(
                "budget  {}  compact {}  reserve {}  {} ({})",
                plan.tier,
                fmt_tokens(plan.compact_at),
                fmt_tokens(plan.reserve_output),
                plan.harness_profile,
                plan.source,
            ),
            items,
        );
        self.select_picker(selected);
    }

    pub(in crate::tui) fn machine_items(&self) -> Vec<(String, Command)> {
        let mut items = Vec::new();
        for peer in self
            .status
            .as_ref()
            .map(|s| s.peers.as_slice())
            .unwrap_or(&[])
        {
            if peer.device_id == self.own_device_id()
                || !peer.has_service(envoy_proto::AGENT_SERVICE_ID)
            {
                continue;
            }
            items.push((
                format!("{} ({})", peer.name, peer.device_id),
                Command::Switch(peer.device_id.clone()),
            ));
        }
        if self.target != "local" {
            items.push(("local".to_string(), Command::Switch("local".into())));
        }
        if items.is_empty() {
            items.push((
                "(no other agent machines online)".to_string(),
                Command::Refresh,
            ));
        }
        items
    }

    pub(in crate::tui) fn command_items(&self) -> Vec<(String, Command)> {
        let mut items = vec![
            (
                "Connect provider / Login (API Key or OAuth)".to_string(),
                Command::OpenLogin,
            ),
            ("Switch model (/models)".to_string(), Command::OpenModels),
            (
                format!("Switch mode (now {})", self.mode.label()),
                Command::OpenModePicker,
            ),
            (
                format!("Set permission mode (now {})", self.perm_mode.label()),
                Command::OpenPermissionPicker,
            ),
            ("New session (/new)".to_string(), Command::NewSession),
            (
                "Switch session (/sessions)".to_string(),
                Command::OpenSessions,
            ),
            (
                "Rewind checkpoint (/rewind)".to_string(),
                Command::OpenRewind,
            ),
            (
                "Expand / collapse output cards".to_string(),
                Command::ToggleExpand,
            ),
            ("Inspector panel".to_string(), Command::OpenSidebarPicker),
            ("Mouse capture".to_string(), Command::OpenMousePicker),
            ("Switch theme".to_string(), Command::OpenThemePicker),
            ("Context budget".to_string(), Command::OpenBudgetPicker),
            ("Cancel active turn / job".to_string(), Command::Cancel),
            ("Refresh".to_string(), Command::Refresh),
        ];
        for peer in self
            .status
            .as_ref()
            .map(|s| s.peers.as_slice())
            .unwrap_or(&[])
        {
            if peer.device_id == self.own_device_id()
                || !peer.has_service(envoy_proto::AGENT_SERVICE_ID)
            {
                continue;
            }
            items.push((
                format!("Switch machine: {} ({})", peer.name, peer.device_id),
                Command::Switch(peer.device_id.clone()),
            ));
        }
        if self.target != "local" {
            items.push((
                "Switch machine: local".to_string(),
                Command::Switch("local".into()),
            ));
        }
        items.push(("Focus prompt".to_string(), Command::FocusInput));
        items.push(("Quit".to_string(), Command::Quit));
        items
    }

    pub(in crate::tui) fn handle_modal_key(&mut self, key: KeyEvent) -> Action {
        let Some(modal) = self.modal.take() else {
            return Action::None;
        };
        match modal {
            Modal::Picker(mut p) => {
                let filtered: Vec<(usize, &(String, Command))> = p
                    .items
                    .iter()
                    .enumerate()
                    .filter(|(_, (label, _))| {
                        p.query.is_empty()
                            || label
                                .to_ascii_lowercase()
                                .contains(&p.query.to_ascii_lowercase())
                    })
                    .collect();
                let len = filtered.len().max(1);
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Up => p.selected = p.selected.saturating_sub(1),
                    KeyCode::Down => p.selected = (p.selected + 1).min(len - 1),
                    KeyCode::Char('k') if p.query.is_empty() => {
                        p.selected = p.selected.saturating_sub(1);
                    }
                    KeyCode::Char('j') if p.query.is_empty() => {
                        p.selected = (p.selected + 1).min(len - 1);
                    }
                    KeyCode::Backspace => {
                        p.query.pop();
                        p.selected = 0;
                    }
                    KeyCode::Enter => {
                        if let Some((_, (_, cmd))) = filtered.get(p.selected) {
                            let cmd = cmd.clone();
                            return self.run_command(cmd);
                        }
                        return Action::None;
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        p.query.push(c);
                        p.selected = 0;
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Picker(p));
                Action::None
            }
            Modal::Login(login) => {
                self.handle_login_key(login, key);
                Action::None
            }
            Modal::Models(mut m) => {
                let filtered: Vec<&AvailableModel> = m
                    .models
                    .iter()
                    .filter(|item| {
                        m.filter.is_empty()
                            || item
                                .model_id
                                .to_ascii_lowercase()
                                .contains(&m.filter.to_ascii_lowercase())
                            || item
                                .provider_id
                                .to_ascii_lowercase()
                                .contains(&m.filter.to_ascii_lowercase())
                    })
                    .collect();
                let len = filtered.len().max(1);
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Up => m.selected = m.selected.saturating_sub(1),
                    KeyCode::Down => m.selected = (m.selected + 1).min(len - 1),
                    KeyCode::Backspace => {
                        m.filter.pop();
                        m.selected = 0;
                    }
                    KeyCode::Enter => {
                        let chosen = if let Some(item) = filtered.get(m.selected) {
                            Some(ModelRef {
                                provider_id: item.provider_id.clone(),
                                model_id: item.model_id.clone(),
                            })
                        } else if !m.filter.trim().is_empty() {
                            resolve_active_provider(&self.data_dir).map(|(p, _)| ModelRef {
                                provider_id: p.id,
                                model_id: m.filter.trim().to_string(),
                            })
                        } else {
                            None
                        };
                        if let Some(mref) = chosen {
                            if let Err(e) = set_active_model(&self.data_dir, mref.clone()) {
                                self.set_toast(format!("{e:#}"), true);
                            } else {
                                self.mode = InputMode::Agent;
                                self.set_toast(format!("model: {}", mref.display()), false);
                            }
                        }
                        return Action::None;
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        m.filter.push(c);
                        m.selected = 0;
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Models(m));
                Action::None
            }
            Modal::Sessions(mut s) => {
                let len = s.sessions.len().max(1);
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Up | KeyCode::Char('k') => s.selected = s.selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        s.selected = (s.selected + 1).min(len - 1)
                    }
                    KeyCode::Char('d') => {
                        if let Some(target) = s.sessions.get(s.selected).map(|x| x.id.clone()) {
                            let _ = delete_session(&self.data_dir, &target);
                            let data_dir = self.data_dir.clone();
                            let close_id = target.clone();
                            tokio::spawn(async move {
                                let _ = runtime::tell(
                                    &data_dir,
                                    AgentReq::CloseSession { id: close_id },
                                )
                                .await;
                            });
                            s.sessions.retain(|x| x.id != target);
                            s.selected = s.selected.min(s.sessions.len().saturating_sub(1));
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(summary) = s.sessions.get(s.selected) {
                            match load_session(&self.data_dir, &summary.id) {
                                Ok(loaded) => {
                                    self.load_existing_session(loaded);
                                    self.spawn_agent_attach();
                                    self.set_toast("session loaded", false);
                                }
                                Err(e) => self.set_toast(format!("{e:#}"), true),
                            }
                        }
                        return Action::None;
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Sessions(s));
                Action::None
            }
            Modal::Rewind(mut r) => {
                let len = r.checkpoints.len().max(1);
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Up | KeyCode::Char('k') => r.selected = r.selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        r.selected = (r.selected + 1).min(len - 1)
                    }
                    KeyCode::Enter => {
                        if let Some(cp) = r.checkpoints.get(r.selected) {
                            self.request_rewind(Some(cp.id));
                        }
                        return Action::None;
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Rewind(r));
                Action::None
            }
        }
    }

    pub(in crate::tui) fn handle_modal_paste(&mut self, modal: Modal, text: &str) {
        let clean: String = text.chars().filter(|c| !matches!(c, '\r' | '\n')).collect();
        match modal {
            Modal::Picker(mut p) => {
                if !clean.is_empty() {
                    p.query.push_str(&clean);
                    p.selected = 0;
                }
                self.modal = Some(Modal::Picker(p));
            }
            Modal::Models(mut m) => {
                if !clean.is_empty() {
                    m.filter.push_str(&clean);
                    m.selected = 0;
                }
                self.modal = Some(Modal::Models(m));
            }
            Modal::Login(login) => self.handle_login_paste(login, &clean),
            other => self.modal = Some(other),
        }
    }
}
