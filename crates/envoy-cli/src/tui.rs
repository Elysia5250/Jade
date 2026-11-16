//! `envoy`: opencode-style terminal console for Envoy.
//!
//! Layout & interaction mirror `opencode`:
//! - Opens directly in the current directory with the prompt editor focused.
//! - Main pane is an inline transcript stream of user prompts, streaming
//!   assistant replies (with collapsible reasoning), and collapsible tool/shell
//!   execution cards (command/tool, duration, exit code, live stdout/stderr).
//! - Right sidebar shows session context, active model/auth, target machine,
//!   and a live index of detached daemon jobs.
//! - `/` opens the filterable command menu (`/login`, `/models`, `/cd` with
//!   directory autocomplete, `/machine`, `/sessions`, `/new`, `/mode`, etc.).
//! - `/login` provides an interactive wizard for API Keys, OAuth (ChatGPT
//!   Plus/Pro PKCE & Device Flow, Anthropic Claude OAuth, GitHub Copilot
//!   Device Flow), and custom OpenAI/Anthropic-compatible endpoints.

use crate::auth::{
    anthropic_authorize_url, available_models, bind_openai_callback_listener,
    effective_providers, exchange_anthropic_code, exchange_openai_code, extract_query_param,
    fetch_openai_models, generate_pkce, load_prefs, open_browser,
    poll_github_copilot_device, poll_openai_device_auth, random_state, record_provider_login,
    remove_provider, resolve_active_provider, save_prefs, set_active_model, slugify_provider_id,
    start_github_copilot_device, start_openai_device_auth, wait_openai_browser_callback,
    AuthMethodKind, AvailableModel, Credential, ModelRef, ProviderProtocol, StoredProvider,
    BUILTIN_PROVIDERS,
};
use crate::daemon;
use crate::llm::{run_agent_turn, AgentEvent, AskBroker, AskOption, TurnConfig};
use crate::session::{
    delete_session, latest_session_for_cwd, list_sessions, load_session, save_session, CardStatus,
    ChatMessage, ChatRole, Session, SessionSummary, TranscriptItem,
};
use anyhow::{bail, Result};
use envoy_proto::{Job, JobId, JobState, LogRecord, SubmitRequest, TaskSpec, DEFAULT_GROUP};
use envoy_service::{default_control_socket, dialer_for, AgentDialer, ConduitClient, Remote, Status};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::io::{stdout, IsTerminal, Stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use unicode_width::UnicodeWidthChar;

const LEADER_TIMEOUT: Duration = Duration::from_secs(2);

type AnyDialer = Box<dyn AgentDialer>;

pub async fn run(
    data_dir: PathBuf,
    target: String,
    conduit: Option<PathBuf>,
    cwd: PathBuf,
    continue_last: bool,
    session_id: Option<String>,
) -> Result<()> {
    if !stdout().is_terminal() {
        bail!("`envoy` opens a console; use a subcommand for scripts (envoy jobs)");
    }
    if target == "local" || target.is_empty() {
        daemon::ensure(&data_dir).await?;
    }
    let mut remote: Remote<AnyDialer> =
        Remote::new(dialer_for(&data_dir, &target, conduit.clone()));

    let mut app = App::new(data_dir.clone(), target.clone(), conduit.clone(), cwd.clone());
    if let Some(id) = session_id {
        if let Ok(loaded) = load_session(&data_dir, &id) {
            app.load_existing_session(loaded);
        }
    } else if continue_last {
        if let Some(loaded) = latest_session_for_cwd(&data_dir, &cwd) {
            app.load_existing_session(loaded);
        }
    }

    let client = ConduitClient::new(conduit.clone().unwrap_or_else(default_control_socket));
    if let Ok(rx) = client.watch().await {
        app.watch = Some(rx);
    }

    let mut tui = Tui::enter()?;
    app.refresh(&remote).await;

    loop {
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match app.handle_key(key) {
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
                }
            }
        }

        app.drain_agent_events();
        app.drain_login_events();
        app.drain_model_events();

        if app.last_refresh.elapsed() >= app.interval {
            app.refresh(&remote).await;
        }
        app.poll_shell_cards(&remote).await;

        let app_ref = &mut app;
        tui.terminal.draw(|frame| draw(frame, app_ref))?;
        tokio::time::sleep(Duration::from_millis(33)).await;
    }
}

// --------------------------------------------------------------------- themes

#[derive(Clone, Copy)]
struct Theme {
    accent: Color,
    secondary: Color,
    dim: Color,
    text: Color,
    ok: Color,
    warn: Color,
    bad: Color,
    border: Color,
    selection: Color,
    surface: Color,
    user_bg: Color,
    /// Full-screen background. Explicit so a transparent terminal or a
    /// wallpaper cannot leak through as random colour bands.
    bg: Color,
    /// Sidebar/editor background, one step off `bg`.
    panel: Color,
}

const THEMES: [(&str, Theme); 3] = [
    (
        "opencode",
        Theme {
            accent: Color::Rgb(0x9d, 0x7c, 0xd8),
            secondary: Color::Rgb(0x7a, 0xa2, 0xf7),
            dim: Color::Rgb(0x80, 0x84, 0x92),
            text: Color::Rgb(0xd6, 0xd8, 0xe0),
            ok: Color::Rgb(0x7f, 0xd8, 0x8f),
            warn: Color::Rgb(0xf2, 0xc9, 0x4c),
            bad: Color::Rgb(0xf2, 0x6b, 0x6b),
            border: Color::Rgb(0x45, 0x47, 0x56),
            selection: Color::Rgb(0x2d, 0x2a, 0x3e),
            surface: Color::Rgb(0x1f, 0x20, 0x29),
            user_bg: Color::Rgb(0x25, 0x24, 0x32),
            bg: Color::Rgb(0x16, 0x17, 0x1d),
            panel: Color::Rgb(0x1b, 0x1c, 0x24),
        },
    ),
    (
        "tokyonight",
        Theme {
            accent: Color::Rgb(0x7a, 0xa2, 0xf7),
            secondary: Color::Rgb(0xbb, 0x9a, 0xf7),
            dim: Color::Rgb(0x73, 0x7a, 0xa2),
            text: Color::Rgb(0xc0, 0xca, 0xf5),
            ok: Color::Rgb(0x9e, 0xce, 0x6a),
            warn: Color::Rgb(0xe0, 0xaf, 0x68),
            bad: Color::Rgb(0xf7, 0x76, 0x8e),
            border: Color::Rgb(0x3b, 0x42, 0x61),
            selection: Color::Rgb(0x28, 0x34, 0x57),
            surface: Color::Rgb(0x1e, 0x20, 0x30),
            user_bg: Color::Rgb(0x24, 0x28, 0x3b),
            bg: Color::Rgb(0x1a, 0x1b, 0x26),
            panel: Color::Rgb(0x1f, 0x23, 0x35),
        },
    ),
    (
        "light",
        Theme {
            accent: Color::Rgb(0x3b, 0x5f, 0xc4),
            secondary: Color::Rgb(0x6b, 0x46, 0xc1),
            dim: Color::Rgb(0x66, 0x6a, 0x75),
            text: Color::Rgb(0x24, 0x26, 0x2d),
            ok: Color::Rgb(0x2f, 0x7d, 0x32),
            warn: Color::Rgb(0x9a, 0x6b, 0x00),
            bad: Color::Rgb(0xb3, 0x26, 0x1e),
            border: Color::Rgb(0xb5, 0xb8, 0xc2),
            selection: Color::Rgb(0xe4, 0xe7, 0xf2),
            surface: Color::Rgb(0xf3, 0xf4, 0xf7),
            user_bg: Color::Rgb(0xea, 0xec, 0xf4),
            bg: Color::Rgb(0xff, 0xff, 0xff),
            panel: Color::Rgb(0xf5, 0xf6, 0xf9),
        },
    ),
];

// ----------------------------------------------------------------------- enums

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Input,
    Transcript,
    Sidebar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
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

enum Action {
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
}

#[derive(Clone)]
enum Command {
    FocusInput,
    OpenLogin,
    OpenModels,
    OpenSessions,
    NewSession,
    ToggleMode,
    ToggleExpand,
    Cancel,
    ToggleSidebar,
    NextTheme,
    Refresh,
    Switch(String),
    Quit,
}

struct Picker {
    title: String,
    query: String,
    items: Vec<(String, Command)>,
    selected: usize,
}

struct SlashCommand {
    name: &'static str,
    aliases: &'static [&'static str],
    args: &'static str,
    about: &'static str,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "login",
        aliases: &["connect"],
        args: "",
        about: "configure API Key or OAuth subscription",
    },
    SlashCommand {
        name: "models",
        aliases: &["model"],
        args: "[id]",
        about: "switch active LLM model",
    },
    SlashCommand {
        name: "logout",
        aliases: &[],
        args: "[provider]",
        about: "remove a saved provider credential",
    },
    SlashCommand {
        name: "mode",
        aliases: &[],
        args: "",
        about: "switch between Agent and Shell mode",
    },
    SlashCommand {
        name: "cd",
        aliases: &[],
        args: "<dir>",
        about: "change working directory (with path completion)",
    },
    SlashCommand {
        name: "machine",
        aliases: &["machines"],
        args: "[name]",
        about: "switch target machine (local or conduit peer)",
    },
    SlashCommand {
        name: "new",
        aliases: &[],
        args: "",
        about: "start a new session",
    },
    SlashCommand {
        name: "sessions",
        aliases: &["resume"],
        args: "",
        about: "list or switch saved sessions",
    },
    SlashCommand {
        name: "expand",
        aliases: &[],
        args: "",
        about: "expand or collapse tool output cards",
    },
    SlashCommand {
        name: "sidebar",
        aliases: &[],
        args: "",
        about: "toggle the right sidebar",
    },
    SlashCommand {
        name: "cancel",
        aliases: &[],
        args: "",
        about: "cancel active turn or selected job",
    },
    SlashCommand {
        name: "clear",
        aliases: &[],
        args: "",
        about: "clear current transcript",
    },
    SlashCommand {
        name: "theme",
        aliases: &["themes"],
        args: "",
        about: "switch colour theme",
    },
    SlashCommand {
        name: "help",
        aliases: &[],
        args: "",
        about: "show key bindings & commands",
    },
    SlashCommand {
        name: "quit",
        aliases: &["exit", "q"],
        args: "",
        about: "exit envoy",
    },
];

// ------------------------------------------------------------------ /login modal

enum LoginEvent {
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

enum LoginModal {
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

struct ModelsModal {
    filter: String,
    selected: usize,
    models: Vec<AvailableModel>,
    fetching: bool,
}

struct SessionsModal {
    sessions: Vec<SessionSummary>,
    selected: usize,
}

enum Modal {
    Picker(Picker),
    Login(LoginModal),
    Models(ModelsModal),
    Sessions(SessionsModal),
}

// ------------------------------------------------------------------ live shell watch

struct ShellWatch {
    item_id: u64,
    job_id: JobId,
    cursor: u64,
    partial_out: Vec<u8>,
    partial_err: Vec<u8>,
    started: Instant,
}

// ------------------------------------------------------------------ App

struct PendingAsk {
    id: u64,
    question: String,
    options: Vec<AskOption>,
    recommended: usize,
    selected: usize,
    deadline: Instant,
}

struct App {
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

    modal: Option<Modal>,
    theme: usize,
    leader_at: Option<Instant>,
    toast: Option<(String, bool, Instant)>,
    connected: bool,
    interval: Duration,
    last_refresh: Instant,

    // Active agent turn
    agent_running: bool,
    asks: Arc<AskBroker>,
    steer: Arc<Mutex<Vec<String>>>,
    pending_ask: Option<PendingAsk>,
    queued: VecDeque<String>,
    agent_rx: Option<mpsc::UnboundedReceiver<AgentEvent>>,
    agent_task: Option<JoinHandle<()>>,
    active_assistant_id: Option<u64>,
    active_tool_items: HashMap<String, u64>,

    // Active shell cards being tailed
    shell_watches: Vec<ShellWatch>,

    // Active /login background flow
    login_rx: Option<mpsc::UnboundedReceiver<LoginEvent>>,
    login_task: Option<JoinHandle<()>>,
    login_cancel: Option<oneshot::Sender<()>>,

    // Remote /models fetch
    models_rx: Option<oneshot::Receiver<Vec<String>>>,
}

impl App {
    fn new(data_dir: PathBuf, target: String, conduit: Option<PathBuf>, cwd: PathBuf) -> Self {
        let prefs = load_prefs(&data_dir);
        let has_provider = resolve_active_provider(&data_dir).is_some();
        let mode = if has_provider {
            InputMode::Agent
        } else {
            InputMode::Shell
        };
        let session = Session::new(cwd, target.clone());
        Self {
            data_dir,
            conduit,
            target,
            status: None,
            watch: None,
            jobs: Vec::new(),
            job_selected: 0,
            session,
            focus: Focus::Input,
            mode,
            show_sidebar: prefs.show_sidebar,
            follow: true,
            transcript_scroll: 0,
            transcript_height: 0,
            transcript_max_scroll: 0,
            selected_card: None,
            input: String::new(),
            input_cursor: 0,
            history: Vec::new(),
            history_pos: None,
            menu_selected: 0,
            modal: None,
            theme: prefs.theme % THEMES.len(),
            leader_at: None,
            toast: None,
            connected: false,
            interval: Duration::from_millis(600),
            last_refresh: Instant::now() - Duration::from_secs(1),
            agent_running: false,
            asks: AskBroker::new(),
            steer: Arc::new(Mutex::new(Vec::new())),
            pending_ask: None,
            queued: VecDeque::new(),
            agent_rx: None,
            agent_task: None,
            active_assistant_id: None,
            active_tool_items: HashMap::new(),
            shell_watches: Vec::new(),
            login_rx: None,
            login_task: None,
            login_cancel: None,
            models_rx: None,
        }
    }

    fn theme(&self) -> Theme {
        THEMES[self.theme].1
    }

    fn load_existing_session(&mut self, loaded: Session) {
        self.persist_session();
        if loaded.cwd.is_dir() {
            self.session_cwd_set(loaded.cwd.clone());
        }
        self.session = loaded;
        self.selected_card = None;
        self.follow = true;
    }

    fn session_cwd_set(&mut self, cwd: PathBuf) {
        self.session.cwd = cwd;
    }

    fn persist_session(&self) {
        let _ = save_session(&self.data_dir, &self.session);
    }

    fn save_ui_prefs(&self) {
        let mut prefs = load_prefs(&self.data_dir);
        prefs.theme = self.theme;
        prefs.show_sidebar = self.show_sidebar;
        let _ = save_prefs(&self.data_dir, &prefs);
    }

    fn set_toast(&mut self, msg: impl Into<String>, err: bool) {
        self.toast = Some((msg.into(), err, Instant::now()));
    }

    fn active_toast(&self) -> Option<(&str, bool)> {
        let (msg, err, at) = self.toast.as_ref()?;
        if at.elapsed() < Duration::from_secs(6) {
            Some((msg.as_str(), *err))
        } else {
            None
        }
    }

    fn push_notice(&mut self, text: impl Into<String>, is_error: bool) {
        let id = self.session.alloc_id();
        self.session.items.push(TranscriptItem::Notice {
            id,
            text: text.into(),
            is_error,
        });
        self.session.touch();
        self.follow = true;
    }

    fn cwd_label(&self) -> String {
        collapse_home(&self.session.cwd)
    }

    fn own_device_id(&self) -> String {
        self.status
            .as_ref()
            .map(|s| s.device_id.clone())
            .unwrap_or_default()
    }

    async fn refresh(&mut self, remote: &Remote<AnyDialer>) {
        self.last_refresh = Instant::now();
        if let Some(watch) = &self.watch {
            if let Some(status) = watch.borrow().clone() {
                self.status = Some(status);
            }
        }
        match remote.jobs(&self.target).await {
            Ok(jobs) => {
                let keep = self.jobs.get(self.job_selected).map(|j| j.id);
                self.jobs = jobs;
                self.job_selected =
                    match keep.and_then(|id| self.jobs.iter().position(|j| j.id == id)) {
                        Some(pos) => pos,
                        None => self.jobs.len().saturating_sub(1),
                    };
                self.connected = true;
            }
            Err(e) => {
                self.connected = false;
                self.set_toast(format!("{e:#}"), true);
            }
        }
    }

    async fn submit_shell(&mut self, remote: &Remote<AnyDialer>, req: SubmitRequest) {
        let command = req.spec.display();
        self.session.maybe_set_title_from(&format!("$ {command}"));
        let user_id = self.session.alloc_id();
        self.session.items.push(TranscriptItem::User {
            id: user_id,
            text: format!("$ {command}"),
            mode: "Shell".into(),
            cwd: self.cwd_label(),
            ts_ms: now_ms(),
        });

        match remote.submit(&self.target, req).await {
            Ok(job) => {
                let card_id = self.session.alloc_id();
                self.session.items.push(TranscriptItem::Tool {
                    id: card_id,
                    call_id: format!("job-{}", job.id),
                    name: "shell".into(),
                    title: format!("$ {command}"),
                    subtitle: self.cwd_label(),
                    job_id: Some(job.id),
                    target: self.target.clone(),
                    output: Vec::new(),
                    status: CardStatus::Running,
                    collapsed: false,
                    duration_ms: None,
                    exit_code: None,
                    started_ms: now_ms(),
                });
                self.shell_watches.push(ShellWatch {
                    item_id: card_id,
                    job_id: job.id,
                    cursor: 0,
                    partial_out: Vec::new(),
                    partial_err: Vec::new(),
                    started: Instant::now(),
                });
                self.session.touch();
                self.persist_session();
                self.follow = true;
                self.refresh(remote).await;
            }
            Err(e) => {
                self.push_notice(format!("submit failed: {e:#}"), true);
            }
        }
    }

    async fn attach_job_card(&mut self, remote: &Remote<AnyDialer>, job: Job) {
        // If this job already has a card in the transcript, uncollapse and jump to it.
        if let Some(pos) = self.session.items.iter().position(|it| {
            matches!(it, TranscriptItem::Tool { job_id: Some(id), .. } if *id == job.id)
        }) {
            if let TranscriptItem::Tool { collapsed, .. } = &mut self.session.items[pos] {
                *collapsed = false;
            }
            self.selected_card = Some(pos);
            self.focus = Focus::Transcript;
            return;
        }
        let card_id = self.session.alloc_id();
        let status = match &job.state {
            JobState::Queued { .. } => CardStatus::Queued,
            JobState::Running { .. } => CardStatus::Running,
            JobState::Done { result, .. } if result.ok() => CardStatus::Done,
            JobState::Done { .. } => CardStatus::Error,
        };
        let exit_code = job.state.exit_code();
        self.session.items.push(TranscriptItem::Tool {
            id: card_id,
            call_id: format!("job-{}", job.id),
            name: "job".into(),
            title: format!("$ {}", job.command()),
            subtitle: job.cwd.clone(),
            job_id: Some(job.id),
            target: self.target.clone(),
            output: Vec::new(),
            status,
            collapsed: false,
            duration_ms: Some(job_elapsed(&job)),
            exit_code,
            started_ms: job.created_ms,
        });
        let mut watch = ShellWatch {
            item_id: card_id,
            job_id: job.id,
            cursor: 0,
            partial_out: Vec::new(),
            partial_err: Vec::new(),
            started: Instant::now(),
        };
        Self::poll_one_watch(remote, &self.target, &mut self.session, &mut watch).await;
        if exit_code.is_none() {
            self.shell_watches.push(watch);
        }
        self.follow = true;
    }

    async fn poll_shell_cards(&mut self, remote: &Remote<AnyDialer>) {
        if self.shell_watches.is_empty() {
            return;
        }
        let mut still_running = Vec::new();
        let mut watches = std::mem::take(&mut self.shell_watches);
        for mut w in watches.drain(..) {
            let done =
                Self::poll_one_watch(remote, &self.target, &mut self.session, &mut w).await;
            if !done {
                still_running.push(w);
            }
        }
        self.shell_watches = still_running;
    }

    async fn poll_one_watch(
        remote: &Remote<AnyDialer>,
        target: &str,
        session: &mut Session,
        w: &mut ShellWatch,
    ) -> bool {
        let mut new_lines: Vec<(String, bool)> = Vec::new();
        let res = remote
            .logs(target, w.job_id, w.cursor, false, |records: &[LogRecord]| {
                for r in records {
                    let (buf, err) = if r.fd == 1 {
                        (&mut w.partial_err, true)
                    } else {
                        (&mut w.partial_out, false)
                    };
                    for &b in &r.data {
                        if b == b'\n' {
                            let line = std::mem::take(buf);
                            new_lines.push((
                                String::from_utf8_lossy(&line)
                                    .trim_end_matches('\r')
                                    .to_string(),
                                err,
                            ));
                        } else {
                            buf.push(b);
                        }
                    }
                }
            })
            .await;

        let Ok((next, finished)) = res else {
            return false;
        };
        w.cursor = next;

        if let Some(item) = session.items.iter_mut().find(|i| i.id() == w.item_id) {
            for (line, err) in new_lines {
                item.push_output_line(line, err);
            }
            if let Some(code) = finished {
                if !w.partial_out.is_empty() {
                    let tail = String::from_utf8_lossy(&std::mem::take(&mut w.partial_out))
                        .trim_end_matches('\r')
                        .to_string();
                    item.push_output_line(tail, false);
                }
                if !w.partial_err.is_empty() {
                    let tail = String::from_utf8_lossy(&std::mem::take(&mut w.partial_err))
                        .trim_end_matches('\r')
                        .to_string();
                    item.push_output_line(tail, true);
                }
                if let TranscriptItem::Tool {
                    status,
                    duration_ms,
                    exit_code,
                    ..
                } = item
                {
                    *status = if code == 0 {
                        CardStatus::Done
                    } else {
                        CardStatus::Error
                    };
                    *exit_code = Some(code);
                    *duration_ms = Some(w.started.elapsed().as_millis() as u64);
                }
                session.touch();
                return true;
            }
        }
        false
    }

    fn start_agent_turn(&mut self, prompt: String) {
        if self.agent_running {
            self.set_toast("agent is still running (Esc or /cancel to stop)", true);
            return;
        }
        let Some((provider, model)) = resolve_active_provider(&self.data_dir) else {
            self.push_notice(
                "No model provider configured. Run `/login` (API Key or OAuth) or prefix shell commands with `!`.",
                true,
            );
            return;
        };

        self.session.maybe_set_title_from(&prompt);
        let user_id = self.session.alloc_id();
        self.session.items.push(TranscriptItem::User {
            id: user_id,
            text: prompt.clone(),
            mode: format!("Agent · {}", model.display()),
            cwd: self.cwd_label(),
            ts_ms: now_ms(),
        });
        self.session.messages.push(ChatMessage {
            role: ChatRole::User,
            content: prompt,
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        });
        self.session.touch();
        self.follow = true;

        self.pending_ask = None;
        let (tx, rx) = mpsc::unbounded_channel();
        let cfg = TurnConfig {
            data_dir: self.data_dir.clone(),
            conduit: self.conduit.clone(),
            target: self.target.clone(),
            cwd: self.session.cwd.clone(),
            provider,
            model,
            history: self.session.messages.clone(),
            asks: self.asks.clone(),
            steer: self.steer.clone(),
        };
        let handle = tokio::spawn(run_agent_turn(cfg, tx));
        self.agent_running = true;
        self.agent_rx = Some(rx);
        self.agent_task = Some(handle);
        self.active_assistant_id = None;
        self.active_tool_items.clear();
    }

    fn drain_agent_events(&mut self) {
        let Some(mut rx) = self.agent_rx.take() else {
            return;
        };
        let mut finished = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AgentEvent::AssistantStart { model } => {
                    let id = self.session.alloc_id();
                    self.session.items.push(TranscriptItem::Assistant {
                        id,
                        text: String::new(),
                        reasoning: String::new(),
                        reasoning_collapsed: true,
                        model,
                        done: false,
                        error: None,
                    });
                    self.active_assistant_id = Some(id);
                }
                AgentEvent::ReasoningDelta(d) => {
                    if let Some(id) = self.active_assistant_id {
                        if let Some(TranscriptItem::Assistant { reasoning, .. }) =
                            self.session.items.iter_mut().find(|i| i.id() == id)
                        {
                            reasoning.push_str(&d);
                        }
                    }
                }
                AgentEvent::TextDelta(d) => {
                    if let Some(id) = self.active_assistant_id {
                        if let Some(TranscriptItem::Assistant { text, .. }) =
                            self.session.items.iter_mut().find(|i| i.id() == id)
                        {
                            text.push_str(&d);
                        }
                    }
                }
                AgentEvent::ToolStart {
                    call_id,
                    name,
                    title,
                    subtitle,
                } => {
                    if let Some(id) = self.active_assistant_id {
                        if let Some(TranscriptItem::Assistant { done, .. }) =
                            self.session.items.iter_mut().find(|i| i.id() == id)
                        {
                            *done = true;
                        }
                    }
                    let card_id = self.session.alloc_id();
                    self.session.items.push(TranscriptItem::Tool {
                        id: card_id,
                        call_id: call_id.clone(),
                        name,
                        title,
                        subtitle,
                        job_id: None,
                        target: self.target.clone(),
                        output: Vec::new(),
                        status: CardStatus::Running,
                        collapsed: false,
                        duration_ms: None,
                        exit_code: None,
                        started_ms: now_ms(),
                    });
                    self.active_tool_items.insert(call_id, card_id);
                }
                AgentEvent::ToolJob { call_id, job_id } => {
                    if let Some(&item_id) = self.active_tool_items.get(&call_id) {
                        if let Some(TranscriptItem::Tool {
                            job_id: slot, ..
                        }) = self.session.items.iter_mut().find(|i| i.id() == item_id)
                        {
                            *slot = Some(job_id);
                        }
                    }
                }
                AgentEvent::ToolOutput { call_id, text, err } => {
                    if let Some(&item_id) = self.active_tool_items.get(&call_id) {
                        if let Some(item) = self.session.items.iter_mut().find(|i| i.id() == item_id)
                        {
                            item.push_output_text(&text, err);
                        }
                    }
                }
                AgentEvent::ToolDone {
                    call_id,
                    ok,
                    duration_ms,
                    exit_code,
                } => {
                    if let Some(&item_id) = self.active_tool_items.get(&call_id) {
                        if let Some(TranscriptItem::Tool {
                            status,
                            duration_ms: dur,
                            exit_code: code,
                            output,
                            collapsed,
                            name,
                            ..
                        }) = self.session.items.iter_mut().find(|i| i.id() == item_id)
                        {
                            *status = if ok {
                                CardStatus::Done
                            } else {
                                CardStatus::Error
                            };
                            *dur = Some(duration_ms);
                            *code = exit_code;
                            // Collapse lengthy read/ls outputs cleanly like opencode
                            if ok && name != "exec" && output.len() > 14 {
                                *collapsed = true;
                            }
                        }
                    }
                }
                AgentEvent::Say { text, kind } => {
                    let id = self.session.alloc_id();
                    self.session.items.push(TranscriptItem::Say {
                        id,
                        text,
                        level: kind,
                    });
                    self.session.touch();
                    self.persist_session();
                    self.follow = true;
                }
                AgentEvent::Ask {
                    id,
                    question,
                    options,
                    recommended,
                    timeout_ms,
                } => {
                    self.pending_ask = Some(PendingAsk {
                        id,
                        question,
                        options,
                        recommended,
                        selected: recommended,
                        deadline: Instant::now() + Duration::from_millis(timeout_ms),
                    });
                    self.follow = true;
                }
                AgentEvent::AskClosed { id } => {
                    if self.pending_ask.as_ref().map(|a| a.id) == Some(id) {
                        self.pending_ask = None;
                        self.set_toast("no answer; using the recommended option", false);
                    }
                }
                AgentEvent::TurnDone {
                    messages,
                    input_tokens,
                    output_tokens,
                } => {
                    if let Some(id) = self.active_assistant_id {
                        if let Some(TranscriptItem::Assistant { done, .. }) =
                            self.session.items.iter_mut().find(|i| i.id() == id)
                        {
                            *done = true;
                        }
                    }
                    self.session.messages = messages;
                    self.session.input_tokens += input_tokens;
                    self.session.output_tokens += output_tokens;
                    self.session.touch();
                    self.persist_session();
                    finished = true;
                }
                AgentEvent::TurnError(err) => {
                    if let Some(id) = self.active_assistant_id {
                        if let Some(TranscriptItem::Assistant { done, error, .. }) =
                            self.session.items.iter_mut().find(|i| i.id() == id)
                        {
                            *done = true;
                            *error = Some(err.clone());
                        }
                    } else {
                        self.push_notice(err, true);
                    }
                    self.session.touch();
                    self.persist_session();
                    finished = true;
                }
            }
        }
        if finished {
            self.agent_running = false;
            self.agent_task = None;
            self.active_assistant_id = None;
            self.active_tool_items.clear();
            self.pending_ask = None;
            if let Some(next) = self.queued.pop_front() {
                self.start_agent_turn(next);
            }
        } else {
            self.agent_rx = Some(rx);
        }
    }

    /// Enter while idle starts a turn; while running it steers the loop with
    /// the user's exact words at the next tool boundary.
    fn submit_agent(&mut self, prompt: String) {
        if !self.agent_running {
            self.start_agent_turn(prompt);
            return;
        }
        self.steer.lock().unwrap().push(prompt.clone());
        let id = self.session.alloc_id();
        self.session.items.push(TranscriptItem::User {
            id,
            text: prompt,
            mode: "Agent · steer".into(),
            cwd: self.cwd_label(),
            ts_ms: now_ms(),
        });
        self.session.touch();
        self.persist_session();
        self.follow = true;
        self.set_toast("steer queued for the next tool boundary", false);
    }

    /// Alt+Enter: deliver after the current run finishes.
    fn queue_followup(&mut self, prompt: String) {
        self.queued.push_back(prompt);
        self.set_toast("follow-up queued", false);
    }

    fn handle_ask_key(&mut self, key: KeyEvent) -> Action {
        let Some(mut ask) = self.pending_ask.take() else {
            return Action::None;
        };
        match key.code {
            KeyCode::Up => {
                ask.selected = ask.selected.saturating_sub(1);
                self.pending_ask = Some(ask);
            }
            KeyCode::Down => {
                ask.selected = (ask.selected + 1).min(ask.options.len().saturating_sub(1));
                self.pending_ask = Some(ask);
            }
            KeyCode::Enter => {
                let label = ask
                    .options
                    .get(ask.selected)
                    .map(|o| o.label.clone())
                    .unwrap_or_default();
                self.asks.answer(ask.id, label.clone());
                self.set_toast(format!("chose: {label}"), false);
            }
            KeyCode::Esc => {
                if let Some(option) = ask.options.get(ask.recommended) {
                    self.asks.answer(ask.id, option.label.clone());
                    self.set_toast(format!("chose recommended: {}", option.label), false);
                }
            }
            _ => self.pending_ask = Some(ask),
        }
        Action::None
    }

    async fn cancel_active(&mut self, remote: &Remote<AnyDialer>) {
        if self.agent_running {
            if let Some(h) = self.agent_task.take() {
                h.abort();
            }
            self.agent_running = false;
            self.agent_rx = None;
            self.pending_ask = None;
            if let Some(id) = self.active_assistant_id.take() {
                if let Some(TranscriptItem::Assistant { done, error, .. }) =
                    self.session.items.iter_mut().find(|i| i.id() == id)
                {
                    *done = true;
                    *error = Some("interrupted by user".into());
                }
            }
            for item_id in self.active_tool_items.values() {
                if let Some(TranscriptItem::Tool {
                    status, job_id, ..
                }) = self.session.items.iter_mut().find(|i| i.id() == *item_id)
                {
                    if *status == CardStatus::Running {
                        *status = CardStatus::Error;
                    }
                    if let Some(jid) = *job_id {
                        let _ = remote.cancel(&self.target, jid).await;
                    }
                }
            }
            self.active_tool_items.clear();
            self.set_toast("turn cancelled", false);
            self.persist_session();
            return;
        }
        if let Some(w) = self.shell_watches.last() {
            let jid = w.job_id;
            self.cancel_job(remote, jid).await;
            return;
        }
        if let Some(job) = self.jobs.get(self.job_selected) {
            let jid = job.id;
            self.cancel_job(remote, jid).await;
        }
    }

    async fn cancel_job(&mut self, remote: &Remote<AnyDialer>, id: JobId) {
        match remote.cancel(&self.target, id).await {
            Ok(()) => self.set_toast(format!("cancel requested for #{id}"), false),
            Err(e) => self.set_toast(format!("{e:#}"), true),
        }
        self.refresh(remote).await;
    }

    fn switch_to(&mut self, target: String) {
        self.target = target.clone();
        self.session.target = target.clone();
        self.jobs.clear();
        self.job_selected = 0;
        self.shell_watches.clear();
        self.set_toast(format!("target: {target}"), false);
    }

    // ------------------------------------------------------------------ slash & autocomplete

    /// Returns either matching slash commands OR `/cd` directory completions.
    fn slash_query(&self) -> Option<&str> {
        if !self.input.starts_with('/') || self.input.contains('\n') {
            return None;
        }
        let rest = &self.input[1..];
        if rest.contains(char::is_whitespace) {
            return None;
        }
        Some(rest)
    }

    fn filtered_slash(&self) -> Vec<&'static SlashCommand> {
        let Some(query) = self.slash_query() else {
            return Vec::new();
        };
        let q = query.to_ascii_lowercase();
        SLASH_COMMANDS
            .iter()
            .filter(|cmd| {
                q.is_empty()
                    || cmd.name.starts_with(&q)
                    || cmd.aliases.iter().any(|a| a.starts_with(&q))
            })
            .collect()
    }

    fn cd_query(&self) -> Option<&str> {
        if self.input.contains('\n') {
            return None;
        }
        self.input.strip_prefix("/cd ")
    }

    fn cd_completions(&self) -> Vec<(String, String)> {
        let Some(raw) = self.cd_query() else {
            return Vec::new();
        };
        complete_directories(&self.session.cwd, raw)
    }

    // ------------------------------------------------------------------ /login flow

    fn open_login_modal(&mut self) {
        self.cancel_login_bg();
        self.modal = Some(Modal::Login(LoginModal::SelectProvider {
            filter: String::new(),
            selected: 0,
        }));
    }

    fn filtered_login_providers(&self, filter: &str) -> Vec<(usize, &'static crate::auth::BuiltinProvider)> {
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

    fn cancel_login_bg(&mut self) {
        if let Some(tx) = self.login_cancel.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.login_task.take() {
            h.abort();
        }
        self.login_rx = None;
    }

    fn start_login_method(&mut self, provider_idx: usize, method_idx: usize) {
        let prov = &BUILTIN_PROVIDERS[provider_idx];
        let method = prov.methods[method_idx];
        match method.kind {
            AuthMethodKind::OpenAiApiKey
            | AuthMethodKind::AnthropicApiKey
            | AuthMethodKind::SimpleApiKey => {
                self.modal = Some(Modal::Login(LoginModal::ApiKeyForm {
                    provider_idx,
                    key: String::new(),
                    base_url: prov.default_base_url.to_string(),
                    model: prov.models.first().copied().unwrap_or("").to_string(),
                    focus_field: 0,
                    reveal: false,
                    error: None,
                }));
            }
            AuthMethodKind::CustomForm => {
                self.modal = Some(Modal::Login(LoginModal::CustomForm {
                    protocol_idx: 0,
                    name: "Custom Provider".to_string(),
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
                            let url = crate::auth::openai_authorize_url(
                                &redirect_uri,
                                &crate::auth::Pkce {
                                    verifier: verifier.clone(),
                                    challenge,
                                },
                                &st,
                            );
                            open_browser(&url);
                            match wait_openai_browser_callback(listener, st, cancel_rx).await {
                                Ok(code) => {
                                    let http = reqwest::Client::new();
                                    match exchange_openai_code(
                                        &http,
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
                                            let _ = tx_task.send(LoginEvent::Error(format!("{e:#}")));
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
                self.login_rx = Some(rx);
                self.login_task = Some(handle);
                self.login_cancel = Some(cancel_tx);
                self.modal = Some(Modal::Login(LoginModal::OAuthBrowser {
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
                    let http = reqwest::Client::new();
                    match start_openai_device_auth(&http).await {
                        Ok(start) => {
                            open_browser(&start.verification_url);
                            let _ = tx.send(LoginEvent::OpenAiDeviceStarted {
                                verification_url: start.verification_url.clone(),
                                user_code: start.user_code.clone(),
                            });
                            match poll_openai_device_auth(&http, &start).await {
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
                self.login_rx = Some(rx);
                self.login_task = Some(handle);
                self.modal = Some(Modal::Login(LoginModal::OAuthDevice {
                    provider_name: prov.name.to_string(),
                    verification_uri: "https://auth.openai.com/codex/device".into(),
                    user_code: "requesting…".into(),
                    status: "Requesting device code from OpenAI…".into(),
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
                self.modal = Some(Modal::Login(LoginModal::OAuthCodePaste {
                    provider_name: if console {
                        "Anthropic Console (API Key)".into()
                    } else {
                        "Claude Pro / Max (OAuth)".into()
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
                    let http = reqwest::Client::new();
                    match start_github_copilot_device(&http).await {
                        Ok(start) => {
                            open_browser(&start.verification_uri);
                            let _ = tx.send(LoginEvent::GitHubDeviceStarted {
                                verification_uri: start.verification_uri.clone(),
                                user_code: start.user_code.clone(),
                            });
                            match poll_github_copilot_device(&http, &start).await {
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
                self.login_rx = Some(rx);
                self.login_task = Some(handle);
                self.modal = Some(Modal::Login(LoginModal::OAuthDevice {
                    provider_name: prov.name.to_string(),
                    verification_uri: "https://github.com/login/device".into(),
                    user_code: "requesting…".into(),
                    status: "Requesting device code from GitHub…".into(),
                    error: None,
                }));
            }
        }
    }

    fn drain_login_events(&mut self) {
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

    fn open_models_modal(&mut self) {
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

    fn drain_model_events(&mut self) {
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

    fn open_sessions_modal(&mut self) {
        self.persist_session();
        let sessions = list_sessions(&self.data_dir);
        self.modal = Some(Modal::Sessions(SessionsModal {
            sessions,
            selected: 0,
        }));
    }

    // ------------------------------------------------------------------ command & palette

    fn open_picker(&mut self, title: &str, items: Vec<(String, Command)>) {
        self.modal = Some(Modal::Picker(Picker {
            title: title.to_string(),
            query: String::new(),
            items,
            selected: 0,
        }));
    }

    fn machine_items(&self) -> Vec<(String, Command)> {
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

    fn command_items(&self) -> Vec<(String, Command)> {
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

    fn run_command(&mut self, command: Command) -> Action {
        match command {
            Command::FocusInput => {
                self.focus = Focus::Input;
                Action::None
            }
            Command::OpenLogin => {
                self.open_login_modal();
                Action::None
            }
            Command::OpenModels => {
                self.open_models_modal();
                Action::None
            }
            Command::OpenSessions => {
                self.open_sessions_modal();
                Action::None
            }
            Command::NewSession => {
                self.persist_session();
                self.session = Session::new(self.session.cwd.clone(), self.target.clone());
                self.selected_card = None;
                self.follow = true;
                self.set_toast("started new session", false);
                Action::None
            }
            Command::ToggleMode => {
                self.mode = match self.mode {
                    InputMode::Agent => InputMode::Shell,
                    InputMode::Shell => InputMode::Agent,
                };
                self.set_toast(format!("mode: {}", self.mode.label()), false);
                Action::None
            }
            Command::ToggleExpand => {
                self.toggle_expand_cards();
                Action::None
            }
            Command::Cancel => Action::Cancel,
            Command::ToggleSidebar => {
                self.show_sidebar = !self.show_sidebar;
                self.save_ui_prefs();
                Action::None
            }
            Command::NextTheme => {
                self.theme = (self.theme + 1) % THEMES.len();
                self.save_ui_prefs();
                self.set_toast(format!("theme: {}", THEMES[self.theme].0), false);
                Action::None
            }
            Command::Refresh => Action::Refresh,
            Command::Switch(target) => Action::Switch(target),
            Command::Quit => Action::Quit,
        }
    }

    fn toggle_expand_cards(&mut self) {
        if let Some(idx) = self.selected_card {
            if let Some(item) = self.session.items.get_mut(idx) {
                match item {
                    TranscriptItem::Tool { collapsed, .. } => *collapsed = !*collapsed,
                    TranscriptItem::Assistant {
                        reasoning_collapsed,
                        ..
                    } => *reasoning_collapsed = !*reasoning_collapsed,
                    _ => {}
                }
                return;
            }
        }
        // Otherwise toggle the latest tool card, or all if none/uniform
        let any_collapsed = self.session.items.iter().any(|i| {
            matches!(i, TranscriptItem::Tool { collapsed: true, .. })
        });
        let target_state = !any_collapsed;
        for item in &mut self.session.items {
            if let TranscriptItem::Tool { collapsed, .. } = item {
                *collapsed = target_state;
            }
        }
    }

    fn run_slash(&mut self, text: &str) -> Action {
        let body = text.trim_start_matches('/').trim();
        let (name, arg) = match body.split_once(char::is_whitespace) {
            Some((name, arg)) => (name, arg.trim()),
            None => (body, ""),
        };
        match name {
            "login" | "connect" => {
                self.open_login_modal();
                Action::None
            }
            "logout" => {
                if arg.is_empty() {
                    if let Some((prov, _)) = resolve_active_provider(&self.data_dir) {
                        let id = prov.id.clone();
                        match remove_provider(&self.data_dir, &id) {
                            Ok(true) => {
                                if resolve_active_provider(&self.data_dir).is_none() {
                                    self.mode = InputMode::Shell;
                                }
                                self.set_toast(format!("logged out of {id}"), false);
                            }
                            Ok(false) => {
                                self.set_toast(format!("{id} comes from env var; unset env to remove"), true);
                            }
                            Err(e) => self.set_toast(format!("{e:#}"), true),
                        }
                    } else {
                        self.set_toast("no provider logged in", true);
                    }
                } else {
                    match remove_provider(&self.data_dir, arg) {
                        Ok(true) => {
                            if resolve_active_provider(&self.data_dir).is_none() {
                                self.mode = InputMode::Shell;
                            }
                            self.set_toast(format!("logged out of {arg}"), false);
                        }
                        Ok(false) => self.set_toast(format!("unknown stored provider `{arg}`"), true),
                        Err(e) => self.set_toast(format!("{e:#}"), true),
                    }
                }
                Action::None
            }
            "models" | "model" => {
                if arg.is_empty() {
                    self.open_models_modal();
                } else if let Some((prov, _)) = resolve_active_provider(&self.data_dir) {
                    let (pid, mid) = match arg.split_once('/') {
                        Some((p, m)) => (p.to_string(), m.to_string()),
                        None => (prov.id, arg.to_string()),
                    };
                    if let Err(e) = set_active_model(
                        &self.data_dir,
                        ModelRef {
                            provider_id: pid.clone(),
                            model_id: mid.clone(),
                        },
                    ) {
                        self.set_toast(format!("{e:#}"), true);
                    } else {
                        self.set_toast(format!("model: {pid}/{mid}"), false);
                    }
                } else {
                    self.set_toast("no provider connected — run /login first", true);
                }
                Action::None
            }
            "mode" => self.run_command(Command::ToggleMode),
            "cd" => {
                if arg.is_empty() {
                    self.set_toast(format!("cwd: {}", self.cwd_label()), false);
                    return Action::None;
                }
                let candidate = resolve_cd_path(&self.session.cwd, arg);
                match std::fs::canonicalize(&candidate) {
                    Ok(path) if path.is_dir() => {
                        self.session.cwd = path;
                        self.persist_session();
                        self.set_toast(format!("cwd: {}", self.cwd_label()), false);
                    }
                    Ok(_) => self.set_toast(format!("not a directory: {arg}"), true),
                    Err(e) => self.set_toast(format!("{arg}: {e}"), true),
                }
                Action::None
            }
            "machine" | "machines" => {
                if arg.is_empty() {
                    let items = self.machine_items();
                    self.open_picker("machines", items);
                    Action::None
                } else {
                    Action::Switch(arg.to_string())
                }
            }
            "new" => self.run_command(Command::NewSession),
            "sessions" | "resume" => {
                self.open_sessions_modal();
                Action::None
            }
            "expand" => {
                self.toggle_expand_cards();
                Action::None
            }
            "sidebar" => self.run_command(Command::ToggleSidebar),
            "jobs" => {
                self.show_sidebar = true;
                self.focus = Focus::Sidebar;
                Action::None
            }
            "logs" | "output" => {
                self.focus = Focus::Transcript;
                Action::None
            }
            "cancel" => Action::Cancel,
            "clear" => {
                self.session.items.clear();
                self.session.messages.clear();
                self.selected_card = None;
                self.transcript_scroll = 0;
                self.set_toast("transcript cleared", false);
                Action::None
            }
            "theme" | "themes" => self.run_command(Command::NextTheme),
            "help" => {
                self.push_notice(
                    "Shortcuts: Enter submit · Shift+Enter/Ctrl+J newline · !cmd direct shell · Tab toggle Agent/Shell · / commands · ctrl+o collapse/expand · ctrl+p palette · ctrl+x b sidebar · Esc focus transcript / cancel",
                    false,
                );
                Action::None
            }
            "quit" | "q" | "exit" => Action::Quit,
            other => {
                self.set_toast(format!("unknown command: /{other} (try /help)"), true);
                Action::None
            }
        }
    }

    // ------------------------------------------------------------------ key handling

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if self.modal.is_some() {
            return self.handle_modal_key(key);
        }
        if self.pending_ask.is_some() {
            return self.handle_ask_key(key);
        }
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Up {
            if let Some(last) = self.queued.pop_back() {
                self.input = last;
                self.input_cursor = self.input.chars().count();
                self.focus = Focus::Input;
            }
            return Action::None;
        }

        // Leader prefix (`ctrl+x`)
        if let Some(at) = self.leader_at.take() {
            if at.elapsed() <= LEADER_TIMEOUT {
                if let KeyCode::Char(c) = key.code {
                    let cmd = match c {
                        'q' => Some(Command::Quit),
                        'n' => Some(Command::NewSession),
                        'c' => Some(Command::Cancel),
                        'b' => Some(Command::ToggleSidebar),
                        't' => Some(Command::NextTheme),
                        's' => Some(Command::Refresh),
                        'l' => Some(Command::OpenLogin),
                        'm' => Some(Command::OpenModels),
                        _ => None,
                    };
                    if let Some(cmd) = cmd {
                        return self.run_command(cmd);
                    }
                }
                return Action::None;
            }
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if self.agent_running {
                        return Action::Cancel;
                    }
                    return Action::Quit;
                }
                KeyCode::Char('d') if self.input.is_empty() => return Action::Quit,
                KeyCode::Char('p') => {
                    let items = self.command_items();
                    self.open_picker("commands", items);
                    return Action::None;
                }
                KeyCode::Char('x') => {
                    self.leader_at = Some(Instant::now());
                    return Action::None;
                }
                KeyCode::Char('o') => {
                    self.toggle_expand_cards();
                    return Action::None;
                }
                KeyCode::Char('u') => {
                    self.follow = false;
                    self.transcript_scroll = self
                        .transcript_scroll
                        .saturating_sub((self.transcript_height / 2).max(1));
                    return Action::None;
                }
                KeyCode::Char('f') => {
                    self.follow = false;
                    self.transcript_scroll = self
                        .transcript_scroll
                        .saturating_add((self.transcript_height / 2).max(1));
                    return Action::None;
                }
                _ => {}
            }
        }

        if self.focus == Focus::Input {
            return self.handle_input_key(key);
        }

        match key.code {
            KeyCode::Char('/') => {
                self.focus = Focus::Input;
                self.input = "/".into();
                self.input_cursor = 1;
                self.menu_selected = 0;
                Action::None
            }
            KeyCode::Char('i') | KeyCode::Char('n') => {
                self.focus = Focus::Input;
                Action::None
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Input => Focus::Transcript,
                    Focus::Transcript => {
                        if self.show_sidebar {
                            Focus::Sidebar
                        } else {
                            Focus::Input
                        }
                    }
                    Focus::Sidebar => Focus::Input,
                };
                Action::None
            }
            KeyCode::Esc => {
                if self.agent_running {
                    return Action::Cancel;
                }
                self.focus = Focus::Input;
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                match self.focus {
                    Focus::Sidebar => {
                        self.job_selected = self.job_selected.saturating_sub(1);
                    }
                    Focus::Transcript => {
                        if let Some(idx) = self.selected_card {
                            self.selected_card = Some(idx.saturating_sub(1));
                        } else if !self.session.items.is_empty() {
                            self.selected_card = Some(self.session.items.len() - 1);
                        }
                        self.follow = false;
                        self.transcript_scroll = self.transcript_scroll.saturating_sub(2);
                    }
                    Focus::Input => {}
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match self.focus {
                    Focus::Sidebar => {
                        if self.job_selected + 1 < self.jobs.len() {
                            self.job_selected += 1;
                        }
                    }
                    Focus::Transcript => {
                        if !self.session.items.is_empty() {
                            let last = self.session.items.len() - 1;
                            self.selected_card = Some(
                                self.selected_card.map(|i| (i + 1).min(last)).unwrap_or(0),
                            );
                        }
                        self.transcript_scroll = self.transcript_scroll.saturating_add(2);
                    }
                    Focus::Input => {}
                }
                Action::None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if self.focus == Focus::Transcript {
                    self.toggle_expand_cards();
                    return Action::None;
                }
                if self.focus == Focus::Sidebar {
                    if let Some(job) = self.jobs.get(self.job_selected).cloned() {
                        return Action::AttachJob(job);
                    }
                }
                Action::None
            }
            KeyCode::Char('c') if self.focus == Focus::Sidebar => {
                if let Some(job) = self.jobs.get(self.job_selected) {
                    return Action::CancelJob(job.id);
                }
                Action::None
            }
            KeyCode::Char('g') => {
                self.follow = false;
                self.transcript_scroll = 0;
                if !self.session.items.is_empty() {
                    self.selected_card = Some(0);
                }
                Action::None
            }
            KeyCode::Char('G') => {
                self.follow = true;
                if !self.session.items.is_empty() {
                    self.selected_card = Some(self.session.items.len() - 1);
                }
                Action::None
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.transcript_scroll = self
                    .transcript_scroll
                    .saturating_sub(self.transcript_height.max(1));
                Action::None
            }
            KeyCode::PageDown => {
                self.transcript_scroll = self
                    .transcript_scroll
                    .saturating_add(self.transcript_height.max(1));
                if self.transcript_scroll >= self.transcript_max_scroll {
                    self.follow = true;
                }
                Action::None
            }
            KeyCode::Home => {
                self.follow = false;
                self.transcript_scroll = 0;
                Action::None
            }
            KeyCode::End => {
                self.follow = true;
                Action::None
            }
            KeyCode::Char('b') => self.run_command(Command::ToggleSidebar),
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Char('q') => Action::Quit,
            _ => Action::None,
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Action {
        // 1. `/cd <dir>` directory completion popup
        let cd_dirs = self.cd_completions();
        if !cd_dirs.is_empty() {
            let last = cd_dirs.len() - 1;
            match key.code {
                KeyCode::Up => {
                    self.menu_selected = self.menu_selected.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.menu_selected = (self.menu_selected + 1).min(last);
                    return Action::None;
                }
                KeyCode::Tab => {
                    let chosen = &cd_dirs[self.menu_selected.min(last)].1;
                    self.input = format!("/cd {chosen}");
                    self.input_cursor = self.input.chars().count();
                    self.menu_selected = 0;
                    return Action::None;
                }
                KeyCode::Enter if key.modifiers == KeyModifiers::NONE => {
                    let chosen = cd_dirs[self.menu_selected.min(last)].1.clone();
                    self.input.clear();
                    self.input_cursor = 0;
                    self.menu_selected = 0;
                    return self.run_slash(&format!("/cd {chosen}"));
                }
                KeyCode::Esc => {
                    self.input.clear();
                    self.input_cursor = 0;
                    return Action::None;
                }
                _ => {}
            }
        }

        // 2. `/` slash command menu
        let menu = self.filtered_slash();
        if !menu.is_empty() {
            let last = menu.len() - 1;
            match key.code {
                KeyCode::Up => {
                    self.menu_selected = self.menu_selected.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.menu_selected = (self.menu_selected + 1).min(last);
                    return Action::None;
                }
                KeyCode::Tab => {
                    let cmd = menu[self.menu_selected.min(last)];
                    self.input = if cmd.args.is_empty() {
                        format!("/{}", cmd.name)
                    } else {
                        format!("/{} ", cmd.name)
                    };
                    self.input_cursor = self.input.chars().count();
                    self.menu_selected = 0;
                    return Action::None;
                }
                KeyCode::Enter if key.modifiers == KeyModifiers::NONE => {
                    let cmd = menu[self.menu_selected.min(last)];
                    if cmd.args.is_empty() || cmd.args.starts_with('[') {
                        self.input.clear();
                        self.input_cursor = 0;
                        self.menu_selected = 0;
                        return self.run_slash(&format!("/{}", cmd.name));
                    }
                    self.input = format!("/{} ", cmd.name);
                    self.input_cursor = self.input.chars().count();
                    self.menu_selected = 0;
                    return Action::None;
                }
                KeyCode::Esc => {
                    self.input.clear();
                    self.input_cursor = 0;
                    return Action::None;
                }
                _ => {}
            }
        }

        // 3. Multiline newline bindings (Shift+Enter, Alt+Enter, Ctrl+J)
        if (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT))
            || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.insert_char('\n');
            return Action::None;
        }

        match key.code {
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return Action::None;
                }
                if self.history.last().map(|h| h != &text).unwrap_or(true) {
                    self.history.push(text.clone());
                }
                self.history_pos = None;
                self.input.clear();
                self.input_cursor = 0;
                self.menu_selected = 0;

                if text.starts_with('/') {
                    return self.run_slash(&text);
                }
                if let Some(shell_cmd) = text.strip_prefix('!') {
                    let cmd = shell_cmd.trim().to_string();
                    if cmd.is_empty() {
                        return Action::None;
                    }
                    return Action::SubmitShell(SubmitRequest {
                        spec: TaskSpec::Shell { command: cmd },
                        cwd: Some(self.session.cwd.display().to_string()),
                        env: Vec::new(),
                        label: None,
                        group: DEFAULT_GROUP.to_string(),
                        timeout_s: 0,
                    });
                }
                match self.mode {
                    InputMode::Shell => Action::SubmitShell(SubmitRequest {
                        spec: TaskSpec::Shell { command: text },
                        cwd: Some(self.session.cwd.display().to_string()),
                        env: Vec::new(),
                        label: None,
                        group: DEFAULT_GROUP.to_string(),
                        timeout_s: 0,
                    }),
                    InputMode::Agent if key.modifiers.contains(KeyModifiers::ALT) => {
                        Action::SubmitAgentFollowup(text)
                    }
                    InputMode::Agent => Action::SubmitAgent(text),
                }
            }
            KeyCode::Tab => {
                // Opencode-style Tab: switch Agent <-> Shell mode
                self.mode = match self.mode {
                    InputMode::Agent => InputMode::Shell,
                    InputMode::Shell => InputMode::Agent,
                };
                self.set_toast(format!("mode: {}", self.mode.label()), false);
                Action::None
            }
            KeyCode::BackTab => {
                self.focus = Focus::Transcript;
                Action::None
            }
            KeyCode::Esc => {
                if !self.input.is_empty() {
                    self.input.clear();
                    self.input_cursor = 0;
                } else if self.agent_running {
                    return Action::Cancel;
                } else {
                    self.focus = Focus::Transcript;
                }
                Action::None
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.transcript_scroll = self
                    .transcript_scroll
                    .saturating_sub(self.transcript_height.max(1));
                Action::None
            }
            KeyCode::PageDown => {
                self.transcript_scroll = self
                    .transcript_scroll
                    .saturating_add(self.transcript_height.max(1));
                if self.transcript_scroll >= self.transcript_max_scroll {
                    self.follow = true;
                }
                Action::None
            }
            KeyCode::Backspace => {
                self.delete_before_cursor();
                Action::None
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
                Action::None
            }
            KeyCode::Left => {
                self.input_cursor = self.input_cursor.saturating_sub(1);
                Action::None
            }
            KeyCode::Right => {
                let len = self.input.chars().count();
                if self.input_cursor < len {
                    self.input_cursor += 1;
                }
                Action::None
            }
            KeyCode::Home => {
                self.input_cursor = self.line_start_char(self.input_cursor);
                Action::None
            }
            KeyCode::End => {
                self.input_cursor = self.line_end_char(self.input_cursor);
                Action::None
            }
            KeyCode::Up => {
                if self.move_cursor_vertical(-1) {
                    return Action::None;
                }
                if !self.history.is_empty() {
                    let pos = match self.history_pos {
                        Some(p) => p.saturating_sub(1),
                        None => self.history.len() - 1,
                    };
                    self.history_pos = Some(pos);
                    self.input = self.history[pos].clone();
                    self.input_cursor = self.input.chars().count();
                }
                Action::None
            }
            KeyCode::Down => {
                if self.move_cursor_vertical(1) {
                    return Action::None;
                }
                if let Some(pos) = self.history_pos {
                    if pos + 1 < self.history.len() {
                        self.history_pos = Some(pos + 1);
                        self.input = self.history[pos + 1].clone();
                    } else {
                        self.history_pos = None;
                        self.input.clear();
                    }
                    self.input_cursor = self.input.chars().count();
                }
                Action::None
            }
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match c {
                    'u' => {
                        self.input.clear();
                        self.input_cursor = 0;
                    }
                    'a' => self.input_cursor = self.line_start_char(self.input_cursor),
                    'e' => self.input_cursor = self.line_end_char(self.input_cursor),
                    'k' => {
                        let byte = self.byte_index(self.input_cursor);
                        self.input.truncate(byte);
                    }
                    _ => {}
                }
                Action::None
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> Action {
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

    fn handle_login_key(&mut self, login: LoginModal, key: KeyEvent) {
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
                                self.modal = Some(Modal::Login(LoginModal::SelectMethod {
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
                self.modal = Some(Modal::Login(LoginModal::SelectProvider { filter, selected }));
            }
            LoginModal::SelectMethod {
                provider_idx,
                mut selected,
            } => {
                let prov = &BUILTIN_PROVIDERS[provider_idx];
                let len = prov.methods.len().max(1);
                match key.code {
                    KeyCode::Left | KeyCode::Backspace => {
                        self.modal = Some(Modal::Login(LoginModal::SelectProvider {
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
                self.modal = Some(Modal::Login(LoginModal::SelectMethod {
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
                                updated_ms: now_ms(),
                            };
                            let chosen = if model.trim().is_empty() {
                                None
                            } else {
                                Some(model.trim().to_string())
                            };
                            match record_provider_login(&self.data_dir, sp, chosen) {
                                Ok(mref) => {
                                    self.mode = InputMode::Agent;
                                    self.set_toast(
                                        format!("connected {} · {}", prov.name, mref.display()),
                                        false,
                                    );
                                    self.push_notice(
                                        format!(
                                            "Connected to {} (api_key). Active model: {}",
                                            prov.name,
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
                            0 => api_key.push(c),
                            1 => base_url.push(c),
                            _ => model.push(c),
                        }
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Login(LoginModal::ApiKeyForm {
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
                                updated_ms: now_ms(),
                            };
                            let pname = sp.name.clone();
                            match record_provider_login(
                                &self.data_dir,
                                sp,
                                Some(model.trim().to_string()),
                            ) {
                                Ok(mref) => {
                                    self.mode = InputMode::Agent;
                                    self.set_toast(
                                        format!("connected {pname} · {}", mref.display()),
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
                self.modal = Some(Modal::Login(LoginModal::CustomForm {
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
                                let http = reqwest::Client::new();
                                match exchange_openai_code(
                                    &http,
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
                            self.login_rx = Some(rx);
                            self.login_task = Some(handle);
                            busy = true;
                            status = "Exchanging authorization code…".into();
                            error = None;
                        }
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            error = None;
                            pasted.push(c);
                        }
                        _ => {}
                    }
                }
                self.modal = Some(Modal::Login(LoginModal::OAuthBrowser {
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
                                let http = reqwest::Client::new();
                                match exchange_anthropic_code(&http, &raw, &ver, &st, console).await
                                {
                                    Ok(sp) => {
                                        let _ = tx.send(LoginEvent::Success(sp, None));
                                    }
                                    Err(e) => {
                                        let _ = tx.send(LoginEvent::Error(format!("{e:#}")));
                                    }
                                }
                            });
                            self.login_rx = Some(rx);
                            self.login_task = Some(handle);
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
                self.modal = Some(Modal::Login(LoginModal::OAuthCodePaste {
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
                self.modal = Some(Modal::Login(LoginModal::OAuthDevice {
                    provider_name,
                    verification_uri,
                    user_code,
                    status,
                    error,
                }));
            }
        }
    }

    // ------------------------------------------------------------------ editor cursor helpers

    fn insert_char(&mut self, c: char) {
        let at = self.byte_index(self.input_cursor);
        self.input.insert(at, c);
        self.input_cursor += 1;
        self.menu_selected = 0;
    }

    fn delete_before_cursor(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let at = self.byte_index(self.input_cursor - 1);
        self.input.remove(at);
        self.input_cursor -= 1;
        self.menu_selected = 0;
    }

    fn delete_at_cursor(&mut self) {
        if self.input_cursor >= self.input.chars().count() {
            return;
        }
        let at = self.byte_index(self.input_cursor);
        self.input.remove(at);
        self.menu_selected = 0;
    }

    fn byte_index(&self, chars: usize) -> usize {
        self.input
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    fn line_start_char(&self, cursor: usize) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = cursor.min(chars.len());
        while i > 0 && chars[i - 1] != '\n' {
            i -= 1;
        }
        i
    }

    fn line_end_char(&self, cursor: usize) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = cursor.min(chars.len());
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        i
    }

    fn move_cursor_vertical(&mut self, delta: isize) -> bool {
        let chars: Vec<char> = self.input.chars().collect();
        let cur_start = self.line_start_char(self.input_cursor);
        let col = self.input_cursor - cur_start;
        if delta < 0 {
            if cur_start == 0 {
                return false;
            }
            let prev_end = cur_start - 1;
            let prev_start = self.line_start_char(prev_end);
            let prev_len = prev_end - prev_start;
            self.input_cursor = prev_start + col.min(prev_len);
            true
        } else {
            let cur_end = self.line_end_char(self.input_cursor);
            if cur_end >= chars.len() {
                return false;
            }
            let next_start = cur_end + 1;
            let next_end = self.line_end_char(next_start);
            let next_len = next_end - next_start;
            self.input_cursor = next_start + col.min(next_len);
            true
        }
    }
}

fn openai_url_preview(redirect_uri: &str, challenge: &str, state: &str) -> String {
    crate::auth::openai_authorize_url(
        redirect_uri,
        &crate::auth::Pkce {
            verifier: String::new(),
            challenge: challenge.to_string(),
        },
        state,
    )
}

fn collapse_home(path: &Path) -> String {
    let home = std::env::var("HOME").ok();
    let text = path.display().to_string();
    match home {
        Some(home) if text == home => "~".to_string(),
        Some(home) if text.starts_with(&format!("{home}/")) => {
            format!("~{}", &text[home.len()..])
        }
        _ => text,
    }
}

fn resolve_cd_path(cwd: &Path, raw: &str) -> PathBuf {
    if raw == "~" {
        return PathBuf::from(std::env::var("HOME").unwrap_or_default());
    }
    if let Some(tail) = raw.strip_prefix("~/") {
        return PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(tail);
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

/// Directory autocomplete entries for `/cd <arg>`: returns `(display, completion_arg)`.
pub fn complete_directories(cwd: &Path, raw: &str) -> Vec<(String, String)> {
    let expanded_prefix = if raw == "~" || raw.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        if raw == "~" {
            format!("{home}/")
        } else {
            format!("{home}/{}", &raw[2..])
        }
    } else {
        raw.to_string()
    };

    let (dir_to_read, name_prefix, arg_dir_prefix) = if raw.is_empty() {
        (cwd.to_path_buf(), String::new(), String::new())
    } else if raw.ends_with('/') {
        let dir = resolve_cd_path(cwd, raw);
        (dir, String::new(), raw.to_string())
    } else if let Some(idx) = raw.rfind('/') {
        let parent_raw = &raw[..=idx];
        let dir = resolve_cd_path(cwd, parent_raw);
        (dir, raw[idx + 1..].to_string(), parent_raw.to_string())
    } else {
        let _ = expanded_prefix;
        (cwd.to_path_buf(), raw.to_string(), String::new())
    };

    let Ok(entries) = std::fs::read_dir(&dir_to_read) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let is_dir = ft.is_dir() || (ft.is_symlink() && entry.path().is_dir());
        if !is_dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && !name_prefix.starts_with('.') {
            continue;
        }
        if !name_prefix.is_empty() && !name.starts_with(&name_prefix) {
            continue;
        }
        let completed = format!("{arg_dir_prefix}{name}/");
        dirs.push((format!("{name}/"), completed));
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    dirs.truncate(10);
    dirs
}

// ------------------------------------------------------------------ drawing

fn draw(frame: &mut Frame, app: &mut App) {
    let theme = app.theme();
    let area = frame.area();
    // Opaque base: every cell gets a background before any pane draws.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg)),
        area,
    );

    let editor_inner_width = area.width.saturating_sub(6).max(10) as usize;
    let (visual_lines, cursor_row, cursor_col) =
        layout_editor_text(&app.input, app.input_cursor, editor_inner_width);
    let editor_height = ((visual_lines.len().clamp(1, 6)) as u16) + 2;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(editor_height),
            Constraint::Length(1),
        ])
        .split(area);

    let max_rows = editor_height.saturating_sub(2) as usize;
    let editor_skip = if cursor_row >= max_rows {
        (cursor_row + 1 - max_rows).min(visual_lines.len().saturating_sub(max_rows))
    } else {
        0
    };

    draw_header(frame, app, &theme, chunks[0]);
    draw_body(frame, app, &theme, chunks[1]);
    draw_editor(frame, app, &theme, chunks[2], &visual_lines, editor_skip);
    draw_footer(frame, app, &theme, chunks[3]);

    draw_queued(frame, app, &theme, chunks[2]);

    // Popups
    if app.pending_ask.is_some() {
        draw_ask(frame, app, &theme, area);
    } else {
        if app.focus == Focus::Input && app.modal.is_none() {
            if !app.cd_completions().is_empty() {
                draw_cd_menu(frame, app, &theme, chunks[2]);
            } else if !app.filtered_slash().is_empty() {
                draw_slash_menu(frame, app, &theme, chunks[2]);
            }
        }
        if app.modal.is_some() {
            draw_modal(frame, app, &theme, area);
        }
    }

    if app.modal.is_none() && app.pending_ask.is_none() && app.focus == Focus::Input {
        let inner = chunks[2];
        let x = inner.x + 4 + (cursor_col as u16);
        let visible_row = cursor_row.saturating_sub(editor_skip).min(max_rows.saturating_sub(1));
        let y = inner.y + 1 + (visible_row as u16);
        if x < inner.x + inner.width.saturating_sub(1) && y < inner.y + inner.height.saturating_sub(1) {
            frame.set_cursor_position((x, y));
        }
    }
}

fn draw_header(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let (dot, dot_style) = if app.connected {
        ("●", Style::default().fg(theme.ok))
    } else {
        ("○", Style::default().fg(theme.bad))
    };
    let running = app
        .jobs
        .iter()
        .filter(|j| matches!(j.state, JobState::Running { .. }))
        .count();

    let model_text = match resolve_active_provider(&app.data_dir) {
        Some((prov, m)) => format!("{} ({})", m.display(), prov.credential.kind_label()),
        None => "no model (/login)".to_string(),
    };

    let mut spans = vec![
        Span::styled(
            " envoy ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} ", app.session.title),
            Style::default().fg(theme.text),
        ),
        Span::styled("· ", Style::default().fg(theme.dim)),
        Span::styled(format!("{} ", app.cwd_label()), Style::default().fg(theme.dim)),
        Span::styled("· ", Style::default().fg(theme.dim)),
        Span::styled(dot, dot_style),
        Span::styled(format!(" {} ", app.target), Style::default().fg(theme.dim)),
        Span::styled("· ", Style::default().fg(theme.dim)),
        Span::styled(
            format!("{model_text} "),
            Style::default().fg(theme.secondary),
        ),
    ];
    if running > 0 {
        spans.push(Span::styled(
            format!("· {running} running "),
            Style::default().fg(theme.ok),
        ));
    }
    if app.agent_running {
        spans.push(Span::styled(
            "· thinking… ",
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg)),
        area,
    );
}

fn draw_body(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let (main_area, sidebar_area) = if app.show_sidebar && area.width >= 84 {
        let halves = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(34)])
            .split(area);
        (halves[0], Some(halves[1]))
    } else {
        (area, None)
    };

    draw_transcript(frame, app, theme, main_area);
    if let Some(sb) = sidebar_area {
        draw_sidebar(frame, app, theme, sb);
    }
}

fn draw_transcript(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let border_color = if app.focus == Focus::Transcript {
        theme.accent
    } else {
        theme.border
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme.bg))
        .title(Span::styled(
            if app.session.items.is_empty() {
                " home ".to_string()
            } else {
                format!(" transcript · {} ", app.session.id)
            },
            Style::default().fg(theme.dim),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.transcript_height = inner.height;

    if app.session.items.is_empty() {
        draw_welcome(frame, app, theme, inner);
        return;
    }

    let width = inner.width.saturating_sub(2).max(16) as usize;
    let mut lines: Vec<Line> = Vec::new();

    for (idx, item) in app.session.items.iter().enumerate() {
        let is_selected = app.focus == Focus::Transcript && app.selected_card == Some(idx);
        match item {
            TranscriptItem::User { text, mode, .. } => {
                lines.push(Line::from(""));
                let bar_style = Style::default().fg(if is_selected {
                    theme.warn
                } else {
                    theme.accent
                });
                let bg = theme.user_bg;
                for (li, raw_line) in text.lines().enumerate() {
                    for wrapped in wrap_text(raw_line, width.saturating_sub(3)) {
                        let mut spans = vec![Span::styled(" ┃ ", bar_style.bg(bg))];
                        spans.push(Span::styled(
                            wrapped,
                            Style::default().fg(theme.text).bg(bg),
                        ));
                        if li == 0 {
                            // keep clean
                        }
                        lines.push(Line::from(spans));
                    }
                }
                lines.push(Line::from(vec![
                    Span::styled(" ┃ ", bar_style),
                    Span::styled(mode.clone(), Style::default().fg(theme.dim)),
                ]));
            }
            TranscriptItem::Assistant {
                text,
                reasoning,
                reasoning_collapsed,
                model,
                done,
                error,
                ..
            } => {
                lines.push(Line::from(""));
                if !reasoning.is_empty() {
                    if *reasoning_collapsed {
                        lines.push(Line::from(vec![
                            Span::styled(" ▸ Thinking ", Style::default().fg(theme.dim)),
                            Span::styled(
                                format!("({} chars · ctrl+o to expand)", reasoning.chars().count()),
                                Style::default().fg(theme.dim),
                            ),
                        ]));
                    } else {
                        lines.push(Line::styled(
                            " ▾ Thinking",
                            Style::default().fg(theme.dim),
                        ));
                        for rline in reasoning.lines() {
                            for w in wrap_text(rline, width.saturating_sub(3)) {
                                lines.push(Line::from(vec![
                                    Span::styled(" │ ", Style::default().fg(theme.border)),
                                    Span::styled(w, Style::default().fg(theme.dim)),
                                ]));
                            }
                        }
                    }
                }
                render_markdown_lines(text, width, theme, &mut lines);
                if !*done {
                    lines.push(Line::styled(
                        format!(" ▍ {model}"),
                        Style::default().fg(theme.accent),
                    ));
                } else if let Some(err) = error {
                    for w in wrap_text(&format!("Error: {err}"), width.saturating_sub(2)) {
                        lines.push(Line::styled(format!(" {w}"), Style::default().fg(theme.bad)));
                    }
                }
            }
            TranscriptItem::Tool {
                title,
                subtitle,
                job_id,
                target,
                output,
                status,
                collapsed,
                duration_ms,
                exit_code,
                started_ms,
                ..
            } => {
                lines.push(Line::from(""));
                let (glyph, glyph_style) = match status {
                    CardStatus::Queued => ("○", Style::default().fg(theme.warn)),
                    CardStatus::Running => ("●", Style::default().fg(theme.ok)),
                    CardStatus::Done => ("✓", Style::default().fg(theme.ok)),
                    CardStatus::Error => ("✗", Style::default().fg(theme.bad)),
                };
                let bar_color = if is_selected {
                    theme.warn
                } else {
                    match status {
                        CardStatus::Error => theme.bad,
                        CardStatus::Running => theme.ok,
                        _ => theme.border,
                    }
                };
                let bar = Span::styled(" │ ", Style::default().fg(bar_color));

                let mut meta_parts = Vec::new();
                if let Some(jid) = job_id {
                    meta_parts.push(format!("#{jid}"));
                }
                let elapsed = duration_ms.unwrap_or_else(|| now_ms().saturating_sub(*started_ms));
                meta_parts.push(fmt_duration(elapsed));
                if let Some(code) = exit_code {
                    meta_parts.push(format!("exit {code}"));
                }
                if target != "local" {
                    meta_parts.push(target.clone());
                }
                let fold_glyph = if *collapsed { "▸" } else { "▾" };
                let meta_badge = format!("  [{}] {fold_glyph}", meta_parts.join(" · "));
                let meta_w = meta_badge.chars().map(char_width).sum::<usize>();
                let budget = width.saturating_sub(5 + meta_w).max(10);
                let title_shown = truncate_chars(title, budget);
                let title_w = title_shown.chars().map(char_width).sum::<usize>();
                let sub_budget = budget.saturating_sub(title_w + 2);

                let mut header_spans = vec![
                    bar.clone(),
                    Span::styled(format!("{glyph} "), glyph_style),
                    Span::styled(
                        title_shown,
                        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                    ),
                ];
                if !subtitle.is_empty() && sub_budget >= 6 {
                    header_spans.push(Span::styled(
                        format!("  {}", truncate_chars(subtitle, sub_budget)),
                        Style::default().fg(theme.dim),
                    ));
                }
                header_spans.push(Span::styled(meta_badge, Style::default().fg(theme.dim)));
                lines.push(Line::from(header_spans));

                if *collapsed {
                    if !output.is_empty() {
                        lines.push(Line::from(vec![
                            bar,
                            Span::styled(
                                format!(
                                    "  {} lines collapsed (ctrl+o or Enter in transcript to expand)",
                                    output.len()
                                ),
                                Style::default().fg(theme.dim),
                            ),
                        ]));
                    }
                } else if !output.is_empty() {
                    let total = output.len();
                    let preview_start = if total > 60 && *status != CardStatus::Running {
                        // Show last 50 lines inline when huge, keep card snappy
                        lines.push(Line::from(vec![
                            bar.clone(),
                            Span::styled(
                                format!("  … ({} earlier lines)", total - 50),
                                Style::default().fg(theme.dim),
                            ),
                        ]));
                        total - 50
                    } else {
                        0
                    };
                    for out_line in &output[preview_start..] {
                        let style = if out_line.err {
                            Style::default().fg(theme.bad)
                        } else {
                            Style::default().fg(theme.text)
                        };
                        for w in wrap_text(&out_line.text, width.saturating_sub(4)) {
                            lines.push(Line::from(vec![bar.clone(), Span::styled(w, style)]));
                        }
                    }
                }
            }
            TranscriptItem::Notice { text, is_error, .. } => {
                lines.push(Line::from(""));
                let style = Style::default().fg(if *is_error { theme.bad } else { theme.dim });
                for w in wrap_text(text, width.saturating_sub(2)) {
                    lines.push(Line::styled(format!(" · {w}"), style));
                }
            }
            TranscriptItem::Say { text, level, .. } => {
                lines.push(Line::from(""));
                let style = Style::default().fg(match level.as_str() {
                    "warning" => theme.warn,
                    "error" => theme.bad,
                    _ => theme.secondary,
                });
                for (i, w) in wrap_text(text, width.saturating_sub(4)).iter().enumerate() {
                    let prefix = if i == 0 { " · " } else { "   " };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(theme.accent)),
                        Span::styled(w.clone(), style),
                    ]));
                }
            }
        }
    }

    lines.push(Line::from(""));
    let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
    app.transcript_max_scroll = max_scroll;
    if app.follow {
        app.transcript_scroll = max_scroll;
    } else {
        app.transcript_scroll = app.transcript_scroll.min(max_scroll);
    }
    frame.render_widget(
        Paragraph::new(lines).scroll((app.transcript_scroll, 0)),
        inner,
    );
}

fn draw_welcome(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let daemon = if app.connected {
        "daemon ok"
    } else {
        "daemon unreachable"
    };
    let peers = match &app.status {
        Some(status) => format!("{} peer(s)", status.peers.len()),
        None => "conduit off".to_string(),
    };
    let (model_line, model_color) = match resolve_active_provider(&app.data_dir) {
        Some((prov, m)) => (
            format!("  model     {} ({})", m.display(), prov.credential.kind_label()),
            theme.ok,
        ),
        None => (
            "  model     not logged in — type /login to configure API Key or OAuth".to_string(),
            theme.warn,
        ),
    };

    let lines = vec![
        Line::from(""),
        Line::styled(
            format!("  envoy {}", env!("CARGO_PKG_VERSION")),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            "  opencode-style terminal agent & detached remote executor",
            Style::default().fg(theme.dim),
        ),
        Line::from(""),
        Line::styled(format!("  cwd       {}", app.cwd_label()), Style::default()),
        Line::styled(
            format!("  target    {} · {daemon} · {peers}", app.target),
            Style::default().fg(theme.dim),
        ),
        Line::styled(model_line, Style::default().fg(model_color)),
        Line::from(""),
        Line::styled(
            "  Type a prompt or command and press enter.",
            Style::default().fg(theme.text),
        ),
        Line::styled(
            "  Type / for commands (/login, /models, /cd, /machine, /sessions).",
            Style::default().fg(theme.dim),
        ),
        Line::styled(
            "  Prefix with ! to run a shell command directly (e.g. !ls -la).",
            Style::default().fg(theme.dim),
        ),
        Line::from(""),
        Line::styled(
            "  ctrl+p palette · tab switch Agent/Shell · shift+enter newline",
            Style::default().fg(theme.dim),
        ),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_sidebar(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let border_color = if app.focus == Focus::Sidebar {
        theme.accent
    } else {
        theme.border
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme.panel))
        .title(Span::styled(" session & jobs ", Style::default().fg(theme.dim)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(4)])
        .split(inner);

    let model_info = match resolve_active_provider(&app.data_dir) {
        Some((prov, m)) => format!("{} [{}]", m.display(), prov.credential.kind_label()),
        None => "none (/login)".to_string(),
    };
    let peers_info = match &app.status {
        Some(s) => {
            let agents = s
                .peers
                .iter()
                .filter(|p| p.has_service(envoy_proto::AGENT_SERVICE_ID))
                .count();
            format!("{agents}/{} agents", s.peers.len())
        }
        None => "conduit off".to_string(),
    };
    let tok_info = format!(
        "{} in / {} out",
        app.session.input_tokens, app.session.output_tokens
    );

    let info_lines = vec![
        Line::from(vec![
            Span::styled(" dir    ", Style::default().fg(theme.dim)),
            Span::raw(truncate_chars(&app.cwd_label(), 22)),
        ]),
        Line::from(vec![
            Span::styled(" model  ", Style::default().fg(theme.dim)),
            Span::styled(
                truncate_chars(&model_info, 22),
                Style::default().fg(theme.secondary),
            ),
        ]),
        Line::from(vec![
            Span::styled(" mode   ", Style::default().fg(theme.dim)),
            Span::styled(app.mode.label(), Style::default().fg(theme.accent)),
        ]),
        Line::from(vec![
            Span::styled(" target ", Style::default().fg(theme.dim)),
            Span::raw(format!("{} ({peers_info})", app.target)),
        ]),
        Line::from(vec![
            Span::styled(" tokens ", Style::default().fg(theme.dim)),
            Span::styled(tok_info, Style::default().fg(theme.dim)),
        ]),
        Line::from(""),
        Line::styled(
            format!(" jobs ({}) ", app.jobs.len()),
            Style::default()
                .fg(theme.dim)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    frame.render_widget(Paragraph::new(info_lines), sections[0]);

    let items: Vec<ListItem> = if app.jobs.is_empty() {
        vec![ListItem::new(Line::styled(
            "  no jobs yet",
            Style::default().fg(theme.dim),
        ))]
    } else {
        app.jobs
            .iter()
            .map(|job| {
                let (glyph, style) = match &job.state {
                    JobState::Queued { .. } => ("○", Style::default().fg(theme.warn)),
                    JobState::Running { .. } => ("●", Style::default().fg(theme.ok)),
                    JobState::Done { result, .. } if result.ok() => {
                        ("✓", Style::default().fg(theme.dim))
                    }
                    JobState::Done { .. } => ("✗", Style::default().fg(theme.bad)),
                };
                let cmd = truncate_chars(&job.command(), 18);
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {glyph} "), style),
                    Span::styled(
                        format!("#{} ", job.id),
                        Style::default().fg(theme.dim),
                    ),
                    Span::raw(cmd),
                ]))
            })
            .collect()
    };

    let mut state = ListState::default();
    if !app.jobs.is_empty() {
        state.select(Some(app.job_selected));
    }
    let list = List::new(items).highlight_style(
        Style::default()
            .bg(theme.selection)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, sections[1], &mut state);
}

fn draw_editor(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    area: Rect,
    visual_lines: &[String],
    skip: usize,
) {
    let mode_color = match app.mode {
        InputMode::Agent => theme.accent,
        InputMode::Shell => theme.warn,
    };
    let title = match app.active_toast() {
        Some((msg, err)) => Span::styled(
            format!(" {} · {msg} ", app.mode.label()),
            Style::default().fg(if err { theme.bad } else { theme.ok }),
        ),
        None => Span::styled(
            format!(
                " {}  (/ commands · ! shell · shift+enter newline) ",
                app.mode.label()
            ),
            Style::default().fg(if app.focus == Focus::Input {
                mode_color
            } else {
                theme.dim
            }),
        ),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if app.focus == Focus::Input {
            mode_color
        } else {
            theme.border
        }))
        .style(Style::default().bg(theme.panel))
        .title(title);

    let max_rows = area.height.saturating_sub(2) as usize;
    let skip = skip.min(visual_lines.len().saturating_sub(max_rows));
    let end = (skip + max_rows).min(visual_lines.len());
    let mut lines: Vec<Line> = visual_lines[skip..end]
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let prefix = if i == 0 && skip == 0 { " > " } else { "   " };
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(mode_color)),
                Span::raw(text.clone()),
            ])
        })
        .collect();
    if app.input.is_empty() && app.focus != Focus::Input {
        if let Some(first) = lines.first_mut() {
            first.spans.push(Span::styled(
                "press i or / to type",
                Style::default().fg(theme.dim),
            ));
        }
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_queued(frame: &mut Frame, app: &App, theme: &Theme, editor: Rect) {
    if app.queued.is_empty() {
        return;
    }
    let take = app.queued.len().min(3);
    let height = take as u16 + 2;
    let area = Rect {
        x: editor.x + 1,
        y: editor.y.saturating_sub(height),
        width: editor.width.saturating_sub(2),
        height,
    };
    if area.height < 3 {
        return;
    }
    frame.render_widget(Clear, area);
    let mut lines = Vec::new();
    for text in app.queued.iter().take(take) {
        lines.push(Line::from(vec![
            Span::styled(" ⏳ later  ", Style::default().fg(theme.warn)),
            Span::raw(truncate_width(
                text,
                area.width.saturating_sub(12) as usize,
            )),
        ]));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warn))
        .style(Style::default().bg(theme.panel))
        .title(Span::styled(" queued ", Style::default().fg(theme.warn)));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_ask(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(ask) = &app.pending_ask else {
        return;
    };
    let width = 64u16.min(area.width.saturating_sub(4));
    let height = (ask.options.len() as u16 + 8).min(area.height.saturating_sub(2));
    if width < 20 || height < 5 {
        return;
    }
    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);
    let mut lines = vec![Line::from("")];
    for wrapped in wrap_text(&ask.question, width.saturating_sub(4) as usize) {
        lines.push(Line::styled(
            format!(" {wrapped}"),
            Style::default().fg(theme.text),
        ));
    }
    lines.push(Line::from(""));
    for (index, option) in ask.options.iter().enumerate() {
        let chosen = index == ask.selected;
        let marker = if chosen { " ▸ " } else { "   " };
        let style = if chosen {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };
        let rec = if option.recommended { "  (recommended)" } else { "" };
        lines.push(Line::styled(format!("{marker}{}{rec}", option.label), style));
        if let Some(desc) = &option.description {
            lines.push(Line::styled(
                format!("     {desc}"),
                Style::default().fg(theme.dim),
            ));
        }
    }
    lines.push(Line::from(""));
    let left = ask
        .deadline
        .saturating_duration_since(Instant::now())
        .as_secs();
    lines.push(Line::styled(
        format!(" enter choose · esc recommended · {left}s left"),
        Style::default().fg(theme.dim),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warn))
        .style(Style::default().bg(theme.panel))
        .title(Span::styled(" ask ", Style::default().fg(theme.warn)));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn draw_footer(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let model_hint = match resolve_active_provider(&app.data_dir) {
        Some((_, m)) => m.display(),
        None => "run /login".to_string(),
    };
    let hints = if app.agent_running {
        Line::from(vec![
            Span::styled(" enter ", Style::default().fg(theme.accent)),
            Span::styled("steer ", Style::default().fg(theme.dim)),
            Span::styled("alt+enter ", Style::default().fg(theme.accent)),
            Span::styled("queue ", Style::default().fg(theme.dim)),
            Span::styled("esc ", Style::default().fg(theme.accent)),
            Span::styled("interrupt ", Style::default().fg(theme.dim)),
            Span::styled("! ", Style::default().fg(theme.accent)),
            Span::styled("shell ", Style::default().fg(theme.dim)),
            Span::styled("ctrl+o ", Style::default().fg(theme.accent)),
            Span::styled("expand", Style::default().fg(theme.dim)),
        ])
    } else {
        Line::from(vec![
        Span::styled(
            format!(" {} ", app.mode.label()),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{model_hint}  "), Style::default().fg(theme.secondary)),
        Span::styled("tab ", Style::default().fg(theme.accent)),
        Span::styled("mode  ", Style::default().fg(theme.dim)),
        Span::styled("/ ", Style::default().fg(theme.accent)),
        Span::styled("commands  ", Style::default().fg(theme.dim)),
        Span::styled("ctrl+o ", Style::default().fg(theme.accent)),
        Span::styled("fold  ", Style::default().fg(theme.dim)),
        Span::styled("ctrl+p ", Style::default().fg(theme.accent)),
        Span::styled("palette  ", Style::default().fg(theme.dim)),
        Span::styled("ctrl+x q ", Style::default().fg(theme.accent)),
        Span::styled("quit", Style::default().fg(theme.dim)),
    ])
    };
    frame.render_widget(Paragraph::new(hints), area);
}

fn draw_slash_menu(frame: &mut Frame, app: &App, theme: &Theme, editor: Rect) {
    let items = app.filtered_slash();
    if items.is_empty() {
        return;
    }
    let height = (items.len() as u16 + 2)
        .min(editor.y.saturating_sub(1))
        .min(14);
    if height < 3 {
        return;
    }
    let width = editor.width.saturating_sub(2).min(78);
    let area = Rect {
        x: editor.x + 1,
        y: editor.y.saturating_sub(height),
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let rows: Vec<ListItem> = items
        .iter()
        .map(|cmd| {
            let label = if cmd.args.is_empty() {
                format!("/{:<10} {}", cmd.name, cmd.about)
            } else {
                format!("/{:<10} {:<10} {}", cmd.name, cmd.args, cmd.about)
            };
            ListItem::new(Line::raw(format!(" {label}")))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.menu_selected.min(items.len() - 1)));
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(" commands ", Style::default().fg(theme.dim))),
        )
        .highlight_style(
            Style::default()
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_cd_menu(frame: &mut Frame, app: &App, theme: &Theme, editor: Rect) {
    let items = app.cd_completions();
    if items.is_empty() {
        return;
    }
    let height = (items.len() as u16 + 2)
        .min(editor.y.saturating_sub(1))
        .min(12);
    if height < 3 {
        return;
    }
    let width = editor.width.saturating_sub(2).min(64);
    let area = Rect {
        x: editor.x + 1,
        y: editor.y.saturating_sub(height),
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let rows: Vec<ListItem> = items
        .iter()
        .map(|(display, _)| ListItem::new(Line::raw(format!(" {display}"))))
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.menu_selected.min(items.len() - 1)));
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    " directories (Tab complete · Enter cd) ",
                    Style::default().fg(theme.dim),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_modal(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(modal) = app.modal.as_ref() else {
        return;
    };
    match modal {
        Modal::Picker(p) => {
            let filtered: Vec<&(String, Command)> = p
                .items
                .iter()
                .filter(|(label, _)| {
                    p.query.is_empty()
                        || label
                            .to_ascii_lowercase()
                            .contains(&p.query.to_ascii_lowercase())
                })
                .collect();
            let width = 62u16.min(area.width.saturating_sub(4));
            let height = ((filtered.len() as u16) + 4)
                .max(6)
                .min(area.height.saturating_sub(2))
                .min(18);
            let popup = centered_rect(area, width, height);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {} ", p.title),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(inner);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" Search: ", Style::default().fg(theme.dim)),
                    Span::raw(p.query.clone()),
                ])),
                parts[0],
            );
            let list_items: Vec<ListItem> = filtered
                .iter()
                .map(|(label, _)| ListItem::new(Line::raw(format!(" {label}"))))
                .collect();
            let mut state = ListState::default();
            if !list_items.is_empty() {
                state.select(Some(p.selected.min(list_items.len() - 1)));
            }
            let list = List::new(list_items).highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, parts[1], &mut state);
        }
        Modal::Models(m) => {
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
            let width = 66u16.min(area.width.saturating_sub(4));
            let height = ((filtered.len() as u16) + 6)
                .max(8)
                .min(area.height.saturating_sub(2))
                .min(20);
            let popup = centered_rect(area, width, height);
            frame.render_widget(Clear, popup);
            let title = if m.fetching {
                " Select model (fetching /models…) "
            } else {
                " Select model (type to filter or enter custom ID) "
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(title, Style::default().fg(theme.accent)));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(inner);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" Filter: ", Style::default().fg(theme.dim)),
                    Span::raw(m.filter.clone()),
                ])),
                parts[0],
            );
            let active = resolve_active_provider(&app.data_dir).map(|(_, mr)| mr);
            let rows: Vec<ListItem> = filtered
                .iter()
                .map(|item| {
                    let is_active = active
                        .as_ref()
                        .is_some_and(|a| a.provider_id == item.provider_id && a.model_id == item.model_id);
                    let dot = if is_active { "● " } else { "  " };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            dot,
                            Style::default().fg(if is_active { theme.ok } else { theme.dim }),
                        ),
                        Span::raw(format!("{:<30} ", item.model_id)),
                        Span::styled(
                            format!("{} ({})", item.provider_name, item.auth_kind),
                            Style::default().fg(theme.dim),
                        ),
                    ]))
                })
                .collect();
            let mut state = ListState::default();
            if !rows.is_empty() {
                state.select(Some(m.selected.min(rows.len() - 1)));
            }
            let list = List::new(rows).highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, parts[1], &mut state);
        }
        Modal::Sessions(s) => {
            let width = 72u16.min(area.width.saturating_sub(4));
            let height = ((s.sessions.len() as u16) + 4)
                .max(7)
                .min(area.height.saturating_sub(2))
                .min(18);
            let popup = centered_rect(area, width, height);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    " Sessions (Enter switch · d delete · Esc close) ",
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let rows: Vec<ListItem> = if s.sessions.is_empty() {
                vec![ListItem::new(Line::styled(
                    " (no saved sessions yet)",
                    Style::default().fg(theme.dim),
                ))]
            } else {
                s.sessions
                    .iter()
                    .map(|item| {
                        let cur = if item.id == app.session.id { "● " } else { "  " };
                        ListItem::new(Line::from(vec![
                            Span::styled(cur, Style::default().fg(theme.ok)),
                            Span::raw(format!("{:<32} ", truncate_chars(&item.title, 30))),
                            Span::styled(
                                format!(
                                    "{} · {} items",
                                    collapse_home(&item.cwd),
                                    item.item_count
                                ),
                                Style::default().fg(theme.dim),
                            ),
                        ]))
                    })
                    .collect()
            };
            let mut state = ListState::default();
            if !s.sessions.is_empty() {
                state.select(Some(s.selected.min(s.sessions.len() - 1)));
            }
            let list = List::new(rows).highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, inner, &mut state);
        }
        Modal::Login(login) => draw_login_modal(frame, app, theme, area, login),
    }
}

fn draw_login_modal(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    area: Rect,
    login: &LoginModal,
) {
    match login {
        LoginModal::SelectProvider { filter, selected } => {
            let providers = app.filtered_login_providers(filter);
            let connected = effective_providers(&app.data_dir);
            let width = 70u16.min(area.width.saturating_sub(4));
            let height = ((providers.len() as u16) + 5)
                .max(8)
                .min(area.height.saturating_sub(2))
                .min(18);
            let popup = centered_rect(area, width, height);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    " Connect a provider (/login) ",
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(inner);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" Search: ", Style::default().fg(theme.dim)),
                    Span::raw(filter.clone()),
                ])),
                parts[0],
            );

            let rows: Vec<ListItem> = providers
                .iter()
                .map(|(_, p)| {
                    let badge = match connected.get(p.id) {
                        Some(sp) => format!("[✓ {}]", sp.credential.kind_label()),
                        None => String::new(),
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!(" {:<20} ", p.name),
                            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("{:<32} ", p.subtitle), Style::default().fg(theme.dim)),
                        Span::styled(badge, Style::default().fg(theme.ok)),
                    ]))
                })
                .collect();
            let mut state = ListState::default();
            if !rows.is_empty() {
                state.select(Some((*selected).min(rows.len() - 1)));
            }
            let list = List::new(rows).highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, parts[1], &mut state);
        }
        LoginModal::SelectMethod {
            provider_idx,
            selected,
        } => {
            let prov = &BUILTIN_PROVIDERS[*provider_idx];
            let width = 72u16.min(area.width.saturating_sub(4));
            let height = ((prov.methods.len() as u16) * 2 + 4)
                .min(area.height.saturating_sub(2));
            let popup = centered_rect(area, width, height);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {} — Select auth method (Esc back/close) ", prov.name),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let rows: Vec<ListItem> = prov
                .methods
                .iter()
                .map(|m| {
                    ListItem::new(vec![
                        Line::styled(
                            format!(" {}", m.label),
                            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                        ),
                        Line::styled(format!("   {}", m.hint), Style::default().fg(theme.dim)),
                    ])
                })
                .collect();
            let mut state = ListState::default();
            state.select(Some(*selected));
            let list = List::new(rows).highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, inner, &mut state);
        }
        LoginModal::ApiKeyForm {
            provider_idx,
            key,
            base_url,
            model,
            focus_field,
            reveal,
            error,
        } => {
            let prov = &BUILTIN_PROVIDERS[*provider_idx];
            let width = 68u16.min(area.width.saturating_sub(4));
            let popup = centered_rect(area, width, 12);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {} — API Key ", prov.name),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let masked = if *reveal || key.is_empty() {
                key.clone()
            } else {
                "•".repeat(key.chars().count())
            };
            let field = |idx: usize, label: &str, val: &str| {
                let marker = if *focus_field == idx { "▸ " } else { "  " };
                let style = if *focus_field == idx {
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.dim)
                };
                Line::from(vec![
                    Span::styled(format!("{marker}{label:<12} "), style),
                    Span::raw(val.to_string()),
                ])
            };
            let mut lines = vec![
                Line::from(""),
                field(0, "API Key:", &masked),
                field(1, "Base URL:", base_url),
                field(2, "Model:", model),
                Line::from(""),
                Line::styled(
                    "  Tab switch field · Ctrl+R reveal · Enter save · Esc cancel",
                    Style::default().fg(theme.dim),
                ),
            ];
            if let Some(err) = error {
                lines.push(Line::styled(
                    format!("  Error: {err}"),
                    Style::default().fg(theme.bad),
                ));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        LoginModal::CustomForm {
            protocol_idx,
            name,
            base_url,
            key,
            model,
            focus_field,
            error,
        } => {
            let width = 72u16.min(area.width.saturating_sub(4));
            let popup = centered_rect(area, width, 14);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    " Custom Compatible Provider ",
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let proto_label = if *protocol_idx == 1 {
                "< Anthropic-Compatible >"
            } else {
                "< OpenAI-Compatible >"
            };
            let masked_key = if key.is_empty() {
                "(optional)".to_string()
            } else {
                "•".repeat(key.chars().count())
            };
            let field = |idx: usize, label: &str, val: &str| {
                let marker = if *focus_field == idx { "▸ " } else { "  " };
                let style = if *focus_field == idx {
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.dim)
                };
                Line::from(vec![
                    Span::styled(format!("{marker}{label:<12} "), style),
                    Span::raw(val.to_string()),
                ])
            };
            let mut lines = vec![
                Line::from(""),
                field(0, "Protocol:", proto_label),
                field(1, "Name:", name),
                field(2, "Base URL:", base_url),
                field(3, "API Key:", &masked_key),
                field(4, "Model ID:", model),
                Line::from(""),
                Line::styled(
                    "  Tab field · ←/→ protocol · Enter save · Esc cancel",
                    Style::default().fg(theme.dim),
                ),
            ];
            if let Some(err) = error {
                lines.push(Line::styled(
                    format!("  Error: {err}"),
                    Style::default().fg(theme.bad),
                ));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        LoginModal::OAuthBrowser {
            provider_name,
            url,
            pasted,
            status,
            busy,
            error,
            ..
        } => {
            let width = 76u16.min(area.width.saturating_sub(4));
            let popup = centered_rect(area, width, 15);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {provider_name} — OAuth Browser "),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let w = inner.width.saturating_sub(4) as usize;
            let mut lines = vec![
                Line::styled(
                    format!("  {status}"),
                    Style::default().fg(if *busy { theme.accent } else { theme.ok }),
                ),
                Line::from(""),
                Line::styled("  Authorize URL:", Style::default().fg(theme.dim)),
            ];
            for chunk in wrap_text(url, w) {
                lines.push(Line::styled(
                    format!("  {chunk}"),
                    Style::default().fg(theme.secondary),
                ));
            }
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "  Or paste redirect URL / code (for SSH/headless):",
                Style::default().fg(theme.dim),
            ));
            lines.push(Line::from(vec![
                Span::styled("  > ", Style::default().fg(theme.accent)),
                Span::raw(pasted.clone()),
            ]));
            if let Some(err) = error {
                lines.push(Line::styled(
                    format!("  Error: {err}"),
                    Style::default().fg(theme.bad),
                ));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        LoginModal::OAuthCodePaste {
            provider_name,
            url,
            code_input,
            busy,
            error,
            ..
        } => {
            let width = 76u16.min(area.width.saturating_sub(4));
            let popup = centered_rect(area, width, 15);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {provider_name} "),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let w = inner.width.saturating_sub(4) as usize;
            let mut lines = vec![
                Line::styled(
                    "  Open this URL in your browser to authorize:",
                    Style::default().fg(theme.dim),
                ),
            ];
            for chunk in wrap_text(url, w) {
                lines.push(Line::styled(
                    format!("  {chunk}"),
                    Style::default().fg(theme.secondary),
                ));
            }
            lines.push(Line::from(""));
            lines.push(Line::styled(
                if *busy {
                    "  Exchanging authorization code…"
                } else {
                    "  Paste the `code#state` (or callback URL) and press Enter:"
                },
                Style::default().fg(if *busy { theme.accent } else { theme.text }),
            ));
            lines.push(Line::from(vec![
                Span::styled("  > ", Style::default().fg(theme.accent)),
                Span::raw(code_input.clone()),
            ]));
            if let Some(err) = error {
                lines.push(Line::styled(
                    format!("  Error: {err}"),
                    Style::default().fg(theme.bad),
                ));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        LoginModal::OAuthDevice {
            provider_name,
            verification_uri,
            user_code,
            status,
            error,
        } => {
            let width = 66u16.min(area.width.saturating_sub(4));
            let popup = centered_rect(area, width, 12);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {provider_name} — Device Flow "),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let mut lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  1. Open:  ", Style::default().fg(theme.dim)),
                    Span::styled(verification_uri.clone(), Style::default().fg(theme.secondary)),
                ]),
                Line::from(vec![
                    Span::styled("  2. Code:  ", Style::default().fg(theme.dim)),
                    Span::styled(
                        user_code.clone(),
                        Style::default()
                            .fg(theme.ok)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::styled(format!("  {status}"), Style::default().fg(theme.dim)),
                Line::styled("  Press Esc to cancel.", Style::default().fg(theme.dim)),
            ];
            if let Some(err) = error {
                lines.push(Line::styled(
                    format!("  Error: {err}"),
                    Style::default().fg(theme.bad),
                ));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
    }
}

// ------------------------------------------------------------------ text / markdown helpers

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(1).max(1)
}

fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let max = max_width.max(8);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in text.chars() {
        let w = char_width(ch);
        if cur_w + w > max && !cur.is_empty() {
            // Prefer a word boundary so `exit=0` never becomes `exit=\n0`.
            if let Some(pos) = cur.rfind(' ') {
                let tail: String = cur[pos + 1..].to_string();
                let head = cur[..pos].trim_end().to_string();
                if !head.is_empty() {
                    out.push(head);
                }
                cur = tail.trim_start().to_string();
                cur_w = cur.chars().map(char_width).sum();
            } else {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
        }
        if ch == ' ' && cur.is_empty() {
            continue;
        }
        cur.push(ch);
        cur_w += w;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

fn layout_editor_text(input: &str, cursor_chars: usize, max_width: usize) -> (Vec<String>, usize, usize) {
    let max_width = max_width.max(8);
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    let mut c_row = 0usize;
    let mut c_col = 0usize;

    for (idx, ch) in input.chars().enumerate() {
        if idx == cursor_chars {
            c_row = lines.len();
            c_col = cur_w;
        }
        if ch == '\n' {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
            continue;
        }
        let w = char_width(ch);
        if cur_w + w > max_width && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
            if idx == cursor_chars {
                c_row = lines.len();
                c_col = 0;
            }
        }
        cur.push(ch);
        cur_w += w;
    }
    if cursor_chars >= input.chars().count() {
        c_row = lines.len();
        c_col = cur_w;
    }
    lines.push(cur);
    (lines, c_row, c_col)
}

fn render_markdown_lines<'a>(
    text: &str,
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'a>>,
) {
    let lines: Vec<&str> = text.lines().collect();
    let mut in_code = false;
    let mut i = 0usize;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim_start();

        if let Some(fence) = trimmed.strip_prefix("```") {
            in_code = !in_code;
            if in_code {
                let lang = fence.trim();
                out.push(Line::styled(
                    if lang.is_empty() {
                        " ┌──".to_string()
                    } else {
                        format!(" ┌─ {lang}")
                    },
                    Style::default().fg(theme.border),
                ));
            } else {
                out.push(Line::styled(" └──", Style::default().fg(theme.border)));
            }
            i += 1;
            continue;
        }
        if in_code {
            let shown = truncate_width(raw, width.saturating_sub(4));
            out.push(Line::from(vec![
                Span::styled(" │ ", Style::default().fg(theme.border)),
                Span::styled(shown, Style::default().fg(theme.secondary)),
            ]));
            i += 1;
            continue;
        }
        if is_table_start(&lines, i) {
            i += render_table(&lines[i..], width, theme, out);
            continue;
        }
        if trimmed.starts_with("### ") || trimmed.starts_with("## ") || trimmed.starts_with("# ") {
            let heading = trimmed.trim_start_matches('#').trim();
            for w in wrap_text(heading, width.saturating_sub(2)) {
                out.push(Line::styled(
                    format!(" {w}"),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            i += 1;
            continue;
        }
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let item = trimmed.trim_start_matches(['-', '*']).trim_start();
            let wrapped = wrap_text(item, width.saturating_sub(4));
            for (n, w) in wrapped.iter().enumerate() {
                let prefix = if n == 0 { " • " } else { "   " };
                let mut spans = vec![Span::styled(prefix, Style::default().fg(theme.accent))];
                spans.extend(inline_spans(w, theme));
                out.push(Line::from(spans));
            }
            i += 1;
            continue;
        }
        for w in wrap_text(raw, width.saturating_sub(2)) {
            out.push(Line::from(inline_spans(&format!(" {w}"), theme)));
        }
        i += 1;
    }
}

fn is_table_start(lines: &[&str], i: usize) -> bool {
    if i + 1 >= lines.len() {
        return false;
    }
    let head = lines[i].trim();
    let delim = lines[i + 1].trim();
    if !head.starts_with('|') || !delim.starts_with('|') {
        return false;
    }
    let cells = split_table_row(delim);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let core = c.trim_matches(':');
            !core.is_empty() && core.chars().all(|ch| ch == '-')
        })
}

fn split_table_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

fn truncate_width(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let w = char_width(ch);
        if width + w > max {
            out.push('…');
            return out;
        }
        out.push(ch);
        width += w;
    }
    out
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
    Center,
}

fn render_table<'a>(
    lines: &[&str],
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'a>>,
) -> usize {
    let header = split_table_row(lines[0]);
    let delim = split_table_row(lines[1]);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut consumed = 2usize;
    while consumed < lines.len() && lines[consumed].trim().starts_with('|') {
        rows.push(split_table_row(lines[consumed]));
        consumed += 1;
    }
    let cols = header
        .len()
        .max(delim.len())
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if cols == 0 {
        return consumed;
    }
    let aligns: Vec<Align> = (0..cols)
        .map(|c| {
            let d = delim.get(c).map(String::as_str).unwrap_or("---");
            match (d.starts_with(':'), d.ends_with(':')) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            }
        })
        .collect();

    let mut widths: Vec<usize> = (0..cols)
        .map(|c| {
            let mut w: usize = header
                .get(c)
                .map(String::as_str)
                .unwrap_or("")
                .chars()
                .map(char_width)
                .sum();
            for row in &rows {
                if let Some(cell) = row.get(c) {
                    w = w.max(cell.chars().map(char_width).sum());
                }
            }
            w.max(3)
        })
        .collect();

    let budget = width.saturating_sub(cols + 3).max(cols * 3);
    while widths.iter().sum::<usize>() > budget {
        let Some((idx, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
            break;
        };
        if widths[idx] <= 3 {
            break;
        }
        widths[idx] -= 1;
    }

    let border = Style::default().fg(theme.border);
    let mut top = String::from(" ┌");
    for (c, w) in widths.iter().enumerate() {
        top.push_str(&"─".repeat(*w + 2));
        top.push(if c + 1 == cols { '┐' } else { '┬' });
    }
    out.push(Line::styled(top, border));
    push_table_row(out, &header, &widths, &aligns, theme, true);
    let mut mid = String::from(" ├");
    for (c, w) in widths.iter().enumerate() {
        mid.push_str(&"─".repeat(*w + 2));
        mid.push(if c + 1 == cols { '┤' } else { '┼' });
    }
    out.push(Line::styled(mid, border));
    for row in &rows {
        push_table_row(out, row, &widths, &aligns, theme, false);
    }
    let mut bottom = String::from(" └");
    for (c, w) in widths.iter().enumerate() {
        bottom.push_str(&"─".repeat(*w + 2));
        bottom.push(if c + 1 == cols { '┘' } else { '┴' });
    }
    out.push(Line::styled(bottom, border));
    consumed
}

fn push_table_row<'a>(
    out: &mut Vec<Line<'a>>,
    cells: &[String],
    widths: &[usize],
    aligns: &[Align],
    theme: &Theme,
    header: bool,
) {
    let border = Style::default().fg(theme.border);
    let text_style = if header {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let mut spans = vec![Span::styled(" │ ", border)];
    for (c, width) in widths.iter().enumerate() {
        let raw = cells.get(c).map(String::as_str).unwrap_or("");
        let shown = truncate_width(raw, *width);
        let used: usize = shown.chars().map(char_width).sum();
        let pad = width.saturating_sub(used);
        let (before, after) = match aligns.get(c).copied().unwrap_or(Align::Left) {
            Align::Left => (0, pad),
            Align::Right => (pad, 0),
            Align::Center => (pad / 2, pad - pad / 2),
        };
        spans.push(Span::styled(
            format!("{}{}{}", " ".repeat(before), shown, " ".repeat(after)),
            text_style,
        ));
        spans.push(Span::styled(
            if c + 1 == widths.len() { " │" } else { " │ " },
            border,
        ));
    }
    out.push(Line::from(spans));
}

fn inline_spans<'a>(line: &str, theme: &Theme) -> Vec<Span<'a>> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut i = 0usize;
    while i < line.len() {
        let rest = &line[i..];
        let marker = if rest.starts_with('`') {
            Some(("`", Style::default().fg(theme.secondary).bg(theme.surface)))
        } else if rest.starts_with("**") {
            Some(("**", Style::default().add_modifier(Modifier::BOLD)))
        } else if rest.starts_with('*') {
            Some(("*", Style::default().add_modifier(Modifier::ITALIC)))
        } else {
            None
        };
        if let Some((marker, style)) = marker {
            let after = &rest[marker.len()..];
            if let Some(end) = after.find(marker) {
                if end > 0 {
                    if !plain.is_empty() {
                        spans.push(Span::raw(std::mem::take(&mut plain)));
                    }
                    spans.push(Span::styled(after[..end].to_string(), style));
                    i += marker.len() + end + marker.len();
                    continue;
                }
            }
        }
        let ch = rest.chars().next().expect("non-empty");
        plain.push(ch);
        i += ch.len_utf8();
    }
    if !plain.is_empty() {
        spans.push(Span::raw(plain));
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn job_elapsed(job: &Job) -> u64 {
    let now = now_ms();
    match &job.state {
        JobState::Queued { since_ms } => now.saturating_sub(*since_ms),
        JobState::Running { since_ms } => now.saturating_sub(*since_ms),
        JobState::Done { at_ms, .. } => at_ms.saturating_sub(job.created_ms),
    }
}

fn fmt_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

// ----------------------------------------------------------------- terminal

struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    enhanced_keys: bool,
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
        execute!(stdout(), EnterAlternateScreen)?;
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
        })
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if self.enhanced_keys {
            let _ = execute!(
                self.terminal.backend_mut(),
                PopKeyboardEnhancementFlags
            );
        }
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::OutputLine;

    fn tmpdir() -> PathBuf {
        let mut b = [0u8; 6];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "envoy-tui-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn job(id: JobId, state: JobState) -> Job {
        Job {
            id,
            owner: "local".into(),
            spec: TaskSpec::Shell {
                command: format!("echo {id}"),
            },
            cwd: "/tmp".into(),
            label: None,
            group: DEFAULT_GROUP.into(),
            created_ms: now_ms(),
            state,
            pid: None,
            log_bytes: 0,
        }
    }

    #[test]
    fn opens_with_input_focused_and_welcome_screen() {
        use ratatui::backend::TestBackend;
        let dir = tmpdir();
        let mut app = App::new(dir.clone(), "local".into(), None, PathBuf::from("/tmp/project"));
        app.connected = true;
        assert_eq!(app.focus, Focus::Input);

        let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        let version = format!("envoy {}", env!("CARGO_PKG_VERSION"));
        for needle in [version.as_str(), "/tmp/project", "/login", "ctrl+p"] {
            assert!(text.contains(needle), "missing {needle:?}:\n{text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slash_menu_filters_completes_and_opens_login() {
        let dir = tmpdir();
        let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
        app.input = "/log".into();
        app.input_cursor = 4;

        let names: Vec<&str> = app.filtered_slash().iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["login", "logout"]);

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "/login");

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.modal, Some(Modal::Login(LoginModal::SelectProvider { .. }))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn login_api_key_flow_persists_and_enables_agent_mode() {
        let dir = tmpdir();
        let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
        assert_eq!(app.mode, InputMode::Shell);

        app.run_slash("/login");
        // Filter to deepseek
        for ch in "deepseek".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::Login(LoginModal::ApiKeyForm { .. }))
        ));

        for ch in "sk-test-key".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.modal.is_none());
        assert_eq!(app.mode, InputMode::Agent);
        let (prov, model) = resolve_active_provider(&dir).unwrap();
        assert_eq!(prov.id, "deepseek");
        assert_eq!(model.model_id, "deepseek-chat");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cd_directory_autocomplete_lists_subdirs_and_switches() {
        let dir = tmpdir();
        let sub = dir.join("alpha-proj");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(dir.join("beta-proj")).unwrap();

        let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
        app.input = "/cd al".into();
        app.input_cursor = app.input.chars().count();

        let comps = app.cd_completions();
        assert_eq!(comps, vec![("alpha-proj/".into(), "alpha-proj/".into())]);

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.session.cwd, std::fs::canonicalize(&sub).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multiline_shift_enter_inserts_newline_and_bang_runs_shell() {
        let dir = tmpdir();
        let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
        app.mode = InputMode::Agent;

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert_eq!(app.input, "a\nb");

        app.input = "!echo hi".into();
        app.input_cursor = app.input.chars().count();
        match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            Action::SubmitShell(req) => assert_eq!(req.spec.display(), "echo hi"),
            _ => panic!("!cmd should always submit shell even in Agent mode"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renders_transcript_cards_and_sidebar() {
        use ratatui::backend::TestBackend;
        let dir = tmpdir();
        let mut app = App::new(dir.clone(), "local".into(), None, PathBuf::from("/tmp"));
        app.connected = true;
        app.jobs = vec![job(
            1,
            JobState::Done {
                at_ms: now_ms(),
                result: envoy_proto::JobResult::Success,
            },
        )];
        let uid = app.session.alloc_id();
        app.session.items.push(TranscriptItem::User {
            id: uid,
            text: "check files".into(),
            mode: "Agent".into(),
            cwd: "/tmp".into(),
            ts_ms: now_ms(),
        });
        let tid = app.session.alloc_id();
        app.session.items.push(TranscriptItem::Tool {
            id: tid,
            call_id: "c1".into(),
            name: "exec".into(),
            title: "$ ls -la".into(),
            subtitle: "/tmp".into(),
            job_id: Some(1),
            target: "local".into(),
            output: vec![OutputLine {
                text: "Cargo.toml".into(),
                err: false,
            }],
            status: CardStatus::Done,
            collapsed: false,
            duration_ms: Some(1200),
            exit_code: Some(0),
            started_ms: now_ms(),
        });

        let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        for needle in ["check files", "$ ls -la", "Cargo.toml", "exit 0", "jobs (1)"] {
            assert!(text.contains(needle), "missing {needle:?}:\n{text}");
        }

        // ctrl+o collapses tool output
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(matches!(
            app.session.items[1],
            TranscriptItem::Tool { collapsed: true, .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leader_requires_prefix_and_palette_filters() {
        let dir = tmpdir();
        let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            Action::None
        ));
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            Action::Quit
        ));

        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(matches!(app.modal, Some(Modal::Picker(_))));
        for ch in "quit".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Quit
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
