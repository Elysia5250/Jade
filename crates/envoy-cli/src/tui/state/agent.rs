//! Agent turn, attach, outbox, and event application.
use super::*;

impl App {

    /// Connect (or reconnect) to the daemon session. The replay cursor comes
    /// from the last event applied to the local transcript.
    pub(in crate::tui) fn spawn_agent_attach(&mut self) {
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
    pub(in crate::tui) fn resume_active_view(&mut self) {
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


    /// Send while connected, otherwise hold the prompt until an attach
    /// succeeds so a daemon restart does not eat what the user typed.
    pub(in crate::tui) fn send_agent(&mut self, text: String, mode: SendMode) {
        if !self.agent_ready || self.agent_offline || self.agent_stream.is_none() {
            self.outbox.push_back((text, mode));
            if self.agent_offline || self.agent_stream.is_none() {
                self.set_toast("daemon offline — prompt queued", true);
            }
            return;
        }
        self.dispatch_agent(vec![(text, mode)]);
    }


    pub(in crate::tui) fn dispatch_agent(&self, items: Vec<(String, SendMode)>) {
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


    pub(in crate::tui) fn flush_outbox(&mut self) {
        if self.outbox.is_empty() {
            return;
        }
        let items: Vec<(String, SendMode)> = self.outbox.drain(..).collect();
        self.dispatch_agent(items);
    }


    pub(in crate::tui) fn remove_outbox(&mut self, text: &str) {
        if let Some(pos) = self.outbox.iter().rposition(|(t, _)| t == text) {
            self.outbox.remove(pos);
        }
    }


    pub(in crate::tui) fn recall_queued(&self) {
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


    pub(in crate::tui) fn drain_ui_notices(&mut self) {
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
                UiNotice::Toast(msg, err) => {
                    self.set_toast(msg, err);
                }
                UiNotice::Notice(msg, err) => {
                    self.push_notice(msg, err);
                }
            }
        }
    }

    pub(in crate::tui) fn request_compact(&mut self, focus: &str, handoff: bool) {
        if self.agent_offline || self.agent_stream.is_none() || !self.agent_ready {
            self.set_toast("daemon offline — cannot run compaction", true);
            return;
        }
        if self.agent_running {
            self.set_toast("turn in progress; wait or interrupt first", true);
            return;
        }
        let data_dir = self.data_dir.clone();
        let id = self.session.id.clone();
        let focus = (!focus.trim().is_empty()).then(|| focus.trim().to_string());
        let notice = self.notice_tx.clone();
        let req = if handoff {
            AgentReq::Handoff { id, focus }
        } else {
            AgentReq::Compact { id, focus }
        };
        tokio::spawn(async move {
            match runtime::request(&data_dir, &req).await {
                Ok(AgentResp::Ack) => {}
                Ok(AgentResp::Error(e)) => {
                    let _ = notice.send(UiNotice::Toast(e, true));
                }
                Err(e) => {
                    let _ = notice.send(UiNotice::Toast(format!("{e:#}"), true));
                }
                _ => {}
            }
        });
    }


    pub(in crate::tui) fn start_agent_turn(&mut self, prompt: String) {
        self.follow = true;
        if !self.agent_offline && self.agent_stream.is_some() {
            self.agent_running = true;
        }
        self.send_agent(prompt, SendMode::Prompt);
    }


    pub(in crate::tui) fn drain_agent_events(&mut self) {
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
                    if self.perm_mode != PermissionMode::Default {
                        self.push_permission_mode_to_daemon(self.perm_mode);
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


    pub(in crate::tui) fn apply_event(&mut self, seq: u64, event: AgentEvent) {
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
            AgentEvent::Compacting { auto, handoff } => {
                let msg = if handoff {
                    "generating handoff summary…"
                } else if auto {
                    "auto-compacting context…"
                } else {
                    "compacting context…"
                };
                self.agent_running = true;
                self.set_toast(msg, false);
            }
            AgentEvent::Compacted {
                messages,
                before_tokens,
                after_tokens,
                summary: _,
                auto,
            } => {
                self.session.messages = messages;
                self.session.prompt_tokens = after_tokens;
                let tag = if auto { "auto-compacted" } else { "compacted" };
                let note = format!(
                    "{tag} context: {} -> {} tokens (visual history kept)",
                    fmt_tokens(before_tokens),
                    fmt_tokens(after_tokens)
                );
                self.push_notice(note, false);
                if !auto {
                    self.agent_running = false;
                    self.set_toast("context compacted", false);
                }
                self.session.touch();
                self.persist_session();
            }
            AgentEvent::HandoffReady {
                summary,
                before_tokens,
                after_tokens,
                title,
            } => {
                self.persist_session();
                let mut fresh = Session::new(self.session.cwd.clone(), self.target.clone());
                fresh.title = title;
                fresh.prompt_tokens = after_tokens;
                fresh.messages = vec![
                    ChatMessage {
                        role: ChatRole::User,
                        content: format!(
                            "<handoff_context>\n{summary}\n</handoff_context>\n\n\
                             [Started clean session via /handoff]"
                        ),
                        reasoning: None,
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                        tool_name: None,
                    },
                    ChatMessage {
                        role: ChatRole::Assistant,
                        content: "Ready to continue from the handoff summary.".into(),
                        reasoning: None,
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                        tool_name: None,
                    },
                ];
                let notice_id = fresh.alloc_id();
                fresh.items.push(TranscriptItem::Notice {
                    id: notice_id,
                    text: format!(
                        "handoff ready: {} -> {} tokens (clean session)",
                        fmt_tokens(before_tokens),
                        fmt_tokens(after_tokens)
                    ),
                    is_error: false,
                });
                let say_id = fresh.alloc_id();
                fresh.items.push(TranscriptItem::Say {
                    id: say_id,
                    text: summary,
                    level: "handoff".into(),
                });
                self.session = fresh;
                self.selected_card = None;
                self.follow = true;
                self.expanded_explore_groups.clear();
                self.transcript_cache.clear();
                self.inspector = InspectorState::default();
                self.reset_agent_view();
                self.persist_session();
                self.spawn_agent_attach();
                self.set_toast("started clean handoff session", false);
            }
            AgentEvent::PermissionModeChanged { mode } => {
                self.perm_mode = mode;
            }
            AgentEvent::FileChanged {
                path,
                before,
                after,
                added: _,
                removed: _,
            } => {
                self.session.record_file_change(path, before, after);
                self.workspace_index.invalidate();
                self.transcript_cache.clear();
                self.session.touch();
                self.persist_session();
            }
        }
    }

    pub(in crate::tui) fn set_permission_mode(&mut self, mode: PermissionMode) {
        self.perm_mode = mode;
        self.set_toast(format!("permissions: {}", mode.label()), false);
        self.push_permission_mode_to_daemon(mode);
    }

    pub(in crate::tui) fn push_permission_mode_to_daemon(&self, mode: PermissionMode) {
        if self.agent_offline || self.agent_stream.is_none() || !self.agent_ready {
            return;
        }
        let data_dir = self.data_dir.clone();
        let id = self.session.id.clone();
        tokio::spawn(async move {
            let _ = runtime::tell(&data_dir, AgentReq::SetPermissionMode { id, mode }).await;
        });
    }


    /// Enter while idle starts a turn; while running it steers the loop with
    /// the user's exact words at the next tool boundary.
    pub(in crate::tui) fn submit_agent(&mut self, prompt: String) {
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
    pub(in crate::tui) fn queue_followup(&mut self, prompt: String) {
        self.send_agent(prompt.clone(), SendMode::FollowUp);
        self.queued.push_back(prompt);
        self.set_toast("follow-up queued", false);
    }


    pub(in crate::tui) fn answer_ask(&self, ask_id: u64, label: String) {
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


    pub(in crate::tui) fn handle_ask_key(&mut self, key: KeyEvent) -> Action {
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


    pub(in crate::tui) fn interrupt_agent(&self) {
        let data_dir = self.data_dir.clone();
        let id = self.session.id.clone();
        tokio::spawn(async move {
            let _ = runtime::tell(&data_dir, AgentReq::Interrupt { id }).await;
        });
    }

}
