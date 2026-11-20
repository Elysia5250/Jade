//! Keyboard handling for the login wizard.
use crate::tui::*;

impl App {
    pub(in crate::tui) fn handle_login_key(&mut self, login: LoginModal, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.cancel_login_bg();
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
                    KeyCode::Backspace => {
                        filter.pop();
                        selected = 0;
                    }
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
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        filter.push(c);
                        selected = 0;
                    }
                    _ => {}
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
                    KeyCode::Tab | KeyCode::Down => focus_field = (focus_field + 1) % 3,
                    KeyCode::BackTab | KeyCode::Up => focus_field = (focus_field + 2) % 3,
                    KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        reveal = !reveal;
                    }
                    KeyCode::Backspace => match focus_field {
                        0 => {
                            api_key.pop();
                        }
                        1 => {
                            base_url.pop();
                        }
                        _ => {
                            model.pop();
                        }
                    },
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
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        error = None;
                        match focus_field {
                            0 => api_key.push(c),
                            1 => base_url.push(c),
                            _ => model.push(c),
                        }
                    }
                    _ => {}
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
                    KeyCode::Tab | KeyCode::Down => focus_field = (focus_field + 1) % 5,
                    KeyCode::BackTab | KeyCode::Up => focus_field = (focus_field + 4) % 5,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if focus_field == 0 => {
                        protocol_idx = 1 - protocol_idx;
                        if protocol_idx == 1 && base_url == "http://127.0.0.1:11434/v1" {
                            base_url = "https://api.anthropic.com/v1".into();
                        } else if protocol_idx == 0 && base_url == "https://api.anthropic.com/v1" {
                            base_url = "http://127.0.0.1:11434/v1".into();
                        }
                    }
                    KeyCode::Backspace => match focus_field {
                        1 => {
                            name.pop();
                        }
                        2 => {
                            base_url.pop();
                        }
                        3 => {
                            api_key.pop();
                        }
                        4 => {
                            model.pop();
                        }
                        _ => {}
                    },
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
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        error = None;
                        match focus_field {
                            1 => name.push(c),
                            2 => base_url.push(c),
                            3 => api_key.push(c),
                            4 => model.push(c),
                            _ => {}
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
                        KeyCode::Backspace => {
                            pasted.pop();
                        }
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
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            error = None;
                            pasted.push(c);
                        }
                        _ => {}
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
                        KeyCode::Backspace => {
                            code_input.pop();
                        }
                        KeyCode::Enter if !code_input.trim().is_empty() => {
                            let raw = code_input.trim().to_string();
                            let ver = verifier.clone();
                            let st = state.clone();
                            let (tx, rx) = mpsc::unbounded_channel();
                            let handle = tokio::spawn(async move {
                                match exchange_anthropic_code_default(&raw, &ver, &st, console).await
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
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            error = None;
                            code_input.push(c);
                        }
                        _ => {}
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

    pub(in crate::tui) fn handle_login_paste(&mut self, mut login: LoginModal, clean: &str) {
        if !clean.is_empty() {
            match &mut login {
                LoginModal::SelectProvider { filter, selected } => {
                    filter.push_str(clean);
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
                    match *focus_field {
                        0 => key.push_str(clean),
                        1 => base_url.push_str(clean),
                        _ => model.push_str(clean),
                    }
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
                    match *focus_field {
                        1 => name.push_str(clean),
                        2 => base_url.push_str(clean),
                        3 => key.push_str(clean),
                        4 => model.push_str(clean),
                        _ => {}
                    }
                }
                LoginModal::OAuthBrowser {
                    pasted,
                    busy,
                    error,
                    ..
                } if !*busy => {
                    *error = None;
                    pasted.push_str(clean);
                }
                LoginModal::OAuthCodePaste {
                    code_input,
                    busy,
                    error,
                    ..
                } if !*busy => {
                    *error = None;
                    code_input.push_str(clean);
                }
                _ => {}
            }
        }
        self.ui.modal = Some(Modal::Login(login));
    }
}
