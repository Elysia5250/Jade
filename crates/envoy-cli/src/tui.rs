//! `envoy ui`: terminal console modelled on opencode's TUI.
//!
//! Same shape as opencode: a header line, a sidebar, a main pane, an editor at
//! the bottom, and a footer of hints. Same vocabulary too: `ctrl+p` opens the
//! command palette, `ctrl+x` is the leader (`q` quit, `n` new job, `c` cancel,
//! `b` sidebar, `t` theme, `s` status), `tab` cycles focus, `esc` backs out.
//!
//! Every panel is a *view* over the same short RPCs the CLI uses; nothing here
//! holds a job open.

use crate::daemon;
use anyhow::{bail, Result};
use envoy_proto::{Job, JobId, JobState, LogRecord, SubmitRequest, TaskSpec, DEFAULT_GROUP};
use envoy_service::{default_control_socket, dialer_for, AgentDialer, ConduitClient, Remote, Status};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use std::io::{stdout, IsTerminal, Stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

/// How long a leader prefix stays armed.
const LEADER_TIMEOUT: Duration = Duration::from_secs(2);
/// Log lines kept in memory per job.
const MAX_LINES: usize = 5000;

type AnyDialer = Box<dyn AgentDialer>;

pub async fn run(
    data_dir: PathBuf,
    target: String,
    conduit: Option<PathBuf>,
    cwd: PathBuf,
) -> Result<()> {
    if !stdout().is_terminal() {
        bail!("`envoy` opens a console; use a subcommand for scripts (envoy jobs)");
    }
    if target == "local" || target.is_empty() {
        daemon::ensure(&data_dir).await?;
    }
    let mut remote: Remote<AnyDialer> =
        Remote::new(dialer_for(&data_dir, &target, conduit.clone()));

    let mut app = App::new(data_dir.clone(), target, conduit.clone(), cwd);
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
                    Action::Quit => return Ok(()),
                    Action::None => {}
                    Action::Submit(req) => app.submit(&remote, req).await,
                    Action::Cancel(id) => app.cancel(&remote, id).await,
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
                }
            }
        }

        if app.last_refresh.elapsed() >= app.interval {
            app.refresh(&remote).await;
        }

        let app_ref = &mut app;
        tui.terminal.draw(|frame| draw(frame, app_ref))?;
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

// --------------------------------------------------------------------- theme

#[derive(Clone, Copy)]
struct Theme {
    accent: Color,
    dim: Color,
    ok: Color,
    warn: Color,
    bad: Color,
    border: Color,
    selection: Color,
}

const THEMES: [(&str, Theme); 2] = [
    (
        "opencode",
        Theme {
            accent: Color::Rgb(0x9d, 0x7c, 0xd8),
            dim: Color::Rgb(0x80, 0x80, 0x80),
            ok: Color::Rgb(0x7f, 0xd8, 0x8f),
            warn: Color::Rgb(0xf2, 0xc9, 0x4c),
            bad: Color::Rgb(0xf2, 0x6b, 0x6b),
            border: Color::Rgb(0x4a, 0x4a, 0x55),
            selection: Color::Rgb(0x2d, 0x2a, 0x3a),
        },
    ),
    (
        "light",
        Theme {
            accent: Color::Rgb(0x3b, 0x5f, 0xc4),
            dim: Color::Rgb(0x6b, 0x6b, 0x6b),
            ok: Color::Rgb(0x2f, 0x7d, 0x32),
            warn: Color::Rgb(0x9a, 0x6b, 0x00),
            bad: Color::Rgb(0xb3, 0x26, 0x1e),
            border: Color::Rgb(0xb5, 0xb5, 0xb5),
            selection: Color::Rgb(0xe6, 0xe6, 0xef),
        },
    ),
];

// ----------------------------------------------------------------------- app

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Jobs,
    Log,
    Input,
}

enum Action {
    None,
    Quit,
    Refresh,
    Submit(SubmitRequest),
    Cancel(JobId),
    Switch(String),
}

#[derive(Clone)]
enum Command {
    NewJob,
    CancelJob,
    ToggleFollow,
    ToggleSidebar,
    NextTheme,
    Refresh,
    Switch(String),
    Quit,
}

/// A modal list: `ctrl+p` commands, or the machine picker from `/machine`.
struct Picker {
    title: String,
    items: Vec<(String, Command)>,
    selected: usize,
}

/// One entry of the `/` menu.
struct SlashCommand {
    name: &'static str,
    args: &'static str,
    about: &'static str,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand { name: "cd", args: "<dir>", about: "change the working directory" },
    SlashCommand { name: "machine", args: "[name]", about: "switch target machine" },
    SlashCommand { name: "jobs", args: "", about: "focus the job list" },
    SlashCommand { name: "logs", args: "", about: "focus the output pane" },
    SlashCommand { name: "cancel", args: "", about: "cancel the selected job" },
    SlashCommand { name: "follow", args: "", about: "toggle output following" },
    SlashCommand { name: "clear", args: "", about: "clear the output pane" },
    SlashCommand { name: "theme", args: "", about: "switch colour theme" },
    SlashCommand { name: "help", args: "", about: "show key bindings" },
    SlashCommand { name: "quit", args: "", about: "exit envoy" },
];

const HELP_LINE: &str =
    "enter run · / commands · ctrl+p palette · tab focus · ctrl+x q quit";

struct LogLine {
    text: String,
    err: bool,
}

#[derive(Default)]
struct LogBuffer {
    lines: Vec<LogLine>,
    partial_out: Vec<u8>,
    partial_err: Vec<u8>,
}

impl LogBuffer {
    fn clear(&mut self) {
        self.lines.clear();
        self.partial_out.clear();
        self.partial_err.clear();
    }

    fn push(&mut self, records: &[LogRecord]) {
        for record in records {
            let mut completed: Vec<(String, bool)> = Vec::new();
            {
                let (partial, err) = if record.fd == 1 {
                    (&mut self.partial_err, true)
                } else {
                    (&mut self.partial_out, false)
                };
                for &byte in &record.data {
                    if byte == b'\n' {
                        let line = std::mem::take(partial);
                        completed.push((String::from_utf8_lossy(&line).to_string(), err));
                    } else {
                        partial.push(byte);
                    }
                }
                if partial.len() > 64 * 1024 {
                    let line = std::mem::take(partial);
                    completed.push((String::from_utf8_lossy(&line).to_string(), err));
                }
            }
            for (line, err) in completed {
                self.push_line(line, err);
            }
        }
    }

    fn push_line(&mut self, text: String, err: bool) {
        self.lines.push(LogLine { text, err });
        if self.lines.len() > MAX_LINES {
            self.lines.drain(..self.lines.len() - MAX_LINES);
        }
    }
}

struct App {
    data_dir: PathBuf,
    cwd: PathBuf,
    target: String,
    conduit: Option<PathBuf>,
    status: Option<Status>,
    watch: Option<watch::Receiver<Option<Status>>>,
    jobs: Vec<Job>,
    selected: usize,
    focus: Focus,
    show_sidebar: bool,
    follow: bool,
    log_scroll: u16,
    log_height: u16,
    log_job: Option<JobId>,
    log_cursor: u64,
    log_finished: Option<i32>,
    log: LogBuffer,
    input: String,
    input_cursor: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    menu_selected: usize,
    picker: Option<Picker>,
    theme: usize,
    leader_at: Option<Instant>,
    message: Option<(String, bool)>,
    connected: bool,
    interval: Duration,
    last_refresh: Instant,
}

impl App {
    fn new(data_dir: PathBuf, target: String, conduit: Option<PathBuf>, cwd: PathBuf) -> Self {
        Self {
            data_dir,
            cwd,
            target,
            conduit,
            status: None,
            watch: None,
            jobs: Vec::new(),
            selected: 0,
            focus: Focus::Jobs,
            show_sidebar: true,
            follow: true,
            log_scroll: 0,
            log_height: 0,
            log_job: None,
            log_cursor: 0,
            log_finished: None,
            log: LogBuffer::default(),
            input: String::new(),
            input_cursor: 0,
            history: Vec::new(),
            history_pos: None,
            menu_selected: 0,
            picker: None,
            theme: 0,
            leader_at: None,
            message: None,
            connected: false,
            interval: Duration::from_millis(500),
            last_refresh: Instant::now() - Duration::from_secs(1),
        }
    }

    fn theme(&self) -> Theme {
        THEMES[self.theme].1
    }

    fn selected_job(&self) -> Option<&Job> {
        self.jobs.get(self.selected)
    }

    fn selected_id(&self) -> Option<JobId> {
        self.selected_job().map(|j| j.id)
    }

    async fn refresh(&mut self, remote: &Remote<AnyDialer>) {
        self.last_refresh = Instant::now();
        if let Some(watch) = &self.watch {
            if let Some(status) = watch.borrow().clone() {
                self.status = Some(status);
            }
        }
        let target = self.target.clone();

        match remote.jobs(&target).await {
            Ok(jobs) => {
                let keep = self.selected_id();
                self.jobs = jobs;
                self.selected = match keep.and_then(|id| self.jobs.iter().position(|j| j.id == id)) {
                    Some(pos) => pos,
                    None => self.jobs.len().saturating_sub(1),
                };
                if !self.connected {
                    self.message = None;
                }
                self.connected = true;
            }
            Err(e) => {
                self.connected = false;
                self.message = Some((format!("{e:#}"), true));
                return;
            }
        }

        let Some(id) = self.selected_id() else {
            self.log.clear();
            self.log_job = None;
            return;
        };
        if self.log_job != Some(id) {
            self.log.clear();
            self.log_job = Some(id);
            self.log_cursor = 0;
            self.log_finished = None;
            self.log_scroll = 0;
        }

        let cursor = self.log_cursor;
        let log = &mut self.log;
        match remote
            .logs(&target, id, cursor, false, |records| log.push(records))
            .await
        {
            Ok((next, finished)) => {
                self.log_cursor = next;
                if finished.is_some() {
                    self.log_finished = finished;
                }
            }
            Err(e) => self.message = Some((format!("{e:#}"), true)),
        }
    }

    async fn submit(&mut self, remote: &Remote<AnyDialer>, req: SubmitRequest) {
        let command = req.spec.display();
        match remote.submit(&self.target, req).await {
            Ok(job) => {
                self.message = Some((format!("queued #{}: {command}", job.id), false));
                self.history.push(command);
                self.input.clear();
                self.focus = Focus::Jobs;
                self.refresh(remote).await;
                if let Some(pos) = self.jobs.iter().position(|j| j.id == job.id) {
                    self.selected = pos;
                }
            }
            Err(e) => self.message = Some((format!("{e:#}"), true)),
        }
    }

    async fn cancel(&mut self, remote: &Remote<AnyDialer>, id: JobId) {
        match remote.cancel(&self.target, id).await {
            Ok(()) => self.message = Some((format!("cancel requested for #{id}"), false)),
            Err(e) => self.message = Some((format!("{e:#}"), true)),
        }
        self.refresh(remote).await;
    }

    /// Palette entries. Machines come first: switching target is the thing you
    /// reach for most once conduit is connected.
    /// Path with `$HOME` collapsed, for headers and messages.
    fn cwd_label(&self) -> String {
        let home = std::env::var("HOME").ok();
        let text = self.cwd.display().to_string();
        match home {
            Some(home) if text == home => "~".to_string(),
            Some(home) if text.starts_with(&format!("{home}/")) => {
                format!("~{}", &text[home.len()..])
            }
            _ => text,
        }
    }

    /// The query after `/` while the command menu should be open.
    fn slash_query(&self) -> Option<&str> {
        let rest = self.input.strip_prefix('/')?;
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
            .filter(|cmd| q.is_empty() || cmd.name.starts_with(&q))
            .collect()
    }

    fn open_picker(&mut self, title: &str, items: Vec<(String, Command)>) {
        self.picker = Some(Picker {
            title: title.to_string(),
            items,
            selected: 0,
        });
    }

    fn machine_items(&self) -> Vec<(String, Command)> {
        let mut items = Vec::new();
        for peer in self
            .status
            .as_ref()
            .map(|s| s.peers.as_slice())
            .unwrap_or(&[])
        {
            if peer.device_id == self.own_device_id() || !peer.has_service(envoy_proto::AGENT_SERVICE_ID) {
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
            items.push(("(no other agent machines online)".to_string(), Command::Refresh));
        }
        items
    }

    fn command_items(&self) -> Vec<(String, Command)> {
        let mut items = Vec::new();
        // Only machines that advertise the agent service are offered; a pure
        // VPN node would fail with `service_not_registered`, so do not send
        // the user there blindly.
        for peer in self
            .status
            .as_ref()
            .map(|s| s.peers.as_slice())
            .unwrap_or(&[])
        {
            if peer.device_id == self.own_device_id() {
                continue;
            }
            if !peer.has_service(envoy_proto::AGENT_SERVICE_ID) {
                continue;
            }
            items.push((
                format!("Switch to {} ({})", peer.name, peer.device_id),
                Command::Switch(peer.device_id.clone()),
            ));
        }
        if self.target != "local" {
            items.push(("Switch to local".to_string(), Command::Switch("local".into())));
        }
        items.extend([
            ("New job".to_string(), Command::NewJob),
            ("Cancel selected job".to_string(), Command::CancelJob),
            ("Toggle follow".to_string(), Command::ToggleFollow),
            ("Toggle sidebar".to_string(), Command::ToggleSidebar),
            ("Switch theme".to_string(), Command::NextTheme),
            ("Refresh".to_string(), Command::Refresh),
            ("Quit".to_string(), Command::Quit),
        ]);
        items
    }

    fn own_device_id(&self) -> String {
        self.status
            .as_ref()
            .map(|s| s.device_id.clone())
            .unwrap_or_default()
    }

    fn switch_to(&mut self, target: String) {
        self.target = target.clone();
        self.jobs.clear();
        self.selected = 0;
        self.log.clear();
        self.log_job = None;
        self.log_cursor = 0;
        self.log_finished = None;
        self.log_scroll = 0;
        self.message = Some((format!("target: {target}"), false));
    }

    fn run_command(&mut self, command: Command) -> Action {
        match command {
            Command::NewJob => {
                self.focus = Focus::Input;
                Action::None
            }
            Command::CancelJob => match self.selected_id() {
                Some(id) => Action::Cancel(id),
                None => Action::None,
            },
            Command::ToggleFollow => {
                self.follow = !self.follow;
                Action::None
            }
            Command::ToggleSidebar => {
                self.show_sidebar = !self.show_sidebar;
                Action::None
            }
            Command::NextTheme => {
                self.theme = (self.theme + 1) % THEMES.len();
                self.message = Some((format!("theme: {}", THEMES[self.theme].0), false));
                Action::None
            }
            Command::Refresh => Action::Refresh,
            Command::Switch(target) => Action::Switch(target),
            Command::Quit => Action::Quit,
        }
    }

    /// Run a `/command` line. Returns an `Action` for anything that needs I/O.
    fn run_slash(&mut self, text: &str) -> Action {
        let body = text.trim_start_matches('/').trim();
        let (name, arg) = match body.split_once(char::is_whitespace) {
            Some((name, arg)) => (name, arg.trim()),
            None => (body, ""),
        };
        match name {
            "cd" => {
                if arg.is_empty() {
                    self.message = Some((format!("usage: /cd <dir> (now {})", self.cwd_label()), true));
                    return Action::None;
                }
                let expanded = match arg.strip_prefix("~/") {
                    Some(tail) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(tail),
                    None => PathBuf::from(arg),
                };
                let candidate = if expanded.is_absolute() {
                    expanded
                } else {
                    self.cwd.join(expanded)
                };
                match std::fs::canonicalize(&candidate) {
                    Ok(path) if path.is_dir() => {
                        self.cwd = path;
                        self.message = Some((format!("cwd: {}", self.cwd_label()), false));
                    }
                    Ok(_) => self.message = Some((format!("not a directory: {arg}"), true)),
                    Err(e) => self.message = Some((format!("{arg}: {e}"), true)),
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
            "jobs" | "ls" => {
                self.focus = Focus::Jobs;
                Action::None
            }
            "logs" | "output" => {
                self.focus = Focus::Log;
                Action::None
            }
            "cancel" => match self.selected_id() {
                Some(id) => Action::Cancel(id),
                None => {
                    self.message = Some(("no job selected".into(), true));
                    Action::None
                }
            },
            "follow" => {
                self.follow = !self.follow;
                self.message = Some((format!("follow: {}", self.follow), false));
                Action::None
            }
            "clear" => {
                self.log.clear();
                self.message = Some(("output cleared".into(), false));
                Action::None
            }
            "theme" => {
                self.theme = (self.theme + 1) % THEMES.len();
                self.message = Some((format!("theme: {}", THEMES[self.theme].0), false));
                Action::None
            }
            "help" => {
                self.message = Some((HELP_LINE.into(), false));
                Action::None
            }
            "quit" | "q" | "exit" => Action::Quit,
            other => {
                self.message = Some((format!("unknown command: /{other} (try /help)"), true));
                Action::None
            }
        }
    }

    /// opencode-style input: `ctrl+p` palette, `ctrl+x` leader, `tab` focus,
    /// `esc` backs out, everything else acts on the focused pane.
    fn handle_key(&mut self, key: KeyEvent) -> Action {
        // Pickers capture keys first.
        if let Some(picker) = self.picker.as_mut() {
            let len = picker.items.len().max(1);
            match key.code {
                KeyCode::Esc => self.picker = None,
                KeyCode::Up | KeyCode::Char('k') => {
                    picker.selected = picker.selected.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    picker.selected = (picker.selected + 1).min(len - 1)
                }
                KeyCode::Enter => {
                    let command = picker.items[picker.selected].1.clone();
                    self.picker = None;
                    return self.run_command(command);
                }
                _ => {}
            }
            return Action::None;
        }

        // Leader prefix.
        if let Some(at) = self.leader_at.take() {
            if at.elapsed() <= LEADER_TIMEOUT {
                if let KeyCode::Char(c) = key.code {
                    let command = match c {
                        'q' => Some(Command::Quit),
                        'n' => Some(Command::NewJob),
                        'c' => Some(Command::CancelJob),
                        'f' => Some(Command::ToggleFollow),
                        'b' => Some(Command::ToggleSidebar),
                        't' => Some(Command::NextTheme),
                        's' => Some(Command::Refresh),
                        _ => None,
                    };
                    if let Some(command) = command {
                        return self.run_command(command);
                    }
                }
                return Action::None;
            }
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('d') => return Action::Quit,
                KeyCode::Char('p') => {
                    let items = self.command_items();
                    self.open_picker("commands", items);
                    return Action::None;
                }
                KeyCode::Char('x') => {
                    self.leader_at = Some(Instant::now());
                    return Action::None;
                }
                KeyCode::Char('u') => {
                    self.log_scroll = self.log_scroll.saturating_sub(self.log_height / 2);
                    self.follow = false;
                    return Action::None;
                }
                _ => {}
            }
        }

        // While the editor has focus it owns everything except the ctrl/leader
        // chords handled above (Tab completes the `/` menu, Esc backs out).
        if self.focus == Focus::Input {
            return self.handle_input_key(key);
        }

        match key.code {
            // `/` is the door to commands, from anywhere.
            KeyCode::Char('/') if self.focus != Focus::Input => {
                self.focus = Focus::Input;
                self.input = "/".into();
                self.input_cursor = 1;
                self.menu_selected = 0;
                Action::None
            }
            KeyCode::Char('n') if self.focus != Focus::Input => {
                self.focus = Focus::Input;
                Action::None
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Jobs => Focus::Log,
                    Focus::Log => Focus::Input,
                    Focus::Input => Focus::Jobs,
                };
                Action::None
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Jobs => Focus::Input,
                    Focus::Log => Focus::Jobs,
                    Focus::Input => Focus::Log,
                };
                Action::None
            }
            KeyCode::Esc => {
                if self.focus == Focus::Input {
                    self.focus = Focus::Jobs;
                } else {
                    self.follow = true;
                }
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.jobs.len() {
                    self.selected += 1;
                }
                Action::None
            }
            KeyCode::Char('g') => {
                self.selected = 0;
                Action::None
            }
            KeyCode::Char('G') => {
                self.selected = self.jobs.len().saturating_sub(1);
                Action::None
            }
            KeyCode::Char('c') => match self.selected_id() {
                Some(id) => Action::Cancel(id),
                None => Action::None,
            },
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                Action::None
            }
            KeyCode::Char('b') => {
                self.show_sidebar = !self.show_sidebar;
                Action::None
            }
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Char('q') => Action::Quit,
            KeyCode::PageUp => {
                self.follow = false;
                self.log_scroll = self.log_scroll.saturating_sub(self.log_height.max(1));
                Action::None
            }
            KeyCode::PageDown => {
                self.log_scroll = self.log_scroll.saturating_add(self.log_height.max(1));
                Action::None
            }
            KeyCode::Home => {
                self.follow = false;
                self.log_scroll = 0;
                Action::None
            }
            KeyCode::End => {
                self.follow = true;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Action {
        // While `/…` is being typed the menu owns Up/Down/Tab/Enter.
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
                KeyCode::Enter => {
                    let cmd = menu[self.menu_selected.min(last)];
                    if cmd.args.is_empty() {
                        self.input.clear();
                        self.input_cursor = 0;
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

        match key.code {
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return Action::None;
                }
                if text.starts_with('/') {
                    self.input.clear();
                    self.input_cursor = 0;
                    return self.run_slash(&text);
                }
                if self.history.last().map(|h| h != &text).unwrap_or(true) {
                    self.history.push(text.clone());
                }
                self.history_pos = None;
                self.input.clear();
                self.input_cursor = 0;
                return Action::Submit(SubmitRequest {
                    spec: TaskSpec::Shell { command: text },
                    cwd: Some(self.cwd.display().to_string()),
                    env: Vec::new(),
                    label: None,
                    group: DEFAULT_GROUP.to_string(),
                    timeout_s: 0,
                });
            }
            KeyCode::Esc => {
                self.input.clear();
                self.input_cursor = 0;
                self.focus = Focus::Jobs;
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
                if self.input_cursor < self.input.chars().count() {
                    self.input_cursor += 1;
                }
                Action::None
            }
            KeyCode::Home => {
                self.input_cursor = 0;
                Action::None
            }
            KeyCode::End => {
                self.input_cursor = self.input.chars().count();
                Action::None
            }
            KeyCode::Up => {
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
                    'a' => self.input_cursor = 0,
                    'e' => self.input_cursor = self.input.chars().count(),
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

}

// ------------------------------------------------------------------ drawing

fn draw(frame: &mut Frame, app: &mut App) {
    let theme = app.theme();
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, app, &theme, chunks[0]);
    draw_body(frame, app, &theme, chunks[1]);
    draw_editor(frame, app, &theme, chunks[2]);
    draw_footer(frame, &theme, chunks[3]);

    if app.picker.is_some() {
        draw_picker(frame, app, &theme, area);
    }
    if app.focus == Focus::Input {
        draw_slash_menu(frame, app, &theme, chunks[2]);
    }

    if app.focus == Focus::Input {
        // Keep the caret visible while typing.
        let inner = chunks[2];
        let x = inner.x + 3 + app.input_cursor as u16;
        let y = inner.y + 1;
        if x < inner.x + inner.width.saturating_sub(1) {
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
    let queued = app
        .jobs
        .iter()
        .filter(|j| matches!(j.state, JobState::Queued { .. }))
        .count();
    let line = Line::from(vec![
        Span::styled(
            " envoy ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(dot, dot_style),
        Span::styled(format!(" {} ", app.target), Style::default().fg(theme.dim)),
        Span::styled(
            format!(" {} jobs · {running} running · {queued} queued ", app.jobs.len()),
            Style::default().fg(theme.dim),
        ),
        Span::styled(
            match &app.status {
                Some(status) => {
                    let agents = status
                        .peers
                        .iter()
                        .filter(|p| p.has_service(envoy_proto::AGENT_SERVICE_ID))
                        .count();
                    format!(" {agents}/{} agents ", status.peers.len())
                }
                None => " no conduit ".to_string(),
            },
            Style::default().fg(theme.dim),
        ),
        Span::styled(
            if app.follow { "follow" } else { "paused" },
            Style::default().fg(if app.follow { theme.ok } else { theme.warn }),
        ),
        Span::styled(
            format!("  {}", THEMES[app.theme].0),
            Style::default().fg(theme.dim),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_body(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let (jobs_area, log_area) = if app.show_sidebar && area.width >= 60 {
        let halves = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(38), Constraint::Min(24)])
            .split(area);
        (Some(halves[0]), halves[1])
    } else {
        (None, area)
    };

    if let Some(jobs_area) = jobs_area {
        draw_jobs(frame, app, theme, jobs_area);
    }
    draw_log(frame, app, theme, log_area);
}

fn draw_jobs(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let items: Vec<ListItem> = app
        .jobs
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
            let command: String = job.command().chars().take(40).collect();
            ListItem::new(Line::from(vec![
                Span::styled(format!("{glyph} "), style),
                Span::styled(
                    format!("{:>3} ", job.id),
                    Style::default().fg(theme.dim),
                ),
                Span::styled(format!("{:<6} ", fmt_duration(job_elapsed(job))), style),
                Span::raw(command),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    if !app.jobs.is_empty() {
        state.select(Some(app.selected));
    }
    let items: Vec<ListItem> = if items.is_empty() {
        vec![
            ListItem::new(Line::styled(" no jobs yet", Style::default().fg(theme.dim))),
            ListItem::new(Line::styled(
                " press n to run one",
                Style::default().fg(theme.dim),
            )),
        ]
    } else {
        items
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if app.focus == Focus::Jobs {
                    theme.accent
                } else {
                    theme.border
                }))
                .title(" jobs "),
        )
        .highlight_style(Style::default().bg(theme.selection).add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_log(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let title = match app.selected_job() {
        Some(job) => {
            let state = match &job.state {
                JobState::Queued { .. } => "queued".to_string(),
                JobState::Running { .. } => format!("running {}", fmt_duration(job_elapsed(job))),
                JobState::Done { result, .. } => format!("done {}", result.describe()),
            };
            format!(" #{} · {} · {} ", job.id, job.command(), state)
        }
        None => " output ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if app.focus == Focus::Log {
            theme.accent
        } else {
            theme.border
        }))
        .title(Span::styled(title, Style::default().fg(theme.dim)));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.log_height = inner.height;

    let mut lines: Vec<Line> = app
        .log
        .lines
        .iter()
        .map(|line| {
            Line::styled(
                line.text.clone(),
                if line.err {
                    Style::default().fg(theme.bad)
                } else {
                    Style::default()
                },
            )
        })
        .collect();
    if !app.log.partial_out.is_empty() {
        lines.push(Line::raw(
            String::from_utf8_lossy(&app.log.partial_out).to_string(),
        ));
    }
    if !app.log.partial_err.is_empty() {
        lines.push(Line::styled(
            String::from_utf8_lossy(&app.log.partial_err).to_string(),
            Style::default().fg(theme.bad),
        ));
    }

    if app.selected_job().is_none() {
        // First-run screen: say what to do instead of showing an empty box.
        let daemon = if app.connected {
            "daemon ok"
        } else {
            "daemon unreachable"
        };
        let peers = match &app.status {
            Some(status) => format!("{} peer(s)", status.peers.len()),
            None => "conduit off".to_string(),
        };
        lines = vec![
            Line::from(""),
            Line::styled(
                format!("  envoy {}", env!("CARGO_PKG_VERSION")),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from(""),
            Line::styled(format!("  {}", app.cwd_label()), Style::default()),
            Line::styled(
                format!("  {} · {daemon} · {peers}", app.target),
                Style::default().fg(theme.dim),
            ),
            Line::from(""),
            Line::styled(
                "  Type a command and press enter to run it here.",
                Style::default(),
            ),
            Line::styled(
                "  Type / for commands, or ctrl+p for the palette.",
                Style::default().fg(theme.dim),
            ),
            Line::from(""),
            Line::styled(
                "  try   ls -la    uname -a    git status",
                Style::default().fg(theme.dim),
            ),
        ];
    }
    let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
    if app.follow {
        app.log_scroll = max_scroll;
    } else {
        app.log_scroll = app.log_scroll.min(max_scroll);
    }
    frame.render_widget(Paragraph::new(lines).scroll((app.log_scroll, 0)), inner);
}

fn draw_editor(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if app.focus == Focus::Input {
            theme.accent
        } else {
            theme.border
        }))
        .title(Span::styled(
            match &app.message {
                Some((message, _)) => format!(" command · {message} "),
                None => " command  (/ for commands) ".to_string(),
            },
            Style::default().fg(match &app.message {
                Some((_, true)) => theme.bad,
                Some((_, false)) => theme.ok,
                None if app.focus == Focus::Input => theme.accent,
                None => theme.dim,
            }),
        ));
    let line = Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.accent)),
        Span::raw(app.input.clone()),
        Span::styled(
            if app.focus == Focus::Input {
                ""
            } else {
                "  tab to type"
            },
            Style::default().fg(theme.dim),
        ),
    ]);
    frame.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_footer(frame: &mut Frame, theme: &Theme, area: Rect) {
    let hints = Line::from(vec![
        Span::styled(" enter ", Style::default().fg(theme.accent)),
        Span::styled("run ", Style::default().fg(theme.dim)),
        Span::styled("/ ", Style::default().fg(theme.accent)),
        Span::styled("commands ", Style::default().fg(theme.dim)),
        Span::styled("ctrl+p ", Style::default().fg(theme.accent)),
        Span::styled("palette ", Style::default().fg(theme.dim)),
        Span::styled("tab ", Style::default().fg(theme.accent)),
        Span::styled("focus ", Style::default().fg(theme.dim)),
        Span::styled("ctrl+x q ", Style::default().fg(theme.accent)),
        Span::styled("quit", Style::default().fg(theme.dim)),
    ]);
    frame.render_widget(Paragraph::new(hints), area);
}

fn draw_picker(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(picker) = app.picker.as_ref() else {
        return;
    };
    let width = 52u16.min(area.width.saturating_sub(4));
    let height = (picker.items.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let list_items: Vec<ListItem> = picker
        .items
        .iter()
        .map(|(label, _)| ListItem::new(Line::raw(format!(" {label}"))))
        .collect();
    let mut state = ListState::default();
    state.select(Some(picker.selected));
    let list = List::new(list_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {} ", picker.title),
                    Style::default().fg(theme.accent),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, popup, &mut state);
}

/// The `/` menu, anchored just above the editor.
fn draw_slash_menu(frame: &mut Frame, app: &App, theme: &Theme, editor: Rect) {
    let items = app.filtered_slash();
    if items.is_empty() {
        return;
    }
    let height = (items.len() as u16 + 2)
        .min(editor.y.saturating_sub(1))
        .min(12);
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
                format!("/{:<10} {:<8} {}", cmd.name, cmd.args, cmd.about)
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
                .title(Span::styled(
                    " commands ",
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

// ------------------------------------------------------------------ helpers

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
}

impl Tui {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        Ok(Self { terminal })
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

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
    fn log_buffer_splits_lines_and_keeps_stderr_separate() {
        let mut log = LogBuffer::default();
        log.push(&[LogRecord {
            seq: 0,
            fd: 0,
            data: b"one\ntw".to_vec(),
        }]);
        log.push(&[LogRecord {
            seq: 9,
            fd: 1,
            data: b"err\n".to_vec(),
        }]);
        log.push(&[LogRecord {
            seq: 18,
            fd: 0,
            data: b"o\n".to_vec(),
        }]);
        let lines: Vec<(&str, bool)> = log
            .lines
            .iter()
            .map(|l| (l.text.as_str(), l.err))
            .collect();
        assert_eq!(lines, vec![("one", false), ("err", true), ("two", false)]);
    }

    #[test]
    fn durations_are_human() {
        assert_eq!(fmt_duration(999), "0s");
        assert_eq!(fmt_duration(61_000), "1m01s");
        assert_eq!(fmt_duration(3_700_000), "1h01m");
    }

    fn tmpdir() -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-tui-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn slash_menu_filters_and_runs() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "local".into(),
            None,
            PathBuf::from("/tmp"),
        );
        app.focus = Focus::Input;
        app.input = "/lo".into();
        app.input_cursor = 3;

        let names: Vec<&str> = app.filtered_slash().iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["logs"]);

        // Tab completes instead of running.
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "/logs");

        // Enter on the full command executes it.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Log);
        assert!(app.input.is_empty());
    }

    #[test]
    fn slash_cd_changes_the_working_directory() {
        let dir = tmpdir();
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "local".into(),
            None,
            PathBuf::from("/tmp"),
        );
        app.run_slash(&format!("/cd {}", dir.display()));
        assert_eq!(app.cwd, std::fs::canonicalize(&dir).unwrap());

        app.run_slash("/cd /definitely/not/here");
        assert_eq!(app.cwd, std::fs::canonicalize(&dir).unwrap());
        assert!(matches!(app.message, Some((_, true))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_submits_in_the_current_directory() {
        let dir = tmpdir();
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "local".into(),
            None,
            dir.clone(),
        );
        app.focus = Focus::Input;
        app.input = "echo hi".into();
        app.input_cursor = 7;
        match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            Action::Submit(req) => {
                assert_eq!(req.spec.display(), "echo hi");
                assert_eq!(req.cwd.as_deref(), Some(dir.display().to_string().as_str()));
            }
            _ => panic!("enter should submit"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_empty_screen_teaches_the_first_step() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(
            PathBuf::from("/tmp"),
            "local".into(),
            None,
            PathBuf::from("/tmp/project"),
        );
        app.connected = true;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let version = format!("envoy {}", env!("CARGO_PKG_VERSION"));
        for needle in [version.as_str(), "/tmp/project", "Type / for commands", "ctrl+p"] {
            assert!(text.contains(needle), "missing {needle:?}:\n{text}");
        }
    }

    #[test]
    fn leader_requires_the_prefix() {
        let mut app = App::new(PathBuf::from("/tmp"), "local".into(), None, PathBuf::from("/tmp"));
        let leader = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(matches!(app.handle_key(leader), Action::None));
        let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(app.handle_key(quit), Action::Quit));
    }

    #[test]
    fn typing_only_happens_in_the_input_focus() {
        let mut app = App::new(PathBuf::from("/tmp"), "local".into(), None, PathBuf::from("/tmp"));
        // `n` focuses the editor instead of inserting.
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Input);
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.input, "ls");
        match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            Action::Submit(req) => assert_eq!(req.spec.display(), "ls"),
            _ => panic!("enter should submit the command"),
        }
    }

    #[test]
    fn palette_enter_runs_the_selected_command() {
        let mut app = App::new(PathBuf::from("/tmp"), "local".into(), None, PathBuf::from("/tmp"));
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(app.picker.is_some());
        for _ in 0..6 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Quit
        ));
    }

    #[test]
    fn renders_header_jobs_and_hints() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(PathBuf::from("/tmp"), "local".into(), None, PathBuf::from("/tmp"));
        app.connected = true;
        app.jobs = vec![
            job(
                1,
                JobState::Done {
                    at_ms: now_ms(),
                    result: envoy_proto::JobResult::Success,
                },
            ),
            job(2, JobState::Running { since_ms: now_ms() }),
        ];
        app.selected = 1;
        app.message = Some(("queued #2: echo 2".into(), false));

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        for needle in ["envoy", "local", "jobs", "echo 1", "echo 2", "command", "ctrl+p", "quit"] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn palette_lists_other_machines_and_switches_target() {
        let mut app = App::new(PathBuf::from("/tmp"), "local".into(), None, PathBuf::from("/tmp"));
        app.status = Some(Status {
            device_id: "me".into(),
            device_name: "workstation".into(),
            peers: vec![
                envoy_service::conduit::Peer {
                    device_id: "other".into(),
                    name: "laptop".into(),
                    services: vec![envoy_service::conduit::PeerService {
                        service_id: 16,
                        name: "envoy".into(),
                    }],
                    ..Default::default()
                },
                envoy_service::conduit::Peer {
                    device_id: "vpn".into(),
                    name: "router".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });

        let items = app.command_items();
        assert!(
            items.iter().any(|(label, _)| label.contains("laptop")),
            "agent peer should be offered: {:?}",
            items.iter().map(|(label, _)| label).collect::<Vec<_>>()
        );
        assert!(
            !items.iter().any(|(label, _)| label.contains("router")),
            "a peer without service 16 must not be offered"
        );
        assert!(
            !items.iter().any(|(label, _)| label.contains("workstation")),
            "this device must not be a target"
        );

        app.jobs = vec![job(1, JobState::Running { since_ms: now_ms() })];
        app.switch_to("other".into());
        assert_eq!(app.target, "other");
        assert!(app.jobs.is_empty(), "switching must drop the old job list");
        assert_eq!(app.log_cursor, 0);
    }

    #[test]
    fn selection_survives_refresh_order() {
        let mut app = App::new(PathBuf::from("/tmp"), "local".into(), None, PathBuf::from("/tmp"));
        app.jobs = vec![
            job(1, JobState::Done {
                at_ms: now_ms(),
                result: envoy_proto::JobResult::Success,
            }),
            job(2, JobState::Running { since_ms: now_ms() }),
            job(3, JobState::Queued { since_ms: now_ms() }),
        ];
        app.selected = 1;
        assert_eq!(app.selected_id(), Some(2));
    }
}
