//! Which transcript rows collapse into one activity line.

use crate::tui::*;

pub(super) fn is_groupable_tool(item: &TranscriptItem) -> bool {
    matches!(item, TranscriptItem::Tool { name, .. } if name != "delegate")
}

pub(super) fn is_reasoning_only(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Assistant {
            text,
            error: None,
            ..
        } if text.trim().is_empty()
    )
}

pub(super) fn is_transparent_notice(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Notice {
            text,
            is_error: false,
            ..
        } if text.starts_with("[monitor #")
    )
}

pub(super) fn first_tool_id(items: &[TranscriptItem]) -> u64 {
    items
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Tool { id, name, .. } if name != "say" => Some(*id),
            _ => None,
        })
        .unwrap_or(0)
}

/// A finished turn collapses its tools into one line. The tail stays open
/// while the agent is still working.
pub(super) fn settled_activity_folds(
    items: &[TranscriptItem],
    agent_running: bool,
) -> Vec<(usize, usize)> {
    let mut folds = Vec::new();
    let mut body_start = 0usize;
    for i in 0..=items.len() {
        let at_user = i < items.len() && matches!(items[i], TranscriptItem::User { .. });
        if !at_user && i != items.len() {
            continue;
        }
        let body_end = i;
        let is_tail = i == items.len();
        if body_end > body_start && !(is_tail && agent_running) {
            let mut cursor = body_start;
            while cursor < body_end {
                if !is_activity_item(&items[cursor]) {
                    cursor += 1;
                    continue;
                }
                let mut end = cursor + 1;
                while end < body_end && is_activity_item(&items[end]) {
                    end += 1;
                }
                let slice = &items[cursor..end];
                let has_tool = slice
                    .iter()
                    .any(|item| matches!(item, TranscriptItem::Tool { name, .. } if name != "say"));
                let running = slice.iter().any(|item| {
                    matches!(
                        item,
                        TranscriptItem::Tool {
                            status: CardStatus::Running,
                            ..
                        }
                    )
                });
                // An opened card stays on screen; collapsing it again returns
                // the one-line turn summary.
                let opened = slice.iter().any(|item| {
                    matches!(
                        item,
                        TranscriptItem::Tool {
                            name,
                            collapsed: false,
                            ..
                        } if name != "say"
                    )
                });
                if has_tool && !running && !opened {
                    folds.push((cursor, end));
                }
                cursor = end;
            }
        }
        body_start = i + 1;
    }
    folds
}

fn is_activity_item(item: &TranscriptItem) -> bool {
    match item {
        TranscriptItem::Tool { name, .. } if name != "delegate" => true,
        TranscriptItem::Say { .. } => true,
        TranscriptItem::Assistant {
            text,
            done: true,
            error: None,
            ..
        } if text.trim().is_empty() => true,
        TranscriptItem::Notice {
            text,
            is_error: false,
            ..
        } if text.starts_with("[monitor #") => true,
        _ => false,
    }
}

pub(super) fn is_folded_filler(item: &TranscriptItem) -> bool {
    is_transparent_separator(item)
        || matches!(item, TranscriptItem::Tool { name, .. } if name == "say")
}

pub(super) fn is_transparent_separator(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Assistant {
            text,
            reasoning_collapsed: true,
            done: true,
            error: None,
            ..
        } if text.trim().is_empty()
    ) || matches!(
        item,
        TranscriptItem::Notice {
            text,
            is_error: false,
            ..
        } if text.starts_with("[monitor #")
    )
}
