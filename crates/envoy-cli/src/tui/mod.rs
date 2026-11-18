//! `envoy`: terminal console.
//!
//! The screen is the transcript and the prompt. One status line carries the
//! only chrome: mode, workspace, model, and context-window fill. Jobs sit in
//! a sidebar that stays hidden until asked for. Reasoning and tool output
//! are collapsed by default; `ctrl+o` expands them.
//! - Turns live in the daemon; the console attaches to an event stream and
//!   survives a lost connection instead of holding the turn in-process.
//! - `/` opens the filterable command menu (`/login`, `/models`, `/cd` with
//!   directory autocomplete, `/machine`, `/sessions`, `/new`, `/mode`, etc.).
//! - `/login` provides an interactive wizard for API Keys, OAuth (ChatGPT
//!   Plus/Pro PKCE & Device Flow, Anthropic Claude OAuth, GitHub Copilot
//!   Device Flow), and custom OpenAI/Anthropic-compatible endpoints.

mod commands;
mod decrypt;
mod diff;
mod highlight;
mod links;
mod markdown;
mod mouse;
mod state;
mod text;
mod theme;
mod types;
mod view;

pub(crate) use commands::{SlashCommand, SLASH_COMMANDS};
pub(crate) use decrypt::*;
pub(crate) use markdown::*;
pub(crate) use text::*;
pub(crate) use theme::{Theme, THEMES};
pub(crate) use types::*;

#[allow(unused_imports)] // re-exported for the state and view modules
pub(crate) use crate::auth::{
    anthropic_authorize_url, available_models, bind_openai_callback_listener, effective_providers,
    exchange_anthropic_code, exchange_openai_code, extract_query_param, generate_pkce,
    is_retired_model, load_prefs, open_browser, poll_github_copilot_device,
    poll_openai_device_auth, random_state, record_provider_login, remove_provider,
    resolve_active_provider, save_prefs, set_active_model, slugify_provider_id,
    start_github_copilot_device, start_openai_device_auth, wait_openai_browser_callback,
    AuthMethodKind, AvailableModel, Credential, ModelRef, ProviderProtocol, StoredProvider,
    BUILTIN_PROVIDERS,
};
pub(crate) use crate::daemon;
pub(crate) use crate::agent::checkpoint::CheckpointSummary;
pub(crate) use crate::agent::MonitorInfo;
pub(crate) use crate::llm::{AgentEvent, AskOption};
pub(crate) use crate::permissions::{
    is_workspace_trusted, set_workspace_trusted, PermissionEngine, PermissionMode, RuleScope,
};
pub(crate) use crate::runtime::{self, AgentReq, AgentResp, SendMode};
pub(crate) use crate::session::{
    delete_session, exact_session_for_cwd, latest_session_for_cwd, list_sessions, load_session,
    recent_workspaces, save_session, CardStatus, ChatMessage, ChatRole, Session, SessionSummary,
    TranscriptItem,
};
pub(crate) use anyhow::{bail, Result};
pub(crate) use envoy_proto::{
    Job, JobId, JobState, LogRecord, SubmitRequest, TaskSpec, DEFAULT_GROUP,
};
pub(crate) use envoy_service::{
    default_control_socket, dialer_for, AgentDialer, ConduitClient, Remote, Status,
};
pub(crate) use ratatui::backend::CrosstermBackend;
pub(crate) use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
pub(crate) use ratatui::crossterm::execute;
pub(crate) use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
    EnterAlternateScreen, LeaveAlternateScreen,
};
pub(crate) use ratatui::layout::{Constraint, Direction, Layout, Rect};
pub(crate) use ratatui::style::{Modifier, Style};
pub(crate) use ratatui::text::{Line, Span};
pub(crate) use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph,
};
pub(crate) use ratatui::{Frame, Terminal};
pub(crate) use std::collections::{HashMap, HashSet, VecDeque};
pub(crate) use std::io::{stdout, IsTerminal, Stdout};
pub(crate) use std::path::PathBuf;
pub(crate) use std::time::{Duration, Instant};
pub(crate) use tokio::sync::{mpsc, oneshot, watch};
pub(crate) use tokio::task::JoinHandle;

pub(crate) const LEADER_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) type AnyDialer = Box<dyn AgentDialer>;

/// True when the active provider has no usable model id, or still lists a
/// retired DeepSeek id. Those ids 400 on `/chat/completions`.
fn model_list_needs_fetch(data_dir: &std::path::Path) -> bool {
    let Some((prov, model)) = resolve_active_provider(data_dir) else {
        return false;
    };
    model.model_id.is_empty()
        || prov.custom_models.is_empty()
        || prov
            .custom_models
            .iter()
            .any(|id| is_retired_model(&prov.id, id))
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    data_dir: PathBuf,
    target: String,
    conduit: Option<PathBuf>,
    cwd: PathBuf,
    continue_last: bool,
    session_id: Option<String>,
    pick_workspace: bool,
    new_session: bool,
) -> Result<()> {
    if !stdout().is_terminal() {
        bail!(
            "`envoy` TUI requires a terminal stdout; use `envoy -p \"...\"` or `envoy ask \"...\"` for non-interactive runs"
        );
    }
    let mut remote: Remote<AnyDialer> =
        Remote::new(dialer_for(&data_dir, &target, conduit.clone()));

    let mut app = App::new(
        data_dir.clone(),
        target.clone(),
        conduit.clone(),
        cwd.clone(),
    );
    if pick_workspace {
        app.open_welcome();
    }
    if let Some(id) = session_id {
        if let Ok(loaded) = load_session(&data_dir, &id) {
            app.load_existing_session(loaded);
        }
    } else if !pick_workspace && !new_session {
        let loaded = if continue_last {
            latest_session_for_cwd(&data_dir, &cwd)
        } else {
            exact_session_for_cwd(&data_dir, &cwd)
        };
        if let Some(loaded) = loaded {
            app.load_existing_session(loaded);
        }
    }

    // Enter alternate screen immediately before daemon/conduit startup so frame 1 is instant.
    let mut tui = Tui::enter()?;
    app.agent_retry_at = Instant::now() + Duration::from_millis(800);
    app.last_refresh = Instant::now();
    {
        let app_ref = &mut app;
        let _ = execute!(stdout(), BeginSynchronizedUpdate);
        tui.terminal.draw(|frame| view::draw(frame, app_ref))?;
        let _ = execute!(stdout(), EndSynchronizedUpdate);
    }

    let boot_data = data_dir.clone();
    let boot_target = target.clone();
    let boot_conduit = conduit.clone();
    let boot_tx = app.notice_tx.clone();
    tokio::spawn(async move {
        if boot_target == "local" || boot_target.is_empty() {
            match daemon::ensure(&boot_data).await {
                Ok(()) => {
                    let _ = boot_tx.send(UiNotice::DaemonReady);
                }
                Err(e) => {
                    let _ = boot_tx.send(UiNotice::Toast(format!("daemon: {e:#}"), true));
                }
            }
        } else {
            let _ = boot_tx.send(UiNotice::DaemonReady);
        }
        let client = ConduitClient::new(boot_conduit.unwrap_or_else(default_control_socket));
        if let Ok(rx) = client.watch().await {
            let _ = boot_tx.send(UiNotice::ConduitWatch(rx));
        }
    });

    // An empty list, or a stored DeepSeek id that the API retired, would 400
    // on the next turn. Fetch in the background before the user sends anything.
    if model_list_needs_fetch(&app.data_dir) {
        app.spawn_model_refresh();
    }

    loop {
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    app.selection = None;
                    let action = app.handle_key(key);
                    match action {
                        Action::Quit => {
                            app.persist_session();
                            return Ok(());
                        }
                        Action::None => {}
                        Action::SubmitShell(req) => app.submit_shell(&remote, req).await,
                        Action::SubmitAgent(prompt) => app.submit_agent(prompt),
                        Action::SubmitAgentFollowup(prompt) => app.queue_followup(prompt),
                        Action::Cancel => app.cancel_active(&remote).await,
                        Action::CancelJob(id) => app.cancel_job(&remote, id).await,
                        Action::Refresh => app.refresh(&remote).await,
                        Action::Switch(target) => {
                            app.switch_to(target);
                            if app.target == "local" {
                                daemon::ensure(&app.data_dir).await?;
                            }
                            remote = Remote::new(dialer_for(
                                &app.data_dir,
                                &app.target,
                                app.conduit.clone(),
                            ));
                            app.refresh(&remote).await;
                        }
                        Action::AttachJob(job) => app.attach_job_card(&remote, job).await,
                        Action::ToggleMouseCapture => {
                            let next = !tui.mouse_capture;
                            tui.set_mouse_capture(next);
                            app.mouse_capture = next;
                            app.set_toast(
                                if next {
                                    "mouse capture: on (drag to OSC52 copy, click links)"
                                } else {
                                    "mouse capture: off (native terminal selection)"
                                },
                                false,
                            );
                        }
                        Action::SetMouseCapture(on) => {
                            tui.set_mouse_capture(on);
                            app.mouse_capture = on;
                        }
                    }
                }
                Event::Paste(text) => {
                    app.selection = None;
                    app.handle_paste(&text);
                }
                Event::Mouse(mouse) => {
                    if let Some(Action::AttachJob(job)) = app.handle_mouse(mouse) {
                        app.attach_job_card(&remote, job).await;
                    }
                }
                _ => {}
            }
        }

        app.drain_agent_events();
        app.drain_ui_notices();
        app.drain_login_events();
        app.drain_model_events();

        if app.welcome.is_none()
            && app.agent_stream.is_none()
            && Instant::now() >= app.agent_retry_at
        {
            app.spawn_agent_attach();
        }

        if app.last_refresh.elapsed() >= app.interval {
            app.refresh(&remote).await;
        }
        app.poll_shell_cards(&remote).await;

        let app_ref = &mut app;
        let _ = execute!(stdout(), BeginSynchronizedUpdate);
        tui.terminal.draw(|frame| view::draw(frame, app_ref))?;
        let _ = execute!(stdout(), EndSynchronizedUpdate);
        let frame_ms = if app.agent_running || !app.jobs.is_empty() {
            25
        } else {
            33
        };
        tokio::time::sleep(Duration::from_millis(frame_ms)).await;
    }
}

// --------------------------------------------------------------------- themes

// ----------------------------------------------------------------------- enums

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Input,
    Transcript,
    Sidebar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputMode {
    Agent,
    Shell,
}

impl InputMode {
    fn label(self) -> &'static str {
        match self {
            InputMode::Agent => "Agent",
            InputMode::Shell => "Shell",
        }
    }
}

pub(crate) enum Action {
    None,
    Quit,
    Refresh,
    SubmitShell(SubmitRequest),
    SubmitAgent(String),
    SubmitAgentFollowup(String),
    Cancel,
    CancelJob(JobId),
    Switch(String),
    AttachJob(Job),
    ToggleMouseCapture,
    /// Set mouse capture to an absolute value. The event loop applies it to
    /// the terminal; `run_command` only records the choice.
    SetMouseCapture(bool),
}

#[derive(Clone)]
pub(crate) enum Command {
    FocusInput,
    OpenLogin,
    OpenModels,
    OpenSessions,
    OpenRewind,
    NewSession,
    ToggleMode,
    OpenModePicker,
    OpenPermissionPicker,
    OpenThemePicker,
    OpenTrustPicker,
    OpenSidebarPicker,
    OpenMousePicker,
    OpenBudgetPicker,
    SetMode(InputMode),
    SetPermission(PermissionMode),
    SetTheme(usize),
    SetTrust(bool),
    SetSidebar(bool),
    ResetSidebar,
    SetMouse(bool),
    SetBudget(&'static str),
    ToggleExpand,
    Cancel,
    ToggleSidebar,
    NextTheme,
    Refresh,
    Switch(String),
    Quit,
}

pub(crate) struct Picker {
    title: String,
    query: String,
    items: Vec<(String, Command)>,
    selected: usize,
}

// ------------------------------------------------------------------ /login modal

pub(crate) enum LoginEvent {
    Success(StoredProvider, Option<String>),
    Error(String),
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
    filter: String,
    selected: usize,
    models: Vec<AvailableModel>,
    fetching: bool,
}

pub(crate) struct SessionsModal {
    sessions: Vec<SessionSummary>,
    selected: usize,
}

pub(crate) struct RewindModal {
    checkpoints: Vec<CheckpointSummary>,
    selected: usize,
}

pub(crate) enum Modal {
    Picker(Picker),
    Login(LoginModal),
    Models(ModelsModal),
    Sessions(SessionsModal),
    Rewind(RewindModal),
}

// ------------------------------------------------------------------ live shell watch

pub(crate) struct ShellWatch {
    item_id: u64,
    job_id: JobId,
    cursor: u64,
    partial_out: Vec<u8>,
    partial_err: Vec<u8>,
    started: Instant,
}

// ------------------------------------------------------------------ App

pub(crate) enum UiNotice {
    /// Background daemon ensure succeeded; console can attach and refresh immediately.
    DaemonReady,
    /// Background conduit watch receiver connected.
    ConduitWatch(watch::Receiver<Option<Status>>),
    /// A send failed because the daemon went away; put the items back in the
    /// outbox so the next attach can deliver them.
    Requeue(Vec<(String, SendMode)>),
    /// The daemon returned a recalled follow-up (or had none).
    Recalled(Option<String>),
    /// Checkpoints fetched from the daemon for `/rewind` selection.
    Checkpoints(Vec<CheckpointSummary>),
    /// Status toast emitted from an async daemon RPC.
    Toast(String, bool),
    /// Transcript notice emitted from an async command.
    Notice(String, bool),
}

pub(crate) struct PendingAsk {
    id: u64,
    question: String,
    options: Vec<AskOption>,
    recommended: usize,
    selected: usize,
    deadline: Instant,
}

pub(crate) struct App {
    data_dir: PathBuf,
    conduit: Option<PathBuf>,
    target: String,
    status: Option<Status>,
    watch: Option<watch::Receiver<Option<Status>>>,
    jobs: Vec<Job>,
    job_selected: usize,

    session: Session,
    focus: Focus,
    mode: InputMode,
    perm_mode: PermissionMode,
    show_sidebar: bool,
    follow: bool,
    transcript_scroll: u16,
    transcript_height: u16,
    transcript_max_scroll: u16,
    selected_card: Option<usize>,

    input: String,
    input_cursor: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    menu_selected: usize,
    at_dismissed_at: Option<usize>,

    modal: Option<Modal>,
    theme: usize,
    leader_at: Option<Instant>,
    toast: Option<(String, bool, Instant)>,
    connected: bool,
    interval: Duration,
    last_refresh: Instant,

    // Active agent turn
    agent_running: bool,
    agent_stream: Option<mpsc::UnboundedReceiver<AgentResp>>,
    agent_retry_at: Instant,
    agent_offline: bool,
    /// True after the daemon has sent the attach snapshot. Until then sends
    /// are held in the outbox so they cannot race the session creation.
    agent_ready: bool,
    pending_ask: Option<PendingAsk>,
    queued: VecDeque<String>,
    outbox: VecDeque<(String, SendMode)>,
    notice_tx: mpsc::UnboundedSender<UiNotice>,
    notice_rx: mpsc::UnboundedReceiver<UiNotice>,
    active_assistant_id: Option<u64>,
    active_tool_items: HashMap<String, u64>,
    active_monitors: Vec<MonitorInfo>,

    // Active shell cards being tailed
    shell_watches: Vec<ShellWatch>,

    // Active /login background flow
    login_rx: Option<mpsc::UnboundedReceiver<LoginEvent>>,
    login_task: Option<JoinHandle<()>>,
    login_cancel: Option<oneshot::Sender<()>>,

    // Remote /models fetch
    models_rx: Option<oneshot::Receiver<(String, Vec<crate::agent::context::ListedModel>)>>,
    welcome: Option<Welcome>,

    // Universal right inspector, clickable links, and mouse selection
    inspector: InspectorState,
    workspace_index: links::WorkspaceIndex,
    hit_regions: Vec<HitRegion>,
    transcript_area: Rect,
    expanded_explore_groups: HashSet<u64>,
    mouse_capture: bool,
    selection: Option<MouseSelection>,
    drag_start: Option<GridPos>,
    screen_lines: Vec<Vec<String>>,
    transcript_cache: HashMap<(u64, usize, usize), (usize, Vec<LinkedLine>)>,
}

pub(crate) struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    enhanced_keys: bool,
    mouse_capture: bool,
}

fn terminal_supports_csi_u() -> bool {
    if std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("WEZTERM_EXECUTABLE").is_some()
        || std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
    {
        return true;
    }
    if matches!(
        std::env::var("TERM_PROGRAM").as_deref(),
        Ok("iTerm.app" | "WezTerm" | "ghostty" | "vscode" | "WarpTerminal" | "rio")
    ) {
        return true;
    }
    matches!(
        std::env::var("TERM").as_deref(),
        Ok("xterm-kitty" | "xterm-ghostty" | "alacritty" | "foot" | "foot-extra")
    )
}

impl Tui {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(
            stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        let enhanced_keys = terminal_supports_csi_u();
        if enhanced_keys {
            let _ = execute!(
                stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            );
        }
        let terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        Ok(Self {
            terminal,
            enhanced_keys,
            mouse_capture: true,
        })
    }

    fn set_mouse_capture(&mut self, enabled: bool) {
        self.mouse_capture = enabled;
        if enabled {
            let _ = execute!(self.terminal.backend_mut(), EnableMouseCapture);
        } else {
            let _ = execute!(self.terminal.backend_mut(), DisableMouseCapture);
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if self.enhanced_keys {
            let _ = execute!(self.terminal.backend_mut(), PopKeyboardEnhancementFlags);
        }
        if self.mouse_capture {
            let _ = execute!(self.terminal.backend_mut(), DisableMouseCapture);
        }
        let _ = execute!(self.terminal.backend_mut(), DisableBracketedPaste);
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_inspector;
#[cfg(test)]
mod tests_ux;
#[cfg(test)]
mod tests_ux_fold;
