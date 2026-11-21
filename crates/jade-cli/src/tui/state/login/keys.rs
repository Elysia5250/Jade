//! Keyboard handling for the login wizard.
use crate::tui::line_edit::{self, EditOutcome};
use crate::tui::*;

impl App {
    pub(in crate::tui) fn handle_login_key(&mut self, login: LoginModal, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.handle_login_esc(login);
            return;
        }
        match login {
            LoginModal::SelectProvider {
                mut filter,
                mut selected,
            } => {
                let list = self.filtered_login_providers(&filter);
                let len = list.len().max(1);
                match key.code {
                    KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Down => selected = (selected + 1).min(len - 1),
                    KeyCode::Enter => {
                        if let Some(&(provider_idx, prov)) = list.get(selected) {
                            if prov.methods.len() == 1 {
                                self.start_login_method(provider_idx, 0);
                            } else {
                                self.ui.modal = Some(Modal::Login(LoginModal::SelectMethod {
                                    provider_idx,
                                    selected: 0,
                                }));
                            }
                            return;
                        }
                    }
                    _ => {
                        if line_edit::handle_key(&mut filter, &mut self.ui.modal_cursor, key)
                            == EditOutcome::Changed
                        {
                            selected = 0;
                        }
                    }
                }
                self.ui.modal = Some(Modal::Login(LoginModal::SelectProvider {
                    filter,
                    selected,
                }));
            }
            LoginModal::SelectMethod {
                provider_idx,
                mut selected,
            } => {
                let prov = &BUILTIN_PROVIDERS[provider_idx];
                let len = prov.methods.len().max(1);
                match key.code {
                    KeyCode::Left | KeyCode::Backspace => {
                        self.ui.modal_cursor = 0;
                        self.ui.modal = Some(Modal::Login(LoginModal::SelectProvider {
                            filter: String::new(),
                            selected: provider_idx,
                        }));
                        return;
                    }
                    KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(len - 1),
                    KeyCode::Enter => {
                        self.start_login_method(provider_idx, selected);
                        return;
                    }
                    _ => {}
                }
                self.ui.modal = Some(Modal::Login(LoginModal::SelectMethod {
                    provider_idx,
                    selected,
                }));
            }
            LoginModal::ApiKeyForm {
                provider_idx,
                key: mut api_key,
                mut base_url,
                mut model,
                mut focus_field,
                mut reveal,
                mut error,
            } => {
                let prov = &BUILTIN_PROVIDERS[provider_idx];
                match key.code {
                    KeyCode::Tab | KeyCode::Down => {
                        focus_field = (focus_field + 1) % 3;
                        self.sync_api_key_cursor(&api_key, &base_url, &model, focus_field);
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        focus_field = (focus_field + 2) % 3;
                        self.sync_api_key_cursor(&api_key, &base_url, &model, focus_field);
                    }
                    KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        reveal = !reveal;
                    }
                    KeyCode::Enter => {
                        if api_key.trim().is_empty() {
                            error = Some("API key cannot be empty".into());
                        } else {
                            let custom_models = if model.trim().is_empty() {
                                Vec::new()
                            } else {
                                vec![model.trim().to_string()]
                            };
                            let sp = StoredProvider {
                                id: prov.id.to_string(),
                                name: prov.name.to_string(),
                                protocol: prov.protocol,
                                base_url: if base_url.trim().is_empty() {
                                    prov.default_base_url.to_string()
                                } else {
                                    base_url.trim().trim_end_matches('/').to_string()
                                },
                                credential: Credential::ApiKey {
                                    key: api_key.trim().to_string(),
                                },
                                custom_models,
                                fetched_models: Vec::new(),
                                model_routes: std::collections::BTreeMap::new(),
                                updated_ms: now_ms(),
                            };
                            let chosen = if model.trim().is_empty() {
                                None
                            } else {
                                Some(model.trim().to_string())
                            };
                            match record_provider_login(&self.data_dir, sp, chosen) {
                                Ok(mref) => {
                                    let name = prov.name.to_string();
                                    self.finish_login(&name, "api_key", &mref);
                                    return;
                                }
                                Err(e) => error = Some(format!("{e:#}")),
                            }
                        }
                    }
                    _ => {
                        let target = match focus_field {
                            0 => &mut api_key,
                            1 => &mut base_url,
                            _ => &mut model,
                        };
                        if line_edit::handle_key(target, &mut self.ui.modal_cursor, key)
                            == EditOutcome::Changed
                        {
                            error = None;
                        }
                    }
                }
                self.ui.modal = Some(Modal::Login(LoginModal::ApiKeyForm {
                    provider_idx,
                    key: api_key,
                    base_url,
                    model,
                    focus_field,
                    reveal,
                    error,
                }));
            }
            LoginModal::CustomForm {
                mut protocol_idx,
                mut name,
                mut base_url,
                key: mut api_key,
                mut model,
                mut focus_field,
                mut error,
            } => {
                match key.code {
                    KeyCode::Tab | KeyCode::Down => {
                        focus_field = (focus_field + 1) % 5;
                        self.sync_custom_cursor(&name, &base_url, &api_key, &model, focus_field);
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        focus_field = (focus_field + 4) % 5;
                        self.sync_custom_cursor(&name, &base_url, &api_key, &model, focus_field);
                    }
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if focus_field == 0 => {
                        protocol_idx = 1 - protocol_idx;
                        if protocol_idx == 1 && base_url == "http://127.0.0.1:11434/v1" {
                            base_url = "https://api.anthropic.com/v1".into();
                        } else if protocol_idx == 0 && base_url == "https://api.anthropic.com/v1" {
                            base_url = "http://127.0.0.1:11434/v1".into();
                        }
                    }
                    KeyCode::Enter => {
                        if base_url.trim().is_empty() {
                            error = Some("Base URL is required".into());
                        } else if model.trim().is_empty() {
                            error = Some("Default Model ID is required".into());
                        } else {
                            let protocol = if protocol_idx == 1 {
                                ProviderProtocol::Anthropic
                            } else {
                                ProviderProtocol::OpenAiChat
                            };
                            let id = slugify_provider_id(&name);
                            let sp = StoredProvider {
                                id,
                                name: if name.trim().is_empty() {
                                    "Custom".into()
                                } else {
                                    name.trim().to_string()
                                },
                                protocol,
                                base_url: base_url.trim().trim_end_matches('/').to_string(),
                                credential: Credential::ApiKey {
                                    key: api_key.trim().to_string(),
                                },
                                custom_models: vec![model.trim().to_string()],
                                fetched_models: Vec::new(),
                                model_routes: std::collections::BTreeMap::new(),
                                updated_ms: now_ms(),
                            };
                            let pname = sp.name.clone();
                            match record_provider_login(
                                &self.data_dir,
                                sp,
                                Some(model.trim().to_string()),
                            ) {
                                Ok(mref) => {
                                    self.ui.mode = InputMode::Agent;
                                    self.inputs.invalidate_catalog();
                                    self.set_toast(
                                        format!("connected {pname} - {}", mref.display()),
                                        false,
                                    );
                                    self.push_notice(
                                        format!(
                                            "Connected to {pname} ({}). Active model: {}",
                                            protocol.label(),
                                            mref.display()
                                        ),
                                        false,
                                    );
                                    return;
                                }
                                Err(e) => error = Some(format!("{e:#}")),
                            }
                        }
                    }
                    _ if focus_field > 0 => {
                        let target = match focus_field {
                            1 => &mut name,
                            2 => &mut base_url,
                            3 => &mut api_key,
                            _ => &mut model,
                        };
                        if line_edit::handle_key(target, &mut self.ui.modal_cursor, key)
                            == EditOutcome::Changed
                        {
                            error = None;
                        }
                    }
                    _ => {}
                }
                self.ui.modal = Some(Modal::Login(LoginModal::CustomForm {
                    protocol_idx,
                    name,
                    base_url,
                    key: api_key,
                    model,
                    focus_field,
                    error,
                }));
            }
            LoginModal::OAuthBrowser {
                provider_name,
                url,
                redirect_uri,
                verifier,
                mut pasted,
                mut status,
                mut busy,
                mut error,
            } => {
                if !busy {
                    match key.code {
                        KeyCode::Enter if !pasted.trim().is_empty() => {
                            let code = extract_query_param(pasted.trim(), "code")
                                .unwrap_or_else(|| pasted.trim().to_string());
                            let redir = redirect_uri.clone();
                            let ver = verifier.clone();
                            let (tx, rx) = mpsc::unbounded_channel();
                            self.cancel_login_bg();
                            let handle = tokio::spawn(async move {
                                match exchange_openai_code_default(
                                    &code,
                                    &redir,
                                    &ver,
                                    "chatgpt-browser",
                                )
                                .await
                                {
                                    Ok(sp) => {
                                        let _ = tx.send(LoginEvent::Success(sp, None));
                                    }
                                    Err(e) => {
                                        let _ = tx.send(LoginEvent::Error(format!("{e:#}")));
                                    }
                                }
                            });
                            self.login.rx = Some(rx);
                            self.login.task = Some(handle);
                            busy = true;
                            status = "Exchanging authorization code.".into();
                            error = None;
                        }
                        _ => {
                            if line_edit::handle_key(&mut pasted, &mut self.ui.modal_cursor, key)
                                == EditOutcome::Changed
                            {
                                error = None;
                            }
                        }
                    }
                }
                self.ui.modal = Some(Modal::Login(LoginModal::OAuthBrowser {
                    provider_name,
                    url,
                    redirect_uri,
                    verifier,
                    pasted,
                    status,
                    busy,
                    error,
                }));
            }
            LoginModal::OAuthCodePaste {
                provider_name,
                url,
                verifier,
                state,
                console,
                mut code_input,
                mut busy,
                mut error,
            } => {
                if !busy {
                    match key.code {
                        KeyCode::Enter if !code_input.trim().is_empty() => {
                            let raw = code_input.trim().to_string();
                            let ver = verifier.clone();
                            let st = state.clone();
                            let (tx, rx) = mpsc::unbounded_channel();
                            let handle = tokio::spawn(async move {
                                match exchange_anthropic_code_default(&raw, &ver, &st, console)
                                    .await
                                {
                                    Ok(sp) => {
                                        let _ = tx.send(LoginEvent::Success(sp, None));
                                    }
                                    Err(e) => {
                                        let _ = tx.send(LoginEvent::Error(format!("{e:#}")));
                                    }
                                }
                            });
                            self.login.rx = Some(rx);
                            self.login.task = Some(handle);
                            busy = true;
                            error = None;
                        }
                        _ => {
                            if line_edit::handle_key(
                                &mut code_input,
                                &mut self.ui.modal_cursor,
                                key,
                            ) == EditOutcome::Changed
                            {
                                error = None;
                            }
                        }
                    }
                }
                self.ui.modal = Some(Modal::Login(LoginModal::OAuthCodePaste {
                    provider_name,
                    url,
                    verifier,
                    state,
                    console,
                    code_input,
                    busy,
                    error,
                }));
            }
            LoginModal::OAuthDevice {
                provider_name,
                verification_uri,
                user_code,
                status,
                error,
            } => {
                self.ui.modal = Some(Modal::Login(LoginModal::OAuthDevice {
                    provider_name,
                    verification_uri,
                    user_code,
                    status,
                    error,
                }));
            }
        }
    }

    fn handle_login_esc(&mut self, login: LoginModal) {
        self.cancel_login_bg();
        self.ui.modal_cursor = 0;
        match login {
            LoginModal::SelectProvider { .. } => {
                self.ui.modal = None;
                self.ui.modal_rect = None;
            }
            LoginModal::SelectMethod { provider_idx, .. } => {
                self.ui.modal = Some(Modal::Login(LoginModal::SelectProvider {
                    filter: String::new(),
                    selected: provider_idx,
                }));
            }
            LoginModal::ApiKeyForm { provider_idx, .. } => {
                let prov = &BUILTIN_PROVIDERS[provider_idx];
                if prov.methods.len() > 1 {
                    self.ui.modal = Some(Modal::Login(LoginModal::SelectMethod {
                        provider_idx,
                        selected: 0,
                    }));
                } else {
                    self.ui.modal = Some(Modal::Login(LoginModal::SelectProvider {
                        filter: String::new(),
                        selected: provider_idx,
                    }));
                }
            }
            LoginModal::CustomForm { .. }
            | LoginModal::OAuthBrowser { .. }
            | LoginModal::OAuthCodePaste { .. }
            | LoginModal::OAuthDevice { .. } => {
                self.ui.modal = Some(Modal::Login(LoginModal::SelectProvider {
                    filter: String::new(),
                    selected: 0,
                }));
            }
        }
    }

    fn sync_api_key_cursor(&mut self, api_key: &str, base_url: &str, model: &str, field: usize) {
        let active = match field {
            0 => api_key,
            1 => base_url,
            _ => model,
        };
        self.ui.modal_cursor = active.chars().count();
    }

    fn sync_custom_cursor(
        &mut self,
        name: &str,
        base_url: &str,
        api_key: &str,
        model: &str,
        field: usize,
    ) {
        let active = match field {
            1 => name,
            2 => base_url,
            3 => api_key,
            4 => model,
            _ => "",
        };
        self.ui.modal_cursor = active.chars().count();
    }

    pub(in crate::tui) fn handle_login_paste(&mut self, mut login: LoginModal, clean: &str) {
        if !clean.is_empty() {
            match &mut login {
                LoginModal::SelectProvider { filter, selected } => {
                    line_edit::insert_str(filter, &mut self.ui.modal_cursor, clean);
                    *selected = 0;
                }
                LoginModal::ApiKeyForm {
                    key,
                    base_url,
                    model,
                    focus_field,
                    error,
                    ..
                } => {
                    *error = None;
                    let target = match *focus_field {
                        0 => key,
                        1 => base_url,
                        _ => model,
                    };
                    line_edit::insert_str(target, &mut self.ui.modal_cursor, clean);
                }
                LoginModal::CustomForm {
                    name,
                    base_url,
                    key,
                    model,
                    focus_field,
                    error,
                    ..
                } => {
                    *error = None;
                    if *focus_field > 0 {
                        let target = match *focus_field {
                            1 => name,
                            2 => base_url,
                            3 => key,
                            _ => model,
                        };
                        line_edit::insert_str(target, &mut self.ui.modal_cursor, clean);
                    }
                }
                LoginModal::OAuthBrowser {
                    pasted,
                    busy,
                    error,
                    ..
                } if !*busy => {
                    *error = None;
                    line_edit::insert_str(pasted, &mut self.ui.modal_cursor, clean);
                }
                LoginModal::OAuthCodePaste {
                    code_input,
                    busy,
                    error,
                    ..
                } if !*busy => {
                    *error = None;
                    line_edit::insert_str(code_input, &mut self.ui.modal_cursor, clean);
                }
                _ => {}
            }
        }
        self.ui.modal = Some(Modal::Login(login));
    }
}
