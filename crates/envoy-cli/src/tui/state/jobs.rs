//! Daemon jobs, shell cards, and job cancellation.
use super::*;

impl App {
    pub(in crate::tui) async fn refresh(&mut self, remote: &Remote<AnyDialer>) {
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

    pub(in crate::tui) async fn submit_shell(
        &mut self,
        remote: &Remote<AnyDialer>,
        req: SubmitRequest,
    ) {
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

    pub(in crate::tui) async fn attach_job_card(&mut self, remote: &Remote<AnyDialer>, job: Job) {
        self.open_job_output(job.id);
        if let Some(pos) = self.session.items.iter().position(
            |it| matches!(it, TranscriptItem::Tool { job_id: Some(id), .. } if *id == job.id),
        ) {
            self.selected_card = Some(pos);
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
            collapsed: true,
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

    pub(in crate::tui) async fn poll_shell_cards(&mut self, remote: &Remote<AnyDialer>) {
        if self.shell_watches.is_empty() {
            return;
        }
        let mut still_running = Vec::new();
        let mut watches = std::mem::take(&mut self.shell_watches);
        for mut w in watches.drain(..) {
            let done = Self::poll_one_watch(remote, &self.target, &mut self.session, &mut w).await;
            if !done {
                still_running.push(w);
            }
        }
        self.shell_watches = still_running;
    }

    pub(in crate::tui) async fn poll_one_watch(
        remote: &Remote<AnyDialer>,
        target: &str,
        session: &mut Session,
        w: &mut ShellWatch,
    ) -> bool {
        let mut new_lines: Vec<(String, bool)> = Vec::new();
        let res = remote
            .logs(
                target,
                w.job_id,
                w.cursor,
                false,
                |records: &[LogRecord]| {
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
                },
            )
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

    pub(in crate::tui) async fn cancel_active(&mut self, remote: &Remote<AnyDialer>) {
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
                if let Some(TranscriptItem::Tool { status, job_id, .. }) =
                    self.session.items.iter_mut().find(|i| i.id() == *item_id)
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

    pub(in crate::tui) async fn cancel_job(&mut self, remote: &Remote<AnyDialer>, id: JobId) {
        match remote.cancel(&self.target, id).await {
            Ok(()) => self.set_toast(format!("cancel requested for #{id}"), false),
            Err(e) => self.set_toast(format!("{e:#}"), true),
        }
        self.refresh(remote).await;
    }
}
