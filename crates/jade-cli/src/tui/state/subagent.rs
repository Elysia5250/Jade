//! Fold `Subagent` events into per-call lanes and open one in the inspector.
use super::*;

const LANE_ITEM_CAP: usize = 48;
const TEXT_CAP: usize = 6000;
const MAX_SUBAGENT_GROUPS: usize = 32;

impl App {
    pub(in crate::tui) fn apply_subagent(
        &mut self,
        parent_call_id: String,
        index: u32,
        name: String,
        description: String,
        note: SubagentNote,
    ) {
        let pos = upsert_lane(
            &mut self.session.current,
            &parent_call_id,
            index,
            &name,
            &description,
        );
        let mut lane = self.session.current.subagents[&parent_call_id][pos].clone();
        let persist = write_note(&mut self.session.current, &mut lane, note);
        self.session
            .current
            .subagents
            .get_mut(&parent_call_id)
            .unwrap()[pos] = lane;
        if persist {
            self.session.current.touch();
            self.persist_session();
        }
    }

    /// A finished `delegate` call closes any child that never reported `Finished`.
    pub(in crate::tui) fn settle_delegate(&mut self, call_id: &str, ok: bool) {
        let Some(lanes) = self.session.current.subagents.get_mut(call_id) else {
            return;
        };
        for lane in lanes {
            if lane.status == CardStatus::Running {
                lane.status = if ok {
                    CardStatus::Done
                } else {
                    CardStatus::Error
                };
            }
            close_open_items(lane, ok);
        }
    }

    /// Interrupt, timeout, and hook failure turn a still-running child red.
    pub(in crate::tui) fn stop_running_subagents(&mut self) {
        for lanes in self.session.current.subagents.values_mut() {
            for lane in lanes
                .iter_mut()
                .filter(|lane| lane.status == CardStatus::Running)
            {
                lane.status = CardStatus::Error;
                close_open_items(lane, false);
            }
        }
    }

    pub(in crate::tui) fn subagent_running(&self) -> bool {
        self.session
            .current
            .subagents
            .values()
            .any(|lanes| lanes.iter().any(|lane| lane.status == CardStatus::Running))
    }

    pub(in crate::tui) fn open_subagent(&mut self, call_id: String, index: u32) {
        self.ui.inspector.subagent = Some((call_id, index));
        self.ui.inspector.tab = InspectorTab::Agent;
        self.ui.inspector.scroll = usize::MAX;
        self.ui.inspector.max_scroll = 0;
        self.ui.show_sidebar = true;
        self.ui.inspector.force_open = true;
        self.ui.focus = Focus::Sidebar;
    }
}

fn upsert_lane(
    session: &mut Session,
    call_id: &str,
    index: u32,
    name: &str,
    description: &str,
) -> usize {
    if !session.subagents.contains_key(call_id) && session.subagents.len() >= MAX_SUBAGENT_GROUPS {
        let evict = session
            .subagents
            .iter()
            .find(|(_, lanes)| lanes.iter().all(|l| l.status != CardStatus::Running))
            .or_else(|| session.subagents.iter().next())
            .map(|(k, _)| k.clone());
        if let Some(key) = evict {
            session.subagents.remove(&key);
        }
    }
    let lanes = session.subagents.entry(call_id.to_string()).or_default();
    if let Some(pos) = lanes.iter().position(|lane| lane.index == index) {
        if lanes[pos].name.is_empty() {
            lanes[pos].name = name.to_string();
        }
        if lanes[pos].description.is_empty() && !description.is_empty() {
            lanes[pos].description = description.to_string();
        }
        return pos;
    }
    lanes.push(SubagentLane {
        index,
        name: name.to_string(),
        description: description.to_string(),
        status: CardStatus::Running,
        items: Vec::new(),
    });
    lanes.sort_by_key(|lane| lane.index);
    lanes
        .iter()
        .position(|lane| lane.index == index)
        .unwrap_or(0)
}

fn write_note(session: &mut Session, lane: &mut SubagentLane, note: SubagentNote) -> bool {
    match note {
        SubagentNote::Started { prompt } => {
            if lane.status != CardStatus::Error && lane.status != CardStatus::Done {
                lane.status = CardStatus::Running;
            }
            let has_prompt = lane
                .items
                .iter()
                .any(|item| matches!(item, TranscriptItem::User { .. }));
            if !prompt.trim().is_empty() && !has_prompt {
                push_capped(lane, user_item(session, prompt));
            }
            true
        }
        SubagentNote::Text { text } => {
            append_text(session, lane, &text);
            false
        }
        SubagentNote::Tool {
            call_id,
            name,
            title,
            subtitle,
            running,
            ok,
            duration_ms,
        } => {
            note_tool(
                session,
                lane,
                SubagentToolUpdate {
                    call_id,
                    name,
                    title,
                    subtitle,
                    running,
                    ok,
                    duration_ms,
                },
            );
            true
        }
        SubagentNote::Say { text } => {
            if !text.trim().is_empty() {
                push_capped(lane, say_item(session, text));
            }
            true
        }
        SubagentNote::Finished { ok } => {
            lane.status = if ok {
                CardStatus::Done
            } else {
                CardStatus::Error
            };
            close_open_items(lane, ok);
            true
        }
    }
}

fn append_text(session: &mut Session, lane: &mut SubagentLane, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(TranscriptItem::Assistant {
        text: slot, done, ..
    }) = lane.items.last_mut()
    {
        if !*done {
            if slot.chars().count() < TEXT_CAP {
                slot.push_str(text);
                return;
            }
            *done = true;
        }
    }
    let trimmed = text.trim_start_matches('\n');
    if trimmed.trim().is_empty() {
        return;
    }
    push_capped(lane, assistant_item(session, lane.name.clone(), trimmed));
}

struct SubagentToolUpdate {
    call_id: String,
    name: String,
    title: String,
    subtitle: String,
    running: bool,
    ok: bool,
    duration_ms: u64,
}

fn note_tool(session: &mut Session, lane: &mut SubagentLane, update: SubagentToolUpdate) {
    let SubagentToolUpdate {
        call_id,
        name,
        title,
        subtitle,
        running,
        ok,
        duration_ms,
    } = update;
    if let Some(item) = lane
        .items
        .iter_mut()
        .find(|item| matches!(item, TranscriptItem::Tool { call_id: id, .. } if id == &call_id))
    {
        if let TranscriptItem::Tool {
            status,
            duration_ms: slot,
            exit_code,
            title: title_slot,
            subtitle: sub_slot,
            name: name_slot,
            ..
        } = item
        {
            if !title.is_empty() {
                *title_slot = title;
                *sub_slot = subtitle;
                *name_slot = name;
            }
            if running {
                *status = CardStatus::Running;
            } else {
                *status = if ok {
                    CardStatus::Done
                } else {
                    CardStatus::Error
                };
                *slot = Some(duration_ms);
                *exit_code = Some(if ok { 0 } else { 1 });
            }
        }
        return;
    }
    if title.is_empty() && name.is_empty() {
        return;
    }
    let label = if title.is_empty() {
        name.clone()
    } else {
        title
    };
    push_capped(
        lane,
        TranscriptItem::Tool {
            id: session.alloc_id(),
            call_id,
            name,
            title: label,
            subtitle,
            job_id: None,
            target: String::new(),
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
            exit_code: if running {
                None
            } else {
                Some(if ok { 0 } else { 1 })
            },
            started_ms: now_ms(),
        },
    );
}

fn close_open_items(lane: &mut SubagentLane, ok: bool) {
    for item in &mut lane.items {
        match item {
            TranscriptItem::Assistant { done, .. } => *done = true,
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

fn push_capped(lane: &mut SubagentLane, item: TranscriptItem) {
    if lane.items.len() >= LANE_ITEM_CAP {
        if let Some(drop_at) = lane
            .items
            .iter()
            .position(|item| !matches!(item, TranscriptItem::User { .. }))
        {
            lane.items.remove(drop_at);
        }
    }
    lane.items.push(item);
}

fn user_item(session: &mut Session, text: String) -> TranscriptItem {
    TranscriptItem::User {
        id: session.alloc_id(),
        text,
        mode: "subagent".into(),
        cwd: String::new(),
        ts_ms: now_ms(),
    }
}

fn assistant_item(session: &mut Session, model: String, text: &str) -> TranscriptItem {
    TranscriptItem::Assistant {
        id: session.alloc_id(),
        text: text.to_string(),
        reasoning: String::new(),
        reasoning_collapsed: true,
        model,
        done: false,
        error: None,
    }
}

fn say_item(session: &mut Session, text: String) -> TranscriptItem {
    TranscriptItem::Say {
        id: session.alloc_id(),
        text,
        level: "info".into(),
    }
}
