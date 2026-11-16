//! Console state: input, sessions, login, and the agent event stream.
use super::*;

mod agent;
mod commands;
mod input;
mod jobs;
mod login;
mod modals;

impl App {
    pub(in crate::tui) fn new(data_dir: PathBuf, target: String, conduit: Option<PathBuf>, cwd: PathBuf) -> Self {
        let prefs = load_prefs(&data_dir);
        let has_provider = resolve_active_provider(&data_dir).is_some();
        let mode = if has_provider {
            InputMode::Agent
        } else {
            InputMode::Shell
        };
        let session = Session::new(cwd, target.clone());
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
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
            agent_stream: None,
            agent_retry_at: Instant::now(),
            agent_offline: false,
            agent_ready: false,
            pending_ask: None,
            queued: VecDeque::new(),
            outbox: VecDeque::new(),
            notice_tx,
            notice_rx,
            active_assistant_id: None,
            active_tool_items: HashMap::new(),
            shell_watches: Vec::new(),
            login_rx: None,
            login_task: None,
            login_cancel: None,
            models_rx: None,
        }
    }


    pub(in crate::tui) fn theme(&self) -> Theme {
        THEMES[self.theme].1
    }


    pub(in crate::tui) fn load_existing_session(&mut self, loaded: Session) {
        self.persist_session();
        if loaded.cwd.is_dir() {
            self.session_cwd_set(loaded.cwd.clone());
        }
        self.session = loaded;
        self.selected_card = None;
        self.follow = true;
        self.reset_agent_view();
    }


    pub(in crate::tui) fn reset_agent_view(&mut self) {
        self.agent_running = false;
        self.active_assistant_id = None;
        self.active_tool_items.clear();
        self.pending_ask = None;
        self.queued.clear();
        self.outbox.clear();
        self.agent_stream = None;
        self.agent_ready = false;
        self.agent_retry_at = Instant::now();
    }


    pub(in crate::tui) fn session_cwd_set(&mut self, cwd: PathBuf) {
        self.session.cwd = cwd;
    }


    pub(in crate::tui) fn persist_session(&self) {
        let _ = save_session(&self.data_dir, &self.session);
    }


    pub(in crate::tui) fn save_ui_prefs(&self) {
        let mut prefs = load_prefs(&self.data_dir);
        prefs.theme = self.theme;
        prefs.show_sidebar = self.show_sidebar;
        let _ = save_prefs(&self.data_dir, &prefs);
    }


    pub(in crate::tui) fn set_toast(&mut self, msg: impl Into<String>, err: bool) {
        self.toast = Some((msg.into(), err, Instant::now()));
    }


    pub(in crate::tui) fn active_toast(&self) -> Option<(&str, bool)> {
        let (msg, err, at) = self.toast.as_ref()?;
        if at.elapsed() < Duration::from_secs(6) {
            Some((msg.as_str(), *err))
        } else {
            None
        }
    }


    pub(in crate::tui) fn push_notice(&mut self, text: impl Into<String>, is_error: bool) {
        let id = self.session.alloc_id();
        self.session.items.push(TranscriptItem::Notice {
            id,
            text: text.into(),
            is_error,
        });
        self.session.touch();
        self.follow = true;
    }


    pub(in crate::tui) fn cwd_label(&self) -> String {
        collapse_home(&self.session.cwd)
    }


    pub(in crate::tui) fn own_device_id(&self) -> String {
        self.status
            .as_ref()
            .map(|s| s.device_id.clone())
            .unwrap_or_default()
    }


    pub(in crate::tui) fn switch_to(&mut self, target: String) {
        self.target = target.clone();
        self.session.target = target.clone();
        self.jobs.clear();
        self.job_selected = 0;
        self.shell_watches.clear();
        self.set_toast(format!("target: {target}"), false);
    }
}
