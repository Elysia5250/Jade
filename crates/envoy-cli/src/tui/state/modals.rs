//! Picker, models, sessions, and generic modal handling.
use super::*;

impl App {

    pub(in crate::tui) fn open_models_modal(&mut self) {
        let models = available_models(&self.data_dir);
        if models.is_empty() {
            self.set_toast("no providers connected — run /login first", true);
            self.open_login_modal();
            return;
        }
        let mut fetching = false;
        if let Some((prov, _)) = resolve_active_provider(&self.data_dir) {
            if matches!(prov.protocol, ProviderProtocol::OpenAiChat) {
                if let Credential::ApiKey { key } = prov.credential {
                    let base = prov.base_url.clone();
                    let (tx, rx) = oneshot::channel();
                    tokio::spawn(async move {
                        let http = reqwest::Client::new();
                        let list = fetch_openai_models(&http, &base, &key).await;
                        let _ = tx.send(list);
                    });
                    self.models_rx = Some(rx);
                    fetching = true;
                }
            }
        }
        self.modal = Some(Modal::Models(ModelsModal {
            filter: String::new(),
            selected: 0,
            models,
            fetching,
        }));
    }


    pub(in crate::tui) fn drain_model_events(&mut self) {
        let Some(mut rx) = self.models_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(fetched) => {
                if let Some(Modal::Models(m)) = self.modal.as_mut() {
                    m.fetching = false;
                    if let Some((prov, _)) = resolve_active_provider(&self.data_dir) {
                        for id in fetched {
                            if !m
                                .models
                                .iter()
                                .any(|x| x.provider_id == prov.id && x.model_id == id)
                            {
                                m.models.push(AvailableModel {
                                    provider_id: prov.id.clone(),
                                    provider_name: prov.name.clone(),
                                    model_id: id,
                                    auth_kind: prov.credential.kind_label(),
                                });
                            }
                        }
                    }
                }
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                self.models_rx = Some(rx);
            }
            Err(_) => {}
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
                format!(
                    "Switch mode (now {})",
                    self.mode.label()
                ),
                Command::ToggleMode,
            ),
            ("New session (/new)".to_string(), Command::NewSession),
            (
                "Switch session (/sessions)".to_string(),
                Command::OpenSessions,
            ),
            (
                "Expand / collapse output cards".to_string(),
                Command::ToggleExpand,
            ),
            ("Toggle sidebar".to_string(), Command::ToggleSidebar),
            ("Switch theme".to_string(), Command::NextTheme),
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
                    KeyCode::Down | KeyCode::Char('j') => s.selected = (s.selected + 1).min(len - 1),
                    KeyCode::Char('d') => {
                        if let Some(target) = s.sessions.get(s.selected).map(|x| x.id.clone()) {
                            let _ = delete_session(&self.data_dir, &target);
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
        }
    }

}
