//! Modal state types for the command palette, `/login`, `/models`, `/sessions`, and `/rewind`.

use super::Command;
use jade_agent::agent::checkpoint::CheckpointSummary;
use jade_agent::auth::{AvailableModel, StoredProvider};
use jade_agent::session::SessionSummary;

pub(crate) struct Picker {
    pub(crate) title: String,
    pub(crate) query: String,
    pub(crate) items: Vec<(String, Command)>,
    pub(crate) selected: usize,
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum LoginEvent {
    Success(StoredProvider, Option<String>),
    Error(String),
    OpenAiBrowserBound {
        url: String,
        redirect_uri: String,
    },
    OpenAiDeviceStarted {
        verification_url: String,
        user_code: String,
    },
    GitHubDeviceStarted {
        verification_uri: String,
        user_code: String,
    },
}

pub(crate) enum LoginModal {
    SelectProvider {
        filter: String,
        selected: usize,
    },
    SelectMethod {
        provider_idx: usize,
        selected: usize,
    },
    ApiKeyForm {
        provider_idx: usize,
        key: String,
        base_url: String,
        model: String,
        focus_field: usize,
        reveal: bool,
        error: Option<String>,
    },
    CustomForm {
        protocol_idx: usize,
        name: String,
        base_url: String,
        key: String,
        model: String,
        focus_field: usize,
        error: Option<String>,
    },
    OAuthBrowser {
        provider_name: String,
        url: String,
        redirect_uri: String,
        verifier: String,
        pasted: String,
        status: String,
        busy: bool,
        error: Option<String>,
    },
    OAuthCodePaste {
        provider_name: String,
        url: String,
        verifier: String,
        state: String,
        console: bool,
        code_input: String,
        busy: bool,
        error: Option<String>,
    },
    OAuthDevice {
        provider_name: String,
        verification_uri: String,
        user_code: String,
        status: String,
        error: Option<String>,
    },
}

pub(crate) struct ModelsModal {
    pub(crate) filter: String,
    pub(crate) selected: usize,
    pub(crate) models: Vec<AvailableModel>,
    pub(crate) fetching: bool,
}

pub(crate) struct SessionsModal {
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) selected: usize,
}

pub(crate) struct RewindModal {
    pub(crate) checkpoints: Vec<CheckpointSummary>,
    pub(crate) selected: usize,
}

pub(crate) enum Modal {
    Picker(Picker),
    Login(LoginModal),
    Models(ModelsModal),
    Sessions(SessionsModal),
    Rewind(RewindModal),
}
