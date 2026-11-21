//! Turn a child agent stream into parent `Subagent` notes.
//!
//! Token deltas are batched. The parent still receives the short `ToolOutput`
//! lines the tool card already logged.

use super::{compact_preview, emit_line, PreparedTask};
use crate::agent::events::{AgentEvent, SubagentNote};
use tokio::sync::mpsc;

const TEXT_FLUSH: usize = 240;
const PROMPT_CAP: usize = 1500;
const NOTE_CAP: usize = 800;

pub(super) struct LaneId<'a> {
    pub parent_call_id: &'a str,
    pub index: u32,
    pub total: u32,
    pub name: &'a str,
    pub description: &'a str,
}

pub(super) fn cap_text(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

pub(super) fn emit_subagent(
    tx: &mpsc::UnboundedSender<AgentEvent>,
    id: LaneId<'_>,
    note: SubagentNote,
) {
    let _ = tx.send(AgentEvent::Subagent {
        parent_call_id: id.parent_call_id.to_string(),
        index: id.index,
        total: id.total,
        name: id.name.to_string(),
        description: id.description.to_string(),
        note,
    });
}

pub(super) fn announce_task(
    tx: &mpsc::UnboundedSender<AgentEvent>,
    parent_call_id: &str,
    total: usize,
    task: &PreparedTask,
) {
    let description = task
        .input
        .description
        .clone()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| compact_preview(&task.input.prompt, 48));
    let id = lane_id(parent_call_id, task, total, &description);
    emit_subagent(
        tx,
        id,
        SubagentNote::Started {
            prompt: cap_text(&task.input.prompt, PROMPT_CAP),
        },
    );
    if let Some(reason) = &task.blocked_reason {
        let id = lane_id(parent_call_id, task, total, &description);
        emit_subagent(
            tx,
            id,
            SubagentNote::Text {
                text: cap_text(reason, NOTE_CAP),
            },
        );
        let id = lane_id(parent_call_id, task, total, &description);
        emit_subagent(tx, id, SubagentNote::Finished { ok: false });
    }
}

pub(super) fn emit_stopped(
    tx: &mpsc::UnboundedSender<AgentEvent>,
    parent_call_id: &str,
    task: &PreparedTask,
    total: usize,
    detail: &str,
) {
    let description = task.input.description.clone().unwrap_or_default();
    let id = lane_id(parent_call_id, task, total, &description);
    emit_subagent(
        tx,
        id,
        SubagentNote::Text {
            text: cap_text(detail, NOTE_CAP),
        },
    );
    let id = lane_id(parent_call_id, task, total, &description);
    emit_subagent(tx, id, SubagentNote::Finished { ok: false });
}

fn lane_id<'a>(
    parent_call_id: &'a str,
    task: &'a PreparedTask,
    total: usize,
    description: &'a str,
) -> LaneId<'a> {
    LaneId {
        parent_call_id,
        index: u32::try_from(task.index).unwrap_or(u32::MAX),
        total: u32::try_from(total).unwrap_or(u32::MAX),
        name: &task.spec.name,
        description,
    }
}

/// Forwards one running child into the parent event stream.
pub(super) struct ChildTap {
    tx: mpsc::UnboundedSender<AgentEvent>,
    parent_call_id: String,
    index: u32,
    total: u32,
    name: String,
    description: String,
    prefix: String,
    text: String,
}

impl ChildTap {
    pub(super) fn new(
        tx: mpsc::UnboundedSender<AgentEvent>,
        parent_call_id: &str,
        index: usize,
        total: usize,
        name: &str,
        description: &str,
        prefix: &str,
    ) -> Self {
        Self {
            tx,
            parent_call_id: parent_call_id.to_string(),
            index: u32::try_from(index).unwrap_or(u32::MAX),
            total: u32::try_from(total).unwrap_or(u32::MAX),
            name: name.to_string(),
            description: description.to_string(),
            prefix: prefix.to_string(),
            text: String::new(),
        }
    }

    fn id(&self) -> LaneId<'_> {
        LaneId {
            parent_call_id: &self.parent_call_id,
            index: self.index,
            total: self.total,
            name: &self.name,
            description: &self.description,
        }
    }

    pub(super) fn push(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::AssistantStart { .. } => self.flush_text(),
            AgentEvent::TextDelta(chunk) => {
                self.text.push_str(&chunk);
                if self.text.chars().count() >= TEXT_FLUSH {
                    self.flush_text();
                }
            }
            AgentEvent::ToolStart {
                call_id,
                name,
                title,
                subtitle,
            } => {
                self.flush_text();
                emit_line(
                    &self.tx,
                    &self.parent_call_id,
                    format!("{} {title}", self.prefix),
                    false,
                );
                emit_subagent(
                    &self.tx,
                    self.id(),
                    SubagentNote::Tool {
                        call_id,
                        name,
                        title,
                        subtitle,
                        running: true,
                        ok: true,
                        duration_ms: 0,
                    },
                );
            }
            AgentEvent::ToolDone {
                call_id,
                ok,
                duration_ms,
                ..
            } => emit_subagent(
                &self.tx,
                self.id(),
                SubagentNote::Tool {
                    call_id,
                    name: String::new(),
                    title: String::new(),
                    subtitle: String::new(),
                    running: false,
                    ok,
                    duration_ms,
                },
            ),
            AgentEvent::Say { text, .. } => {
                self.flush_text();
                emit_line(
                    &self.tx,
                    &self.parent_call_id,
                    format!("{} note: {text}", self.prefix),
                    false,
                );
                emit_subagent(&self.tx, self.id(), SubagentNote::Say { text });
            }
            ev @ (AgentEvent::FileChanged { .. }
            | AgentEvent::Ask { .. }
            | AgentEvent::AskClosed { .. }
            | AgentEvent::HookRan { .. }) => {
                let _ = self.tx.send(ev);
            }
            _ => {}
        }
    }

    pub(super) fn flush_text(&mut self) {
        let text = std::mem::take(&mut self.text);
        if text.trim().is_empty() {
            return;
        }
        const CHUNK_CHARS: usize = 4000;
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let split = rest
                .char_indices()
                .nth(CHUNK_CHARS)
                .map(|(idx, _)| idx)
                .unwrap_or(rest.len());
            let chunk = &rest[..split];
            emit_subagent(
                &self.tx,
                self.id(),
                SubagentNote::Text {
                    text: chunk.to_string(),
                },
            );
            rest = &rest[split..];
        }
    }
}
