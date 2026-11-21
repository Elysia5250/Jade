//! Agent turn, attach, outbox, and event application.
use super::*;

impl App {
    /// Connect (or reconnect) to the daemon session. The replay cursor comes
    /// from the last event applied to the local transcript.
    pub(in crate::tui) fn spawn_agent_attach(&mut self) {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(runtime::attach(
            self.data_dir.clone(),
            self.session.current.id.clone(),
            self.session.current.cwd.clone(),
            self.jobs.target.clone(),
            self.session.current.messages.clone(),
            runtime::AttachOrigin {
                from: self.session.current.event_seq,
                incarnation_id: self.agent.daemon_incarnation,
            },
            tx,
        ));
        self.agent.stream = Some(rx);
        self.agent.ready = false;
        self.agent.retry_at = Instant::now() + Duration::from_secs(2);
    }

    /// Rebuild the in-flight view after an attach. Events before the cursor
    /// are not replayed, so find the open assistant/tool items in the
    /// persisted transcript.
    pub(in crate::tui) fn resume_active_view(&mut self) {
        self.agent.active_assistant_id =
            self.session
                .current
                .items
                .iter()
                .rev()
                .find_map(|item| match item {
                    TranscriptItem::Assistant {
                        id, done: false, ..
                    } => Some(*id),
                    _ => None,
                });
        self.agent.active_tool_items.clear();
        for item in &self.session.current.items {
            if let TranscriptItem::Tool {
                id,
                call_id,
                status: CardStatus::Running,
                ..
            } = item
            {
                self.agent.active_tool_items.insert(call_id.clone(), *id);
            }
        }
    }

    /// Send while connected, otherwise hold the prompt until an attach
    /// succeeds so a daemon restart does not eat what the user typed.
    pub(in crate::tui) fn send_agent(
        &mut self,
        text: String,
        images: Vec<jade_agent::session::ImageAttachment>,
        mode: SendMode,
    ) {
        if !self.agent.ready || self.agent.offline || self.agent.stream.is_none() {
            self.agent.outbox.push_back((text, images, mode));
            if self.agent.offline || self.agent.stream.is_none() {
                self.set_toast("daemon offline - prompt queued", true);
            }
            return;
        }
        self.dispatch_agent(vec![(text, images, mode)]);
    }

    fn daemon_sender(&self) -> Option<mpsc::UnboundedSender<DaemonCmd>> {
        if let Some(tx) = self.agent.daemon_tx.borrow().as_ref() {
            if !tx.is_closed() {
                return Some(tx.clone());
            }
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return None;
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<DaemonCmd>();
        let data_dir = self.data_dir.clone();
        let notice = self.notice_tx.clone();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    DaemonCmd::Send {
                        session_id,
                        mut items,
                    } => {
                        while let Some((text, images, mode)) = items.first().cloned() {
                            let req = if images.is_empty() {
                                AgentReq::Send {
                                    id: session_id.clone(),
                                    text,
                                    mode,
                                }
                            } else {
                                AgentReq::SendImages {
                                    id: session_id.clone(),
                                    text,
                                    images,
                                    mode,
                                }
                            };
                            let result = runtime::tell(&data_dir, req).await;
                            if result.is_err() {
                                let _ = notice.send(UiNotice::Requeue { session_id, items });
                                break;
                            }
                            items.remove(0);
                        }
                    }
                    DaemonCmd::Tell(req) => {
                        let session_id = req_session_id(&req).map(str::to_string);
                        if let Err(err) = runtime::tell(&data_dir, req).await {
                            let _ = notice.send(UiNotice::Toast {
                                session_id,
                                msg: format!("{err:#}"),
                                err: true,
                            });
                        }
                    }
                }
            }
        });
        *self.agent.daemon_tx.borrow_mut() = Some(tx.clone());
        Some(tx)
    }

    pub(in crate::tui) fn dispatch_agent(
        &self,
        items: Vec<(String, Vec<jade_agent::session::ImageAttachment>, SendMode)>,
    ) {
        if items.is_empty() {
            return;
        }
        if let Some(tx) = self.daemon_sender() {
            let _ = tx.send(DaemonCmd::Send {
                session_id: self.session.current.id.clone(),
                items,
            });
        }
    }

    pub(in crate::tui) fn flush_outbox(&mut self) {
        if self.agent.outbox.is_empty() {
            return;
        }
        let items: Vec<_> = self.agent.outbox.drain(..).collect();
        self.dispatch_agent(items);
    }

    pub(in crate::tui) fn remove_outbox(&mut self, text: &str) {
        if let Some(pos) = self.agent.outbox.iter().rposition(|(t, _, _)| t == text) {
            self.agent.outbox.remove(pos);
        }
    }

    pub(in crate::tui) fn recall_queued(&self) {
        if self.agent.offline || self.agent.stream.is_none() {
            return;
        }
        let data_dir = self.data_dir.clone();
        let id = self.session.current.id.clone();
        let notice = self.notice_tx.clone();
        tokio::spawn(async move {
            let text = match runtime::request(&data_dir, &AgentReq::Recall { id: id.clone() }).await
            {
                Ok(AgentResp::Recalled(text)) => text,
                _ => None,
            };
            let _ = notice.send(UiNotice::Recalled {
                session_id: id,
                text,
            });
        });
    }

    pub(in crate::tui) fn drain_ui_notices(&mut self) -> bool {
        let mut notices = Vec::new();
        while let Ok(notice) = self.notice_rx.try_recv() {
            notices.push(notice);
        }
        let saw = !notices.is_empty();
        for notice in notices {
            self.apply_notice(notice);
        }
        saw
    }

    pub(in crate::tui) fn apply_notice(&mut self, notice: UiNotice) {
        match notice {
            UiNotice::DaemonReady => {
                self.jobs.last_refresh = Instant::now()
                    .checked_sub(self.jobs.interval)
                    .unwrap_or_else(Instant::now);
                self.agent.retry_at = Instant::now();
                self.inputs.invalidate_catalog();
                if self.ui.welcome.is_none() && self.agent.stream.is_none() {
                    self.spawn_agent_attach();
                }
            }
            UiNotice::ConduitWatch(rx) => {
                self.jobs.watch = Some(rx);
            }
            UiNotice::Requeue { session_id, items } => {
                if session_id != self.session.current.id {
                    return;
                }
                self.agent.offline = true;
                self.agent.ready = false;
                self.agent.stream = None;
                self.agent.retry_at = Instant::now() + Duration::from_millis(750);
                for item in items.into_iter().rev() {
                    self.agent.outbox.push_front(item);
                }
                self.set_toast("daemon offline - prompt queued", true);
            }
            UiNotice::Recalled {
                session_id,
                text: Some(text),
            } => {
                if session_id != self.session.current.id {
                    return;
                }
                // The daemon's copy is authoritative; this also catches
                // follow-ups queued by an earlier console.
                self.ui.input = text;
                self.ui.input_cursor = self.ui.input.chars().count();
                self.ui.focus = Focus::Input;
            }
            UiNotice::Recalled { text: None, .. } => {}
            UiNotice::Checkpoints {
                session_id,
                mut list,
            } => {
                if session_id != self.session.current.id {
                    return;
                }
                if list.is_empty() {
                    self.set_toast("no checkpoints recorded yet", false);
                } else {
                    list.reverse();
                    self.ui.modal = Some(Modal::Rewind(RewindModal {
                        checkpoints: list,
                        selected: 0,
                    }));
                }
            }
            UiNotice::Toast {
                session_id,
                msg,
                err,
            } => {
                if let Some(sid) = session_id {
                    if sid != self.session.current.id {
                        return;
                    }
                }
                self.set_toast(msg, err);
            }
            UiNotice::Notice {
                session_id,
                msg,
                err,
            } => {
                if session_id != self.session.current.id {
                    return;
                }
                self.push_notice(msg, err);
            }
            UiNotice::LiveSessions(list) => {
                self.apply_hub_live_sessions(list);
            }
            UiNotice::McpProbeLines(lines) => {
                self.apply_hub_mcp_probe(lines);
            }
        }
    }

    pub(in crate::tui) fn request_compact(&mut self, focus: &str, handoff: bool) {
        if self.agent.offline || self.agent.stream.is_none() || !self.agent.ready {
            self.set_toast("daemon offline - cannot run compaction", true);
            return;
        }
        if self.agent.running {
            self.set_toast("turn in progress; wait or interrupt first", true);
            return;
        }
        let data_dir = self.data_dir.clone();
        let id = self.session.current.id.clone();
        let focus = (!focus.trim().is_empty()).then(|| focus.trim().to_string());
        let notice = self.notice_tx.clone();
        self.set_toast(
            if handoff {
                "handoff compacting..."
            } else {
                "compacting..."
            },
            false,
        );
        let req = if handoff {
            AgentReq::Handoff {
                id: id.clone(),
                focus,
            }
        } else {
            AgentReq::Compact {
                id: id.clone(),
                focus,
            }
        };
        tokio::spawn(async move {
            match runtime::request(&data_dir, &req).await {
                Ok(AgentResp::Ack) => {}
                Ok(AgentResp::Error(e)) => {
                    let _ = notice.send(UiNotice::Toast {
                        session_id: Some(id),
                        msg: e,
                        err: true,
                    });
                }
                Err(e) => {
                    let _ = notice.send(UiNotice::Toast {
                        session_id: Some(id),
                        msg: format!("{e:#}"),
                        err: true,
                    });
                }
                _ => {}
            }
        });
    }

    pub(in crate::tui) fn start_agent_turn(
        &mut self,
        prompt: String,
        images: Vec<jade_agent::session::ImageAttachment>,
    ) {
        self.ui.follow = true;
        if !self.agent.offline && self.agent.stream.is_some() {
            self.agent.running = true;
        }
        self.send_agent(prompt, images, SendMode::Prompt);
    }

    pub(in crate::tui) fn drain_agent_events(&mut self) -> bool {
        self.drain_agent_batch(None)
    }

    /// Apply `first` (already taken off the stream) and any messages still queued.
    /// Session JSON is written once for the whole batch. Returns whether any
    /// message was applied.
    pub(in crate::tui) fn drain_agent_batch(&mut self, first: Option<AgentResp>) -> bool {
        self.session.persist_hold.set(true);
        let mut first = first;
        let mut saw = false;
        let mut disconnected = false;
        loop {
            let message = if let Some(message) = first.take() {
                message
            } else {
                let Some(rx) = self.agent.stream.as_mut() else {
                    self.release_persist();
                    return saw;
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
            saw = true;
            match message {
                AgentResp::SnapshotStart {
                    running,
                    incarnation_id,
                    ..
                } => {
                    self.agent.daemon_incarnation = incarnation_id;
                    self.agent.snapshot = Some(SnapshotAccum {
                        running,
                        ..SnapshotAccum::default()
                    });
                }
                AgentResp::SnapshotChunk { events } => {
                    if let Some(accum) = &mut self.agent.snapshot {
                        accum.events.extend(events);
                    }
                }
                AgentResp::SnapshotMessages { messages } => {
                    if let Some(accum) = &mut self.agent.snapshot {
                        accum.messages.extend(messages);
                        accum.saw_messages = true;
                    }
                }
                AgentResp::SnapshotEnd => {
                    if let Some(accum) = self.agent.snapshot.take() {
                        self.finish_snapshot(accum);
                    }
                }
                AgentResp::Event(item) => self.apply_event(item.seq, item.event),
                AgentResp::Error(err) => {
                    self.agent.snapshot = None;
                    let first = !self.agent.offline;
                    self.agent.offline = true;
                    self.agent.ready = false;
                    self.agent.running = false;
                    if first {
                        self.set_toast(err, true);
                    }
                }
                _ => {}
            }
        }
        self.release_persist();
        if disconnected {
            self.agent.stream = None;
            self.agent.ready = false;
            self.agent.running = false;
            self.agent.turn_started_at = None;
            self.agent.retry_at = Instant::now() + Duration::from_millis(750);
            if !self.agent.offline {
                self.agent.offline = true;
                self.set_toast("agent stream lost - reconnecting.", true);
            }
        }
        saw
    }

    pub(in crate::tui) fn set_permission_mode(&mut self, mode: PermissionMode) {
        self.session.perm_mode = mode;
        let _ = jade_agent::permissions::rules::save_default_mode(&self.data_dir, mode);
        self.set_toast(format!("permissions: {}", mode.label()), false);
        self.push_permission_mode_to_daemon(mode);
    }

    pub(in crate::tui) fn push_permission_mode_to_daemon(&self, mode: PermissionMode) {
        if self.agent.offline || self.agent.stream.is_none() || !self.agent.ready {
            return;
        }
        let id = self.session.current.id.clone();
        self.sync_agent(AgentReq::SetPermissionMode { id, mode });
    }

    pub(in crate::tui) fn sync_agent(&self, req: AgentReq) {
        if let Some(tx) = self.daemon_sender() {
            let _ = tx.send(DaemonCmd::Tell(req));
        }
    }

    fn finish_snapshot(&mut self, accum: SnapshotAccum) {
        self.agent.skip_replay_tokens = accum
            .events
            .iter()
            .any(|item| matches!(item.event, AgentEvent::SessionReset));
        let SnapshotAccum {
            running,
            events,
            messages,
            saw_messages,
        } = accum;
        for item in events {
            self.apply_event(item.seq, item.event);
        }
        self.agent.skip_replay_tokens = false;
        if saw_messages {
            self.session.current.messages = messages;
            self.persist_session();
        }
        // The daemon is authoritative: a console that missed a TurnDone must
        // not stay stuck in "running".
        self.agent.running = running;
        if running {
            self.agent.turn_started_at.get_or_insert_with(Instant::now);
        } else {
            self.agent.turn_started_at = None;
        }
        self.agent.offline = false;
        self.agent.ready = true;
        if running {
            self.resume_active_view();
        }
        if !self.agent.outbox.is_empty() {
            self.set_toast("daemon back - sending queued prompt", false);
            self.flush_outbox();
        }
        if self.session.perm_mode != PermissionMode::Default {
            self.push_permission_mode_to_daemon(self.session.perm_mode);
        }
    }

    fn ensure_provider_or_login(&mut self, prompt: &str) -> bool {
        if resolve_active_provider(&self.data_dir).is_some() {
            return true;
        }
        if self.ui.input.is_empty() {
            self.ui.input = prompt.to_string();
            self.ui.input_cursor = self.ui.input.chars().count();
        }
        self.ui.focus = Focus::Input;
        self.set_toast("no provider connected - complete /login first", true);
        self.open_login_modal();
        false
    }

    /// Enter while idle starts a worker turn. While a worker turn is running,
    /// Enter opens the plan sidechat. The side composer only talks to that thread.
    /// Ctrl+Enter is [`Self::submit_agent_steer`].
    pub(in crate::tui) fn submit_agent(&mut self, prompt: String) {
        if !self.ensure_provider_or_login(&prompt) {
            return;
        }
        let images = std::mem::take(&mut self.ui.pending_images);
        match self.send_mode_for_submit() {
            SendMode::Side => {
                self.send_agent(prompt, images, SendMode::Side);
                self.open_side_pane();
            }
            SendMode::SideTalk => self.send_agent(prompt, images, SendMode::SideTalk),
            _ => self.start_agent_turn(prompt, images),
        }
    }

    /// Ctrl+Enter. Steers the worker from the main composer. From the side
    /// composer it only steers the side thread.
    pub(in crate::tui) fn submit_agent_steer(&mut self, prompt: String) {
        if !self.ensure_provider_or_login(&prompt) {
            return;
        }
        let images = std::mem::take(&mut self.ui.pending_images);
        if self.ui.focus == Focus::Side {
            self.send_agent(prompt, images, SendMode::SideTalk);
            return;
        }
        self.send_agent(prompt, images, SendMode::Steer);
        if self.agent.running {
            self.set_toast("steer queued for the next tool boundary", false);
        }
    }

    /// Alt+Enter: deliver after the current run finishes.
    /// From the side composer this queues on the side thread only.
    pub(in crate::tui) fn queue_followup(&mut self, prompt: String) {
        if !self.ensure_provider_or_login(&prompt) {
            return;
        }
        let images = std::mem::take(&mut self.ui.pending_images);
        if self.ui.focus == Focus::Side {
            self.send_agent(prompt, images, SendMode::SideQueue);
            self.set_toast("side follow-up queued", false);
            return;
        }
        self.send_agent(prompt.clone(), images, SendMode::FollowUp);
        self.agent.queued.push_back(prompt);
        self.set_toast("follow-up queued", false);
    }

    pub(in crate::tui) fn answer_ask(&self, ask_id: u64, label: String) {
        let id = self.session.current.id.clone();
        self.sync_agent(AgentReq::Answer { id, ask_id, label });
    }

    /// Report the first interaction with a shown prompt, which stops the
    /// daemon's answer clock so the recommended option is never taken out from
    /// under a user who is still reading or moving the cursor.
    pub(in crate::tui) fn engage_ask(&mut self) {
        let Some(ask_id) = self.agent.pending_ask.as_mut().and_then(|ask| {
            if ask.touched {
                return None;
            }
            ask.touched = true;
            ask.paused_left = Some(ask.deadline.saturating_duration_since(Instant::now()));
            Some(ask.id)
        }) else {
            return;
        };
        let id = self.session.current.id.clone();
        self.sync_agent(AgentReq::AskTouched { id, ask_id });
    }

    pub(in crate::tui) fn handle_ask_key(&mut self, key: KeyEvent) -> Action {
        // Any keypress is a reaction, so the countdown stops before it fires.
        self.engage_ask();
        let Some(mut ask) = self.agent.pending_ask.take() else {
            return Action::None;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                ask.selected = ask.selected.saturating_sub(1);
                self.agent.pending_ask = Some(ask);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                ask.selected = (ask.selected + 1).min(ask.options.len().saturating_sub(1));
                self.agent.pending_ask = Some(ask);
            }
            KeyCode::Char(c @ '1'..='9') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let idx = (c as usize) - ('1' as usize);
                if let Some(opt) = ask.options.get(idx) {
                    let label = opt.label.clone();
                    self.answer_ask(ask.id, label.clone());
                    self.set_toast(format!("chose: {label}"), false);
                } else {
                    self.agent.pending_ask = Some(ask);
                }
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
                self.agent.pending_ask = Some(ask);
                return self.esc_interrupt();
            }
            _ => self.agent.pending_ask = Some(ask),
        }
        Action::None
    }

    pub(in crate::tui) fn interrupt_agent(&self) {
        let id = self.session.current.id.clone();
        self.sync_agent(AgentReq::Interrupt { id });
    }
}

fn req_session_id(req: &AgentReq) -> Option<&str> {
    match req {
        AgentReq::Send { id, .. }
        | AgentReq::Interrupt { id }
        | AgentReq::Answer { id, .. }
        | AgentReq::AskTouched { id, .. }
        | AgentReq::SetPermissionMode { id, .. }
        | AgentReq::SetCwd { id, .. }
        | AgentReq::SetTarget { id, .. }
        | AgentReq::Clear { id }
        | AgentReq::CloseSession { id } => Some(id.as_str()),
        _ => None,
    }
}
