//! Fold daemon agent events into the local transcript.
use super::*;

impl App {
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
                        *status = if ok {
                            CardStatus::Done
                        } else {
                            CardStatus::Error
                        };
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
                    "generating handoff summary."
                } else if auto {
                    "auto-compacting context."
                } else {
                    "compacting context."
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
}
