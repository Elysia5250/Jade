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
use envoy_service::{default_control_socket, dialer_for, AgentDialer, ConduitClient, Device, Remote, Status};
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

pub async fn run(data_dir: PathBuf, target: String, conduit: Option<PathBuf>) -> Result<()> {
    if !stdout().is_terminal() {
        bail!("`envoy ui` needs a terminal (try `envoy jobs` for scripts)");
    }
    if target == "local" || target.is_empty() {
        daemon::ensure(&data_dir).await?;
    }
    let mut remote: Remote<AnyDialer> =
        Remote::new(dialer_for(&data_dir, &target, conduit.clone()));

    let mut app = App::new(data_dir.clone(), target, conduit.clone());
    let client = ConduitClient::new(conduit.clone().unwrap_or_else(default_control_socket));
    if let Ok(devices) = client.devices().await {
        app.devices = devices.devices;
    }
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

struct Palette {
    selected: usize,
}

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
    target: String,
    conduit: Option<PathBuf>,
    devices: Vec<Device>,
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
    history: Vec<String>,
    history_pos: Option<usize>,
    palette: Option<Palette>,
    theme: usize,
    leader_at: Option<Instant>,
    message: Option<(String, bool)>,
    connected: bool,
    interval: Duration,
    last_refresh: Instant,
}

impl App {
    fn new(data_dir: PathBuf, target: String, conduit: Option<PathBuf>) -> Self {
        Self {
            data_dir,
            target,
            conduit,
            devices: Vec::new(),
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
            history: Vec::new(),
            history_pos: None,
            palette: None,
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
    fn palette_items(&self) -> Vec<(String, Command)> {
        let mut items = Vec::new();
        for device in &self.devices {
            if device.device_id == self.own_device_id() {
                continue;
            }
            let online = self
                .status
                .as_ref()
                .map(|s| s.peers.iter().any(|p| p.device_id == device.device_id))
                .unwrap_or(false);
            items.push((
                format!(
                    "Switch to {} ({}){}",
                    device.name,
                    device.device_id,
                    if online { "" } else { " · offline" }
                ),
                Command::Switch(device.device_id.clone()),
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

    /// opencode-style input: `ctrl+p` palette, `ctrl+x` leader, `tab` focus,
    /// `esc` backs out, everything else acts on the focused pane.
    fn handle_key(&mut self, key: KeyEvent) -> Action {
        // Palette and dialogs capture keys first.
        if self.palette.is_some() {
            let items = self.palette_items();
            let palette = self.palette.as_mut().expect("checked above");
            match key.code {
                KeyCode::Esc => self.palette = None,
                KeyCode::Up | KeyCode::Char('k') => {
                    palette.selected = palette.selected.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    palette.selected = (palette.selected + 1).min(items.len() - 1)
                }
                KeyCode::Enter => {
                    let command = items[palette.selected].1.clone();
                    self.palette = None;
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
                    self.palette = Some(Palette { selected: 0 });
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

        match key.code {
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
            _ if self.focus == Focus::Input => self.handle_input_key(key),
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
        match key.code {
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return Action::None;
                }
                return Action::Submit(SubmitRequest {
                    spec: TaskSpec::Shell { command: text },
                    cwd: None,
                    env: Vec::new(),
                    label: None,
                    group: DEFAULT_GROUP.to_string(),
                    timeout_s: 0,
                });
            }
            KeyCode::Esc => {
                self.input.clear();
                self.focus = Focus::Jobs;
                Action::None
            }
            KeyCode::Backspace => {
                self.input.pop();
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
                }
                Action::None
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                Action::None
            }
            _ => Action::None,
        }
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

    if app.palette.is_some() {
        draw_palette(frame, app, &theme, area);
    }

    if app.focus == Focus::Input {
        // Keep the caret visible while typing.
        let inner = chunks[2];
        let x = inner.x + 3 + app.input.chars().count() as u16;
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
                Some(status) => format!(" {} peer(s) ", status.peers.len()),
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
        lines.push(Line::styled(
            "select a job with j / k",
            Style::default().fg(theme.dim),
        ));
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
                None => " command ".to_string(),
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
    let hints = {
        Line::from(vec![
            Span::styled(" ctrl+p ", Style::default().fg(theme.accent)),
            Span::styled("palette ", Style::default().fg(theme.dim)),
            Span::styled("n ", Style::default().fg(theme.accent)),
            Span::styled("new ", Style::default().fg(theme.dim)),
            Span::styled("c ", Style::default().fg(theme.accent)),
            Span::styled("cancel ", Style::default().fg(theme.dim)),
            Span::styled("f ", Style::default().fg(theme.accent)),
            Span::styled("follow ", Style::default().fg(theme.dim)),
            Span::styled("tab ", Style::default().fg(theme.accent)),
            Span::styled("focus ", Style::default().fg(theme.dim)),
            Span::styled("ctrl+x q ", Style::default().fg(theme.accent)),
            Span::styled("quit", Style::default().fg(theme.dim)),
        ])
    };
    frame.render_widget(Paragraph::new(hints), area);
}

fn draw_palette(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let items = app.palette_items();
    let width = 44u16.min(area.width.saturating_sub(4));
    let height = items.len() as u16 + 2;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let list_items: Vec<ListItem> = items
        .iter()
        .map(|(label, _)| ListItem::new(Line::raw(format!(" {label}"))))
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.palette.as_ref().map(|p| p.selected).unwrap_or(0)));
    let list = List::new(list_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(" commands "),
        )
        .highlight_style(Style::default().bg(theme.selection).add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, popup, &mut state);
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

    #[test]
    fn leader_requires_the_prefix() {
        let mut app = App::new(PathBuf::from("/tmp"), "local".into(), None);
        let leader = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(matches!(app.handle_key(leader), Action::None));
        let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(app.handle_key(quit), Action::Quit));
    }

    #[test]
    fn typing_only_happens_in_the_input_focus() {
        let mut app = App::new(PathBuf::from("/tmp"), "local".into(), None);
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
        let mut app = App::new(PathBuf::from("/tmp"), "local".into(), None);
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(app.palette.is_some());
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

        let mut app = App::new(PathBuf::from("/tmp"), "local".into(), None);
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
        let mut app = App::new(PathBuf::from("/tmp"), "local".into(), None);
        app.status = Some(Status {
            device_id: "me".into(),
            device_name: "workstation".into(),
            peers: vec![envoy_service::conduit::Peer {
                device_id: "other".into(),
                name: "laptop".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        app.devices = vec![
            Device {
                device_id: "me".into(),
                name: "workstation".into(),
                dns_name: None,
                virtual_ip: None,
                status: "active".into(),
                me: true,
            },
            Device {
                device_id: "other".into(),
                name: "laptop".into(),
                dns_name: None,
                virtual_ip: None,
                status: "active".into(),
                me: false,
            },
        ];

        let items = app.palette_items();
        assert!(
            items.iter().any(|(label, _)| label.contains("laptop")),
            "peer should be offered: {:?}",
            items.iter().map(|(label, _)| label).collect::<Vec<_>>()
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
        let mut app = App::new(PathBuf::from("/tmp"), "local".into(), None);
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
