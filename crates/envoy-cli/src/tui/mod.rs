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

mod state;
mod text;
mod theme;
mod view;

pub(crate) use text::*;
pub(crate) use theme::{Theme, THEMES};

#[allow(unused_imports)] // re-exported for the state and view modules
pub(crate) use crate::auth::{
    anthropic_authorize_url, available_models, bind_openai_callback_listener,
    effective_providers, exchange_anthropic_code, exchange_openai_code, extract_query_param,
    fetch_openai_models, generate_pkce, load_prefs, open_browser,
    poll_github_copilot_device, poll_openai_device_auth, random_state, record_provider_login,
    remove_provider, resolve_active_provider, save_prefs, set_active_model, slugify_provider_id,
    start_github_copilot_device, start_openai_device_auth, wait_openai_browser_callback,
    AuthMethodKind, AvailableModel, Credential, ModelRef, ProviderProtocol, StoredProvider,
    BUILTIN_PROVIDERS,
};
pub(crate) use crate::daemon;
pub(crate) use crate::llm::{AgentEvent, AskOption};
pub(crate) use crate::runtime::{self, AgentReq, AgentResp, SendMode};
pub(crate) use crate::session::{
    delete_session, latest_session_for_cwd, list_sessions, load_session, save_session, CardStatus,
    Session, SessionSummary, TranscriptItem,
};
pub(crate) use anyhow::{bail, Result};
pub(crate) use envoy_proto::{Job, JobId, JobState, LogRecord, SubmitRequest, TaskSpec, DEFAULT_GROUP};
pub(crate) use envoy_service::{
    default_control_socket, dialer_for, AgentDialer, ConduitClient, Remote, Status,
};
pub(crate) use ratatui::backend::CrosstermBackend;
pub(crate) use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
pub(crate) use ratatui::crossterm::execute;
pub(crate) use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
pub(crate) use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
pub(crate) use ratatui::style::{Modifier, Style};
pub(crate) use ratatui::text::{Line, Span};
pub(crate) use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
pub(crate) use ratatui::{Frame, Terminal};
pub(crate) use std::collections::{HashMap, VecDeque};
pub(crate) use std::io::{stdout, IsTerminal, Stdout};
pub(crate) use std::path::PathBuf;
pub(crate) use std::time::{Duration, Instant};
pub(crate) use tokio::sync::{mpsc, oneshot, watch};
pub(crate) use tokio::task::JoinHandle;

pub(crate) const LEADER_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) type AnyDialer = Box<dyn AgentDialer>;

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

    // The turn lives in the daemon; the console attaches to its stream.
    app.spawn_agent_attach();

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
        app.drain_ui_notices();
        app.drain_login_events();
        app.drain_model_events();

        if app.agent_stream.is_none() && Instant::now() >= app.agent_retry_at {
            app.spawn_agent_attach();
        }

        if app.last_refresh.elapsed() >= app.interval {
            app.refresh(&remote).await;
        }
        app.poll_shell_cards(&remote).await;

        let app_ref = &mut app;
        tui.terminal.draw(|frame| view::draw(frame, app_ref))?;
        tokio::time::sleep(Duration::from_millis(33)).await;
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
}

#[derive(Clone)]
pub(crate) enum Command {
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

pub(crate) struct Picker {
    title: String,
    query: String,
    items: Vec<(String, Command)>,
    selected: usize,
}

pub(crate) struct SlashCommand {
    name: &'static str,
    aliases: &'static [&'static str],
    args: &'static str,
    about: &'static str,
}

pub(crate) const SLASH_COMMANDS: &[SlashCommand] = &[
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

pub(crate) enum Modal {
    Picker(Picker),
    Login(LoginModal),
    Models(ModelsModal),
    Sessions(SessionsModal),
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
    /// A send failed because the daemon went away; put the items back in the
    /// outbox so the next attach can deliver them.
    Requeue(Vec<(String, SendMode)>),
    /// The daemon returned a recalled follow-up (or had none).
    Recalled(Option<String>),
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

    // Active shell cards being tailed
    shell_watches: Vec<ShellWatch>,

    // Active /login background flow
    login_rx: Option<mpsc::UnboundedReceiver<LoginEvent>>,
    login_task: Option<JoinHandle<()>>,
    login_cancel: Option<oneshot::Sender<()>>,

    // Remote /models fetch
    models_rx: Option<oneshot::Receiver<Vec<String>>>,
}

pub(crate) struct Tui {
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
        terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        for needle in ["/tmp/project", "shell", "sign in"] {
            assert!(text.contains(needle), "missing {needle:?}:\n{text}");
        }
        assert!(
            !text.contains("ctrl+p"),
            "shortcut crib should stay off the screen:\n{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_line_shows_model_workspace_and_context() {
        use crate::auth::{record_provider_login, Credential, ProviderProtocol, StoredProvider};
        use ratatui::backend::TestBackend;

        let dir = tmpdir();
        record_provider_login(
            &dir,
            StoredProvider {
                id: "anthropic".into(),
                name: "Anthropic".into(),
                protocol: ProviderProtocol::Anthropic,
                base_url: "https://api.anthropic.com/v1".into(),
                credential: Credential::ApiKey {
                    key: "sk-test".into(),
                },
                custom_models: Vec::new(),
                updated_ms: now_ms(),
            },
            Some("claude-sonnet-4-5".into()),
        )
        .unwrap();

        let mut app = App::new(dir.clone(), "local".into(), None, PathBuf::from("/tmp/project"));
        app.connected = true;
        app.session.prompt_tokens = 50_000;
        assert_eq!(app.mode, InputMode::Agent);

        let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
        terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        for needle in ["/tmp/project", "agent", "claude-sonnet-4-5", "50k/200k"] {
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
        app.show_sidebar = true;
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
        terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        for needle in ["check files", "$ ls -la", "Cargo.toml", "jobs", "echo 1"] {
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
