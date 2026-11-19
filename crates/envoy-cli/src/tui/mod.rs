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
mod edit_file;
mod highlight;
mod links;
mod markdown;
mod markdown_table;
mod modals;
mod mouse;
mod state;
mod terminal;
mod text;
mod theme;
mod types;
mod view;

pub(crate) use commands::{SlashCommand, SLASH_COMMANDS};
pub(crate) use decrypt::*;
pub(crate) use markdown::*;
pub(crate) use modals::*;
pub(crate) use terminal::Tui;
pub(crate) use text::*;
pub(crate) use theme::{Theme, THEMES};
pub(crate) use types::*;

pub(crate) use anyhow::{bail, Result};
pub(crate) use envoy_agent::agent::checkpoint::CheckpointSummary;
pub(crate) use envoy_agent::agent::MonitorInfo;
pub(crate) use envoy_agent::agent::{AgentEvent, AskOption};
#[allow(unused_imports)] // re-exported for the state and view modules
pub(crate) use envoy_agent::auth::{
    anthropic_authorize_url, available_models, bind_openai_callback_listener, effective_providers,
    exchange_anthropic_code, exchange_openai_code, extract_query_param, generate_pkce,
    is_retired_model, load_prefs, open_browser, poll_github_copilot_device,
    poll_openai_device_auth, random_state, record_provider_login, remove_provider,
    resolve_active_provider, save_prefs, set_active_model, slugify_provider_id,
    start_github_copilot_device, start_openai_device_auth, wait_openai_browser_callback,
    AuthMethodKind, AvailableModel, Credential, ModelRef, ProviderProtocol, StoredProvider,
    BUILTIN_PROVIDERS,
};
pub(crate) use envoy_agent::daemon;
pub(crate) use envoy_agent::permissions::{
    is_workspace_trusted, set_workspace_trusted, PermissionEngine, PermissionMode, RuleScope,
};
pub(crate) use envoy_agent::runtime::{self, AgentReq, AgentResp, SendMode, SeqEvent};
pub(crate) use envoy_agent::session::{
    delete_session, exact_session_for_cwd, latest_session_for_cwd, list_sessions, load_session,
    recent_workspaces, save_session, CardStatus, ChatMessage, ChatRole, Session, TranscriptItem,
};
pub(crate) use envoy_proto::{
    Job, JobId, JobState, LogRecord, SubmitRequest, TaskSpec, DEFAULT_GROUP,
};
pub(crate) use envoy_service::{
    default_control_socket, dialer_for, AgentDialer, ConduitClient, Remote, Status,
};
pub(crate) use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
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
pub(crate) use ratatui::Frame;
#[cfg(test)]
pub(crate) use ratatui::Terminal;
pub(crate) use std::collections::{HashMap, HashSet, VecDeque};
pub(crate) use std::io::{stdout, IsTerminal};
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
                        Action::EditFile(path) => {
                            if let Err(err) = edit_file::run(&mut tui, &path) {
                                app.set_toast(err, true);
                            }
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
    /// Suspend the console and open this file in `$VISUAL` or `$EDITOR`.
    EditFile(PathBuf),
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

#[derive(Default)]
pub(crate) struct SnapshotAccum {
    pub(crate) running: bool,
    pub(crate) events: Vec<SeqEvent>,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) saw_messages: bool,
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
    /// Last daemon incarnation observed on `SnapshotStart`. `0` means none.
    daemon_incarnation: u64,
    snapshot: Option<SnapshotAccum>,
    /// Set while applying a replay that included `SessionReset`, so token
    /// totals already stored on the session are not added a second time.
    skip_replay_tokens: bool,
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
    models_rx: Option<oneshot::Receiver<(String, Vec<envoy_agent::agent::context::ListedModel>)>>,
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

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_delegate;
#[cfg(test)]
mod tests_effort;
#[cfg(test)]
mod tests_inspector;
#[cfg(test)]
mod tests_ux;
#[cfg(test)]
mod tests_ux_fold;
#[cfg(test)]
mod tests_ux_layout;
#[cfg(test)]
mod tests_welcome;
#[cfg(test)]
mod tests_worktree;
