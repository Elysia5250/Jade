//! Login method selection and provider-specific setup.
use super::openai_url_preview;
use crate::tui::*;

impl App {
    pub(in crate::tui) fn start_login_method(&mut self, provider_idx: usize, method_idx: usize) {
        let prov = &BUILTIN_PROVIDERS[provider_idx];
        let method = prov.methods[method_idx];
        match method.kind {
            AuthMethodKind::OpenAiApiKey
            | AuthMethodKind::AnthropicApiKey
            | AuthMethodKind::SimpleApiKey => {
                self.ui.modal_cursor = 0;
                self.ui.modal = Some(Modal::Login(LoginModal::ApiKeyForm {
                    provider_idx,
                    key: String::new(),
                    base_url: prov.default_base_url.to_string(),
                    model: String::new(),
                    focus_field: 0,
                    reveal: false,
                    error: None,
                }));
            }
            AuthMethodKind::CustomForm => {
                let initial_name = "Custom Provider".to_string();
                self.ui.modal_cursor = initial_name.chars().count();
                self.ui.modal = Some(Modal::Login(LoginModal::CustomForm {
                    protocol_idx: 0,
                    name: initial_name,
                    base_url: "http://127.0.0.1:11434/v1".to_string(),
                    key: String::new(),
                    model: String::new(),
                    focus_field: 1,
                    error: None,
                }));
            }
            AuthMethodKind::OpenAiBrowserOAuth => {
                let Ok(pkce) = generate_pkce() else {
                    self.set_toast("failed to generate PKCE", true);
                    return;
                };
                let Ok(state) = random_state() else {
                    self.set_toast("failed to generate OAuth state", true);
                    return;
                };
                let (tx, rx) = mpsc::unbounded_channel();
                let (cancel_tx, cancel_rx) = oneshot::channel();
                let verifier = pkce.verifier.clone();
                let challenge = pkce.challenge.clone();
                let st = state.clone();
                // Optimistically show port 1455 URL immediately; task updates if different
                let initial_redirect = "http://localhost:1455/auth/callback".to_string();
                let initial_url = openai_url_preview(&initial_redirect, &challenge, &st);
                let tx_task = tx.clone();
                let handle = tokio::spawn(async move {
                    match bind_openai_callback_listener().await {
                        Ok((listener, port)) => {
                            let redirect_uri = format!("http://localhost:{port}/auth/callback");
                            let url = jade_agent::auth::openai_authorize_url(
                                &redirect_uri,
                                &jade_agent::auth::Pkce {
                                    verifier: verifier.clone(),
                                    challenge,
                                },
                                &st,
                            );
                            let _ = tx_task.send(LoginEvent::OpenAiBrowserBound {
                                url: url.clone(),
                                redirect_uri: redirect_uri.clone(),
                            });
                            open_browser(&url);
                            match wait_openai_browser_callback(listener, st, cancel_rx).await {
                                Ok(code) => {
                                    match exchange_openai_code_default(
                                        &code,
                                        &redirect_uri,
                                        &verifier,
                                        "chatgpt-browser",
                                    )
                                    .await
                                    {
                                        Ok(sp) => {
                                            let _ = tx_task.send(LoginEvent::Success(sp, None));
                                        }
                                        Err(e) => {
                                            let _ =
                                                tx_task.send(LoginEvent::Error(format!("{e:#}")));
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = tx_task.send(LoginEvent::Error(format!("{e:#}")));
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx_task.send(LoginEvent::Error(format!("{e:#}")));
                        }
                    }
                });
                self.login.rx = Some(rx);
                self.login.task = Some(handle);
                self.login.cancel = Some(cancel_tx);
                self.ui.modal_cursor = 0;
                self.ui.modal = Some(Modal::Login(LoginModal::OAuthBrowser {
                    provider_name: prov.name.to_string(),
                    url: initial_url,
                    redirect_uri: initial_redirect,
                    verifier: pkce.verifier,
                    pasted: String::new(),
                    status: "Waiting for browser callback (or paste redirect URL below)...".into(),
                    busy: false,
                    error: None,
                }));
            }
            AuthMethodKind::OpenAiDeviceOAuth => {
                let (tx, rx) = mpsc::unbounded_channel();
                let handle = tokio::spawn(async move {
                    match start_openai_device_auth_default().await {
                        Ok(start) => {
                            open_browser(&start.verification_url);
                            let _ = tx.send(LoginEvent::OpenAiDeviceStarted {
                                verification_url: start.verification_url.clone(),
                                user_code: start.user_code.clone(),
                            });
                            match poll_openai_device_auth_default(&start).await {
                                Ok(sp) => {
                                    let _ = tx.send(LoginEvent::Success(sp, None));
                                }
                                Err(e) => {
                                    let _ = tx.send(LoginEvent::Error(format!("{e:#}")));
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(LoginEvent::Error(format!("{e:#}")));
                        }
                    }
                });
                self.login.rx = Some(rx);
                self.login.task = Some(handle);
                self.ui.modal = Some(Modal::Login(LoginModal::OAuthDevice {
                    provider_name: prov.name.to_string(),
                    verification_uri: "https://auth.openai.com/codex/device".into(),
                    user_code: "requesting.".into(),
                    status: "Requesting device code from OpenAI.".into(),
                    error: None,
                }));
            }
            AuthMethodKind::AnthropicClaudeOAuth | AuthMethodKind::AnthropicConsoleOAuth => {
                let console = matches!(method.kind, AuthMethodKind::AnthropicConsoleOAuth);
                let Ok(pkce) = generate_pkce() else {
                    self.set_toast("failed to generate PKCE", true);
                    return;
                };
                let Ok(state) = random_state() else {
                    self.set_toast("failed to generate OAuth state", true);
                    return;
                };
                let url = anthropic_authorize_url(console, &pkce, &state);
                open_browser(&url);
                self.ui.modal_cursor = 0;
                self.ui.modal = Some(Modal::Login(LoginModal::OAuthCodePaste {
                    provider_name: if console {
                        "Anthropic Console (OAuth, experimental / unofficial)".into()
                    } else {
                        "Claude Pro / Max (OAuth, experimental / unofficial)".into()
                    },
                    url,
                    verifier: pkce.verifier,
                    state,
                    console,
                    code_input: String::new(),
                    busy: false,
                    error: None,
                }));
            }
            AuthMethodKind::GitHubCopilotDeviceOAuth => {
                let (tx, rx) = mpsc::unbounded_channel();
                let handle = tokio::spawn(async move {
                    match start_github_copilot_device_default().await {
                        Ok(start) => {
                            open_browser(&start.verification_uri);
                            let _ = tx.send(LoginEvent::GitHubDeviceStarted {
                                verification_uri: start.verification_uri.clone(),
                                user_code: start.user_code.clone(),
                            });
                            match poll_github_copilot_device_default(&start).await {
                                Ok(sp) => {
                                    let _ = tx.send(LoginEvent::Success(sp, None));
                                }
                                Err(e) => {
                                    let _ = tx.send(LoginEvent::Error(format!("{e:#}")));
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(LoginEvent::Error(format!("{e:#}")));
                        }
                    }
                });
                self.login.rx = Some(rx);
                self.login.task = Some(handle);
                self.ui.modal = Some(Modal::Login(LoginModal::OAuthDevice {
                    provider_name: prov.name.to_string(),
                    verification_uri: "https://github.com/login/device".into(),
                    user_code: "requesting.".into(),
                    status: "Requesting device code from GitHub.".into(),
                    error: None,
                }));
            }
        }
    }
}
