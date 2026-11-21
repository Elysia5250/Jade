//! Plan sidechat transcript and its composer.
use super::*;

impl App {
    pub(in crate::tui) fn send_mode_for_submit(&self) -> SendMode {
        if self.ui.focus == Focus::Side {
            SendMode::SideTalk
        } else if self.agent.running {
            SendMode::Side
        } else {
            SendMode::Prompt
        }
    }

    pub(in crate::tui) fn open_side_pane(&mut self) {
        self.ui.side_open = true;
        self.ui.inspector.tab = InspectorTab::Side;
        self.ui.inspector.scroll = usize::MAX;
        self.ui.inspector.max_scroll = 0;
        self.ui.show_sidebar = true;
        self.ui.inspector.force_open = true;
        self.ui.focus = Focus::Side;
    }

    pub(in crate::tui) fn apply_side_note(&mut self, note: SideNote) {
        match note {
            SideNote::Closed => {
                self.ui.side_open = false;
                self.ui.side_running = false;
                self.ui.side_items.clear();
                self.ui.side_input.clear();
                self.ui.side_cursor = 0;
                if self.ui.inspector.tab == InspectorTab::Side {
                    self.ui.inspector.tab = InspectorTab::Overview;
                }
                if self.ui.focus == Focus::Side {
                    self.ui.focus = Focus::Input;
                }
            }
            SideNote::Done { ok } => {
                self.ui.side_running = false;
                close_side_items(&mut self.ui.side_items, ok);
            }
            other => {
                self.ui.side_open = true;
                self.ui.show_sidebar = true;
                self.ui.inspector.force_open = true;
                if self.ui.inspector.tab != InspectorTab::Agent {
                    self.ui.inspector.tab = InspectorTab::Side;
                }
                write_side_note(self, other);
            }
        }
    }

    pub(in crate::tui) fn handle_side_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Esc {
            if self.has_interruptible_work() {
                return self.esc_interrupt();
            }
            self.ui.focus = Focus::Input;
            return Action::None;
        }
        if key.code == KeyCode::Enter {
            let text = self.ui.side_input.trim().to_string();
            if text.is_empty() {
                return Action::None;
            }
            self.ui.side_input.clear();
            self.ui.side_cursor = 0;
            return if key.modifiers.contains(KeyModifiers::ALT) {
                Action::SubmitAgentFollowup(text)
            } else if key.modifiers.contains(KeyModifiers::CONTROL) {
                Action::SubmitAgentSteer(text)
            } else {
                Action::SubmitAgent(text)
            };
        }
        line_edit::handle_key(&mut self.ui.side_input, &mut self.ui.side_cursor, key);
        Action::None
    }
}

fn write_side_note(app: &mut App, note: SideNote) {
    match note {
        SideNote::User { text } => {
            app.ui.side_running = true;
            let cwd = app.cwd_label();
            let id = app.session.current.alloc_id();
            app.ui.side_items.push(TranscriptItem::User {
                id,
                text,
                mode: "plan".into(),
                cwd,
                ts_ms: now_ms(),
            });
        }
        SideNote::Text { text } => {
            append_side_text(&mut app.session.current, &mut app.ui.side_items, &text);
        }
        SideNote::Tool {
            call_id,
            name,
            title,
            subtitle,
            running,
            ok,
            duration_ms,
        } => upsert_side_tool(
            &mut app.session.current,
            &mut app.ui.side_items,
            SideToolUpdate {
                call_id,
                name,
                title,
                subtitle,
                running,
                ok,
                duration_ms,
            },
        ),
        SideNote::Say { text } => {
            if text.trim().is_empty() {
                return;
            }
            let id = app.session.current.alloc_id();
            app.ui.side_items.push(TranscriptItem::Say {
                id,
                text,
                level: "info".into(),
            });
        }
        SideNote::Done { .. } | SideNote::Closed => {}
    }
}

fn append_side_text(session: &mut Session, items: &mut Vec<TranscriptItem>, text: &str) {
    if let Some(TranscriptItem::Assistant {
        text: slot, done, ..
    }) = items.last_mut()
    {
        if !*done {
            slot.push_str(text);
            return;
        }
    }
    let id = session.alloc_id();
    items.push(TranscriptItem::Assistant {
        id,
        text: text.to_string(),
        reasoning: String::new(),
        reasoning_collapsed: true,
        model: String::new(),
        done: false,
        error: None,
    });
}

struct SideToolUpdate {
    call_id: String,
    name: String,
    title: String,
    subtitle: String,
    running: bool,
    ok: bool,
    duration_ms: u64,
}

fn upsert_side_tool(
    session: &mut Session,
    items: &mut Vec<TranscriptItem>,
    update: SideToolUpdate,
) {
    let SideToolUpdate {
        call_id,
        name,
        title,
        subtitle,
        running,
        ok,
        duration_ms,
    } = update;
    if let Some(TranscriptItem::Tool {
        name: slot_name,
        title: slot_title,
        subtitle: slot_sub,
        status,
        duration_ms: slot_dur,
        ..
    }) = items
        .iter_mut()
        .rev()
        .find(|item| matches!(item, TranscriptItem::Tool { call_id: id, .. } if id == &call_id))
    {
        if !name.is_empty() {
            *slot_name = name;
        }
        if !title.is_empty() {
            *slot_title = title;
        }
        if !subtitle.is_empty() {
            *slot_sub = subtitle;
        }
        *status = if running {
            CardStatus::Running
        } else if ok {
            CardStatus::Done
        } else {
            CardStatus::Error
        };
        if !running {
            *slot_dur = Some(duration_ms);
        }
        return;
    }
    if name.is_empty() && title.is_empty() {
        return;
    }
    let id = session.alloc_id();
    items.push(TranscriptItem::Tool {
        id,
        call_id,
        name,
        title,
        subtitle,
        job_id: None,
        target: "local".into(),
        output: Vec::new(),
        status: if running {
            CardStatus::Running
        } else if ok {
            CardStatus::Done
        } else {
            CardStatus::Error
        },
        collapsed: true,
        duration_ms: if running { None } else { Some(duration_ms) },
        exit_code: None,
        started_ms: now_ms(),
    });
}

fn close_side_items(items: &mut [TranscriptItem], ok: bool) {
    for item in items.iter_mut() {
        match item {
            TranscriptItem::Assistant { done, error, .. } if !*done => {
                *done = true;
                if !ok && error.is_none() {
                    *error = Some("stopped".into());
                }
            }
            TranscriptItem::Tool { status, .. } if *status == CardStatus::Running => {
                *status = if ok {
                    CardStatus::Done
                } else {
                    CardStatus::Error
                };
            }
            _ => {}
        }
    }
}
