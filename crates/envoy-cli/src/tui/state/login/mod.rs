//! Interactive provider login and OAuth/device flows.
use super::*;

mod keys;
mod methods;

impl App {
pub(in crate::tui) fn open_login_modal(&mut self) {
        self.cancel_login_bg();
        self.modal = Some(Modal::Login(LoginModal::SelectProvider {
            filter: String::new(),
            selected: 0,
        }));
    }

pub(in crate::tui) fn filtered_login_providers(&self, filter: &str) -> Vec<(usize, &'static crate::auth::BuiltinProvider)> {
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
        if let Some(tx) = self.login_cancel.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.login_task.take() {
            h.abort();
        }
        self.login_rx = None;
    }

pub(in crate::tui) fn drain_login_events(&mut self) {
        let Some(mut rx) = self.login_rx.take() else {
            return;
        };
        let mut keep = true;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                LoginEvent::OpenAiDeviceStarted {
                    verification_url,
                    user_code,
                } => {
                    if let Some(Modal::Login(LoginModal::OAuthDevice {
                        verification_uri,
                        user_code: uc,
                        status,
                        ..
                    })) = self.modal.as_mut()
                    {
                        *verification_uri = verification_url;
                        *uc = user_code;
                        *status = "Enter the code in your browser — waiting for authorization…"
                            .into();
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
                    })) = self.modal.as_mut()
                    {
                        *vu = verification_uri;
                        *uc = user_code;
                        *status = "Enter the code at GitHub — waiting for authorization…".into();
                    }
                }
                LoginEvent::Success(sp, preferred_model) => {
                    let pname = sp.name.clone();
                    let kind = sp.credential.kind_label();
                    match record_provider_login(&self.data_dir, sp, preferred_model) {
                        Ok(mref) => {
                            self.mode = InputMode::Agent;
                            self.modal = None;
                            self.set_toast(
                                format!("connected {pname} ({kind}) · {}", mref.display()),
                                false,
                            );
                            self.push_notice(
                                format!("Connected to {pname} ({kind}). Active model: {}", mref.display()),
                                false,
                            );
                        }
                        Err(e) => {
                            self.set_toast(format!("{e:#}"), true);
                            self.modal = None;
                        }
                    }
                    self.login_task = None;
                    self.login_cancel = None;
                    keep = false;
                }
                LoginEvent::Error(err) => {
                    match self.modal.as_mut() {
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
                    self.login_task = None;
                    self.login_cancel = None;
                    keep = false;
                }
            }
        }
        if keep {
            self.login_rx = Some(rx);
        }
    }

}

pub(super) fn openai_url_preview(redirect_uri: &str, challenge: &str, state: &str) -> String {
    crate::auth::openai_authorize_url(
        redirect_uri,
        &crate::auth::Pkce {
            verifier: String::new(),
            challenge: challenge.to_string(),
        },
        state,
    )
}
