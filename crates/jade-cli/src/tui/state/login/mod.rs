//! Interactive provider login and OAuth/device flows.
use super::*;

mod keys;
mod methods;

impl App {
    pub(in crate::tui) fn open_login_modal(&mut self) {
        self.cancel_login_bg();
        self.ui.modal = Some(Modal::Login(LoginModal::SelectProvider {
            filter: String::new(),
            selected: 0,
        }));
    }

    pub(in crate::tui) fn filtered_login_providers(
        &self,
        filter: &str,
    ) -> Vec<(usize, &'static jade_agent::auth::BuiltinProvider)> {
        let q = filter.trim().to_ascii_lowercase();
        BUILTIN_PROVIDERS
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                q.is_empty()
                    || p.name.to_ascii_lowercase().contains(&q)
                    || p.id.contains(&q)
                    || p.subtitle.to_ascii_lowercase().contains(&q)
            })
            .collect()
    }

    pub(in crate::tui) fn cancel_login_bg(&mut self) {
        if let Some(tx) = self.login.cancel.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.login.task.take() {
            h.abort();
        }
        self.login.rx = None;
    }

    pub(in crate::tui) fn finish_login(&mut self, pname: &str, kind: &str, mref: &ModelRef) {
        self.ui.mode = InputMode::Agent;
        self.ui.modal = None;
        let fetching = mref.model_id.is_empty() && self.spawn_model_refresh();
        let toast = if fetching {
            format!("connected {pname} ({kind}) - fetching models")
        } else if mref.model_id.is_empty() {
            format!("connected {pname} ({kind}) - set a model with /model")
        } else {
            format!("connected {pname} ({kind}) - {}", mref.display())
        };
        self.set_toast(toast, false);
        let notice = if fetching {
            format!("Connected to {pname} ({kind}). Fetching models.")
        } else if mref.model_id.is_empty() {
            format!("Connected to {pname} ({kind}). Set a model with /model.")
        } else {
            format!(
                "Connected to {pname} ({kind}). Active model: {}",
                mref.display()
            )
        };
        self.push_notice(notice, false);
        self.inputs.invalidate_catalog();
    }

    pub(in crate::tui) fn drain_login_events(&mut self) -> bool {
        self.drain_login_from(None)
    }

    pub(in crate::tui) fn drain_login_from(&mut self, first: Option<LoginEvent>) -> bool {
        let mut rx = self.login.rx.take();
        let mut queued = Vec::new();
        if let Some(ev) = first {
            queued.push(ev);
        }
        let mut alive = true;
        if let Some(rx) = rx.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(ev) => queued.push(ev),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        alive = false;
                        break;
                    }
                }
            }
        }
        if queued.is_empty() {
            if alive {
                self.login.rx = rx;
            }
            return false;
        }
        let mut keep = true;
        for ev in queued {
            match ev {
                LoginEvent::OpenAiBrowserBound {
                    url: next_url,
                    redirect_uri: next_redirect,
                } => {
                    if let Some(Modal::Login(LoginModal::OAuthBrowser {
                        url, redirect_uri, ..
                    })) = self.ui.modal.as_mut()
                    {
                        *url = next_url;
                        *redirect_uri = next_redirect;
                    }
                }
                LoginEvent::OpenAiDeviceStarted {
                    verification_url,
                    user_code,
                } => {
                    if let Some(Modal::Login(LoginModal::OAuthDevice {
                        verification_uri,
                        user_code: uc,
                        status,
                        ..
                    })) = self.ui.modal.as_mut()
                    {
                        *verification_uri = verification_url;
                        *uc = user_code;
                        *status =
                            "Enter the code in your browser - waiting for authorization.".into();
                    }
                }
                LoginEvent::GitHubDeviceStarted {
                    verification_uri,
                    user_code,
                } => {
                    if let Some(Modal::Login(LoginModal::OAuthDevice {
                        verification_uri: vu,
                        user_code: uc,
                        status,
                        ..
                    })) = self.ui.modal.as_mut()
                    {
                        *vu = verification_uri;
                        *uc = user_code;
                        *status = "Enter the code at GitHub - waiting for authorization.".into();
                    }
                }
                LoginEvent::Success(sp, preferred_model) => {
                    let pname = sp.name.clone();
                    let kind = sp.credential.kind_label();
                    match record_provider_login(&self.data_dir, sp, preferred_model) {
                        Ok(mref) => self.finish_login(&pname, kind, &mref),
                        Err(e) => {
                            self.set_toast(format!("{e:#}"), true);
                            self.ui.modal = None;
                        }
                    }
                    self.login.task = None;
                    self.login.cancel = None;
                    keep = false;
                }
                LoginEvent::Error(err) => {
                    match self.ui.modal.as_mut() {
                        Some(Modal::Login(LoginModal::OAuthDevice { error, .. })) => {
                            *error = Some(err);
                        }
                        Some(Modal::Login(LoginModal::OAuthBrowser { error, busy, .. })) => {
                            *busy = false;
                            *error = Some(err);
                        }
                        Some(Modal::Login(LoginModal::OAuthCodePaste { error, busy, .. })) => {
                            *busy = false;
                            *error = Some(err);
                        }
                        _ => self.set_toast(err, true),
                    }
                    self.login.task = None;
                    self.login.cancel = None;
                    keep = false;
                }
            }
        }
        if keep && alive {
            self.login.rx = rx;
        }
        true
    }
}

pub(super) fn openai_url_preview(redirect_uri: &str, challenge: &str, state: &str) -> String {
    jade_agent::auth::openai_authorize_url(
        redirect_uri,
        &jade_agent::auth::Pkce {
            verifier: String::new(),
            challenge: challenge.to_string(),
        },
        state,
    )
}
