//! Console state: input, sessions, login, and the agent event stream.
use super::*;

impl App {
    pub(super) fn new(data_dir: PathBuf, target: String, conduit: Option<PathBuf>, cwd: PathBuf) -> Self {
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

    pub(super) fn theme(&self) -> Theme {
        THEMES[self.theme].1
    }

    pub(super) fn load_existing_session(&mut self, loaded: Session) {
        self.persist_session();
        if loaded.cwd.is_dir() {
            self.session_cwd_set(loaded.cwd.clone());
        }
        self.session = loaded;
        self.selected_card = None;
        self.follow = true;
        self.reset_agent_view();
    }

    pub(super) fn reset_agent_view(&mut self) {
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

    /// Connect (or reconnect) to the daemon session. The replay cursor comes
    /// from the last event applied to the local transcript.
    pub(super) fn spawn_agent_attach(&mut self) {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(runtime::attach(
            self.data_dir.clone(),
            self.session.id.clone(),
            self.session.cwd.clone(),
            self.target.clone(),
            self.session.messages.clone(),
            self.session.event_seq,
            tx,
        ));
        self.agent_stream = Some(rx);
        self.agent_ready = false;
        self.agent_retry_at = Instant::now() + Duration::from_secs(2);
    }

    /// Rebuild the in-flight view after an attach. Events before the cursor
    /// are not replayed, so find the open assistant/tool items in the
    /// persisted transcript.
    pub(super) fn resume_active_view(&mut self) {
        self.active_assistant_id = self
            .session
            .items
            .iter()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::Assistant { id, done: false, .. } => Some(*id),
                _ => None,
            });
        self.active_tool_items.clear();
        for item in &self.session.items {
            if let TranscriptItem::Tool {
                id,
                call_id,
                status: CardStatus::Running,
                ..
            } = item
            {
                self.active_tool_items.insert(call_id.clone(), *id);
            }
        }
    }

    pub(super) fn session_cwd_set(&mut self, cwd: PathBuf) {
        self.session.cwd = cwd;
    }

    pub(super) fn persist_session(&self) {
        let _ = save_session(&self.data_dir, &self.session);
    }

    pub(super) fn save_ui_prefs(&self) {
        let mut prefs = load_prefs(&self.data_dir);
        prefs.theme = self.theme;
        prefs.show_sidebar = self.show_sidebar;
        let _ = save_prefs(&self.data_dir, &prefs);
    }

    pub(super) fn set_toast(&mut self, msg: impl Into<String>, err: bool) {
        self.toast = Some((msg.into(), err, Instant::now()));
    }

    pub(super) fn active_toast(&self) -> Option<(&str, bool)> {
        let (msg, err, at) = self.toast.as_ref()?;
        if at.elapsed() < Duration::from_secs(6) {
            Some((msg.as_str(), *err))
        } else {
            None
        }
    }

    pub(super) fn push_notice(&mut self, text: impl Into<String>, is_error: bool) {
        let id = self.session.alloc_id();
        self.session.items.push(TranscriptItem::Notice {
            id,
            text: text.into(),
            is_error,
        });
        self.session.touch();
        self.follow = true;
    }

    pub(super) fn cwd_label(&self) -> String {
        collapse_home(&self.session.cwd)
    }

    pub(super) fn own_device_id(&self) -> String {
        self.status
            .as_ref()
            .map(|s| s.device_id.clone())
            .unwrap_or_default()
    }

    pub(super) async fn refresh(&mut self, remote: &Remote<AnyDialer>) {
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
            Err(_) => {
                // Keep the console usable while the daemon is down: the
                // status line carries the state, and a toast every refresh
                // would stomp on whatever the user is doing.
                self.connected = false;
            }
        }
    }

    pub(super) async fn submit_shell(&mut self, remote: &Remote<AnyDialer>, req: SubmitRequest) {
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
                    collapsed: true,
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

    pub(super) async fn attach_job_card(&mut self, remote: &Remote<AnyDialer>, job: Job) {
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

    pub(super) async fn poll_shell_cards(&mut self, remote: &Remote<AnyDialer>) {
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

    pub(super) async fn poll_one_watch(
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

    /// Send while connected, otherwise hold the prompt until an attach
    /// succeeds so a daemon restart does not eat what the user typed.
    pub(super) fn send_agent(&mut self, text: String, mode: SendMode) {
        if !self.agent_ready || self.agent_offline || self.agent_stream.is_none() {
            self.outbox.push_back((text, mode));
            if self.agent_offline || self.agent_stream.is_none() {
                self.set_toast("daemon offline — prompt queued", true);
            }
            return;
        }
        self.dispatch_agent(vec![(text, mode)]);
    }

    pub(super) fn dispatch_agent(&self, items: Vec<(String, SendMode)>) {
        if items.is_empty() {
            return;
        }
        let data_dir = self.data_dir.clone();
        let id = self.session.id.clone();
        let notice = self.notice_tx.clone();
        tokio::spawn(async move {
            let mut items = items;
            while let Some((text, mode)) = items.first().cloned() {
                let result = runtime::tell(
                    &data_dir,
                    AgentReq::Send {
                        id: id.clone(),
                        text,
                        mode,
                    },
                )
                .await;
                if result.is_err() {
                    let _ = notice.send(UiNotice::Requeue(items));
                    return;
                }
                items.remove(0);
            }
        });
    }

    pub(super) fn flush_outbox(&mut self) {
        if self.outbox.is_empty() {
            return;
        }
        let items: Vec<(String, SendMode)> = self.outbox.drain(..).collect();
        self.dispatch_agent(items);
    }

    pub(super) fn remove_outbox(&mut self, text: &str) {
        if let Some(pos) = self.outbox.iter().rposition(|(t, _)| t == text) {
            self.outbox.remove(pos);
        }
    }

    pub(super) fn recall_queued(&self) {
        if self.agent_offline || self.agent_stream.is_none() {
            return;
        }
        let data_dir = self.data_dir.clone();
        let id = self.session.id.clone();
        let notice = self.notice_tx.clone();
        tokio::spawn(async move {
            let recalled = match runtime::request(&data_dir, &AgentReq::Recall { id }).await {
                Ok(AgentResp::Recalled(text)) => text,
                _ => None,
            };
            let _ = notice.send(UiNotice::Recalled(recalled));
        });
    }

    pub(super) fn drain_ui_notices(&mut self) {
        let mut notices = Vec::new();
        while let Ok(notice) = self.notice_rx.try_recv() {
            notices.push(notice);
        }
        for notice in notices {
            match notice {
                UiNotice::Requeue(items) => {
                    self.agent_offline = true;
                    self.agent_ready = false;
                    self.agent_stream = None;
                    self.agent_retry_at = Instant::now() + Duration::from_millis(750);
                    for item in items.into_iter().rev() {
                        self.outbox.push_front(item);
                    }
                    self.set_toast("daemon offline — prompt queued", true);
                }
                UiNotice::Recalled(Some(text)) => {
                    // The daemon's copy is authoritative; this also catches
                    // follow-ups queued by an earlier console.
                    self.input = text;
                    self.input_cursor = self.input.chars().count();
                    self.focus = Focus::Input;
                }
                UiNotice::Recalled(None) => {}
            }
        }
    }

    pub(super) fn start_agent_turn(&mut self, prompt: String) {
        self.follow = true;
        if !self.agent_offline && self.agent_stream.is_some() {
            self.agent_running = true;
        }
        self.send_agent(prompt, SendMode::Prompt);
    }

    pub(super) fn drain_agent_events(&mut self) {
        let mut disconnected = false;
        loop {
            let message = {
                let Some(rx) = self.agent_stream.as_mut() else {
                    return;
                };
                match rx.try_recv() {
                    Ok(message) => message,
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            };
            match message {
                AgentResp::Snapshot { events, running } => {
                    for item in events {
                        self.apply_event(item.seq, item.event);
                    }
                    // The daemon is authoritative: a console that missed a
                    // TurnDone must not stay stuck in "running".
                    self.agent_running = running;
                    self.agent_offline = false;
                    self.agent_ready = true;
                    if running {
                        self.resume_active_view();
                    }
                    if !self.outbox.is_empty() {
                        self.set_toast("daemon back — sending queued prompt", false);
                        self.flush_outbox();
                    }
                }
                AgentResp::Event(item) => self.apply_event(item.seq, item.event),
                AgentResp::Error(err) => {
                    let first = !self.agent_offline;
                    self.agent_offline = true;
                    self.agent_ready = false;
                    self.agent_running = false;
                    if first {
                        self.set_toast(err, true);
                    }
                }
                _ => {}
            }
        }
        if disconnected {
            self.agent_stream = None;
            self.agent_ready = false;
            self.agent_running = false;
            self.agent_retry_at = Instant::now() + Duration::from_millis(750);
            if !self.agent_offline {
                self.agent_offline = true;
                self.set_toast("agent stream lost — reconnecting…", true);
            }
        }
    }

    pub(super) fn apply_event(&mut self, seq: u64, event: AgentEvent) {
        self.session.event_seq = seq;
        match event {
            AgentEvent::SessionReset => {
                // The daemon restarted: its sequence numbers restarted too.
                // Keep the local transcript (it is history), just re-base the
                // cursor so the new events apply.
                self.session.event_seq = 0;
                self.active_assistant_id = None;
                self.active_tool_items.clear();
                self.pending_ask = None;
            }
            AgentEvent::UserMessage { text, mode } => {
                if mode != "steer" && self.queued.front().map(|q| q == &text).unwrap_or(false) {
                    self.queued.pop_front();
                }
                let id = self.session.alloc_id();
                self.session.items.push(TranscriptItem::User {
                    id,
                    text,
                    mode,
                    cwd: self.cwd_label(),
                    ts_ms: now_ms(),
                });
                self.session.touch();
                self.follow = true;
                self.agent_running = true;
            }
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
                    collapsed: true,
                    duration_ms: None,
                    exit_code: None,
                    started_ms: now_ms(),
                });
                self.active_tool_items.insert(call_id, card_id);
                self.session.touch();
                self.persist_session();
            }
            AgentEvent::ToolJob { call_id, job_id } => {
                if let Some(&item_id) = self.active_tool_items.get(&call_id) {
                    if let Some(TranscriptItem::Tool { job_id: slot, .. }) =
                        self.session.items.iter_mut().find(|i| i.id() == item_id)
                    {
                        *slot = Some(job_id);
                    }
                }
            }
            AgentEvent::ToolOutput { call_id, text, err } => {
                if let Some(&item_id) = self.active_tool_items.get(&call_id) {
                    if let Some(item) = self.session.items.iter_mut().find(|i| i.id() == item_id) {
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
                        *status = if ok { CardStatus::Done } else { CardStatus::Error };
                        *dur = Some(duration_ms);
                        *code = exit_code;
                        if ok && name != "exec" && output.len() > 14 {
                            *collapsed = true;
                        }
                    }
                }
                self.session.touch();
                self.persist_session();
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
                context_tokens,
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
                if context_tokens > 0 {
                    self.session.prompt_tokens = context_tokens;
                }
                self.session.touch();
                self.persist_session();
                self.agent_running = false;
                self.active_assistant_id = None;
                self.active_tool_items.clear();
                self.pending_ask = None;
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
                self.agent_running = false;
                self.active_assistant_id = None;
                self.active_tool_items.clear();
                self.pending_ask = None;
            }
        }
    }

    /// Enter while idle starts a turn; while running it steers the loop with
    /// the user's exact words at the next tool boundary.
    pub(super) fn submit_agent(&mut self, prompt: String) {
        let was_running = self.agent_running;
        // Always `Prompt`: the daemon starts a turn when idle and steers the
        // live one otherwise. Keeping the decision on the server side means a
        // stale console flag cannot swallow the next task.
        self.start_agent_turn(prompt);
        if was_running {
            self.set_toast("steer queued for the next tool boundary", false);
        }
    }

    /// Alt+Enter: deliver after the current run finishes.
    pub(super) fn queue_followup(&mut self, prompt: String) {
        self.send_agent(prompt.clone(), SendMode::FollowUp);
        self.queued.push_back(prompt);
        self.set_toast("follow-up queued", false);
    }

    pub(super) fn answer_ask(&self, ask_id: u64, label: String) {
        let data_dir = self.data_dir.clone();
        let id = self.session.id.clone();
        tokio::spawn(async move {
            let _ = runtime::tell(
                &data_dir,
                AgentReq::Answer {
                    id,
                    ask_id,
                    label,
                },
            )
            .await;
        });
    }

    pub(super) fn handle_ask_key(&mut self, key: KeyEvent) -> Action {
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
                self.answer_ask(ask.id, label.clone());
                self.set_toast(format!("chose: {label}"), false);
            }
            KeyCode::Esc => {
                if let Some(option) = ask.options.get(ask.recommended) {
                    self.answer_ask(ask.id, option.label.clone());
                    self.set_toast(format!("chose recommended: {}", option.label), false);
                }
            }
            _ => self.pending_ask = Some(ask),
        }
        Action::None
    }

    pub(super) fn interrupt_agent(&self) {
        let data_dir = self.data_dir.clone();
        let id = self.session.id.clone();
        tokio::spawn(async move {
            let _ = runtime::tell(&data_dir, AgentReq::Interrupt { id }).await;
        });
    }

    pub(super) async fn cancel_active(&mut self, remote: &Remote<AnyDialer>) {
        if self.agent_running {
            self.interrupt_agent();
            self.agent_running = false;
            self.pending_ask = None;
            // The daemon publishes the authoritative TurnError; just stop the
            // local spinner and leave the id in place so that event lands on
            // the open assistant item.
            if let Some(id) = self.active_assistant_id {
                if let Some(TranscriptItem::Assistant { done, .. }) =
                    self.session.items.iter_mut().find(|i| i.id() == id)
                {
                    *done = true;
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

    pub(super) async fn cancel_job(&mut self, remote: &Remote<AnyDialer>, id: JobId) {
        match remote.cancel(&self.target, id).await {
            Ok(()) => self.set_toast(format!("cancel requested for #{id}"), false),
            Err(e) => self.set_toast(format!("{e:#}"), true),
        }
        self.refresh(remote).await;
    }

    pub(super) fn switch_to(&mut self, target: String) {
        self.target = target.clone();
        self.session.target = target.clone();
        self.jobs.clear();
        self.job_selected = 0;
        self.shell_watches.clear();
        self.set_toast(format!("target: {target}"), false);
    }

    // ------------------------------------------------------------------ slash & autocomplete

    /// Returns either matching slash commands OR `/cd` directory completions.
    pub(super) fn slash_query(&self) -> Option<&str> {
        if !self.input.starts_with('/') || self.input.contains('\n') {
            return None;
        }
        let rest = &self.input[1..];
        if rest.contains(char::is_whitespace) {
            return None;
        }
        Some(rest)
    }

    pub(super) fn filtered_slash(&self) -> Vec<&'static SlashCommand> {
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

    pub(super) fn cd_query(&self) -> Option<&str> {
        if self.input.contains('\n') {
            return None;
        }
        self.input.strip_prefix("/cd ")
    }

    pub(super) fn cd_completions(&self) -> Vec<(String, String)> {
        let Some(raw) = self.cd_query() else {
            return Vec::new();
        };
        complete_directories(&self.session.cwd, raw)
    }

    // ------------------------------------------------------------------ /login flow

    pub(super) fn open_login_modal(&mut self) {
        self.cancel_login_bg();
        self.modal = Some(Modal::Login(LoginModal::SelectProvider {
            filter: String::new(),
            selected: 0,
        }));
    }

    pub(super) fn filtered_login_providers(&self, filter: &str) -> Vec<(usize, &'static crate::auth::BuiltinProvider)> {
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

    pub(super) fn cancel_login_bg(&mut self) {
        if let Some(tx) = self.login_cancel.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.login_task.take() {
            h.abort();
        }
        self.login_rx = None;
    }

    pub(super) fn start_login_method(&mut self, provider_idx: usize, method_idx: usize) {
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

    pub(super) fn drain_login_events(&mut self) {
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

    pub(super) fn open_models_modal(&mut self) {
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

    pub(super) fn drain_model_events(&mut self) {
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

    pub(super) fn open_sessions_modal(&mut self) {
        self.persist_session();
        let sessions = list_sessions(&self.data_dir);
        self.modal = Some(Modal::Sessions(SessionsModal {
            sessions,
            selected: 0,
        }));
    }

    // ------------------------------------------------------------------ command & palette

    pub(super) fn open_picker(&mut self, title: &str, items: Vec<(String, Command)>) {
        self.modal = Some(Modal::Picker(Picker {
            title: title.to_string(),
            query: String::new(),
            items,
            selected: 0,
        }));
    }

    pub(super) fn machine_items(&self) -> Vec<(String, Command)> {
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

    pub(super) fn command_items(&self) -> Vec<(String, Command)> {
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

    pub(super) fn run_command(&mut self, command: Command) -> Action {
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
                self.reset_agent_view();
                self.spawn_agent_attach();
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

    pub(super) fn toggle_expand_cards(&mut self) {
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
        // Otherwise fold/unfold every tool card and reasoning block.
        let any_collapsed = self.session.items.iter().any(|i| {
            matches!(i, TranscriptItem::Tool { collapsed: true, .. })
                || matches!(
                    i,
                    TranscriptItem::Assistant {
                        reasoning_collapsed: true,
                        ..
                    }
                )
        });
        let target_state = !any_collapsed;
        for item in &mut self.session.items {
            match item {
                TranscriptItem::Tool { collapsed, .. } => *collapsed = target_state,
                TranscriptItem::Assistant {
                    reasoning_collapsed,
                    ..
                } => *reasoning_collapsed = target_state,
                _ => {}
            }
        }
        self.persist_session();
    }

    pub(super) fn run_slash(&mut self, text: &str) -> Action {
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

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> Action {
        if self.modal.is_some() {
            return self.handle_modal_key(key);
        }
        if self.pending_ask.is_some() {
            return self.handle_ask_key(key);
        }
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Up {
            if let Some(last) = self.queued.pop_back() {
                // A queued item can be in two places: the local outbox while
                // offline, or the daemon's follow-up queue. Remove it from
                // both before putting it back in the editor.
                self.remove_outbox(&last);
                self.recall_queued();
                self.input = last;
                self.input_cursor = self.input.chars().count();
                self.focus = Focus::Input;
            } else {
                // The console may have restarted while the daemon still had
                // follow-ups queued; ask it for the last one.
                self.recall_queued();
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

    pub(super) fn handle_input_key(&mut self, key: KeyEvent) -> Action {
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

    pub(super) fn handle_modal_key(&mut self, key: KeyEvent) -> Action {
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
                                    self.spawn_agent_attach();
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

    pub(super) fn handle_login_key(&mut self, login: LoginModal, key: KeyEvent) {
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

    pub(super) fn insert_char(&mut self, c: char) {
        let at = self.byte_index(self.input_cursor);
        self.input.insert(at, c);
        self.input_cursor += 1;
        self.menu_selected = 0;
    }

    pub(super) fn delete_before_cursor(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let at = self.byte_index(self.input_cursor - 1);
        self.input.remove(at);
        self.input_cursor -= 1;
        self.menu_selected = 0;
    }

    pub(super) fn delete_at_cursor(&mut self) {
        if self.input_cursor >= self.input.chars().count() {
            return;
        }
        let at = self.byte_index(self.input_cursor);
        self.input.remove(at);
        self.menu_selected = 0;
    }

    pub(super) fn byte_index(&self, chars: usize) -> usize {
        self.input
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    pub(super) fn line_start_char(&self, cursor: usize) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = cursor.min(chars.len());
        while i > 0 && chars[i - 1] != '\n' {
            i -= 1;
        }
        i
    }

    pub(super) fn line_end_char(&self, cursor: usize) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = cursor.min(chars.len());
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        i
    }

    pub(super) fn move_cursor_vertical(&mut self, delta: isize) -> bool {
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

