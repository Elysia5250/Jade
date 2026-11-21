//! `delegating` block: one indented row per child, with a status diamond.
use crate::tui::links::LinkedSpan;
use crate::tui::*;

pub(super) fn status_diamond(status: CardStatus, theme: &Theme) -> Span<'static> {
    let (color, bold) = match status {
        CardStatus::Running => {
            let on = (now_ms() / 480).is_multiple_of(2);
            (if on { theme.ok } else { theme.dim }, on)
        }
        CardStatus::Done => (theme.ok, true),
        CardStatus::Error => (theme.bad, true),
        CardStatus::Queued => (theme.dim, false),
    };
    let mut style = Style::default().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    Span::styled("◆", style)
}

pub(super) fn render_delegate_rows(
    lanes: Option<&[SubagentLane]>,
    width: usize,
    theme: &Theme,
    call_id: &str,
) -> Vec<LinkedLine> {
    let mut lines = vec![LinkedLine::from_line(Line::styled(
        "  delegating",
        Style::default().fg(theme.text),
    ))];
    let Some(lanes) = lanes else {
        return lines;
    };
    for lane in lanes {
        lines.push(lane_row(lane, width, theme, call_id));
    }
    lines
}

fn lane_row(lane: &SubagentLane, width: usize, theme: &Theme, call_id: &str) -> LinkedLine {
    let target = HitTarget::OpenSubagent {
        call_id: call_id.to_string(),
        index: lane.index,
    };
    let label = format!("subagent {}", lane.index);
    let hint = lane_hint(lane);
    let room = width.saturating_sub(8 + display_width(&label));
    let hint = truncate_width(&hint, room);
    let mut spans = vec![
        LinkedSpan {
            span: Span::raw("    "),
            target: Some(target.clone()),
        },
        LinkedSpan {
            span: status_diamond(lane.status, theme),
            target: Some(target.clone()),
        },
        LinkedSpan {
            span: Span::styled(format!(" {label}"), Style::default().fg(theme.text)),
            target: Some(target.clone()),
        },
    ];
    if !hint.is_empty() {
        spans.push(LinkedSpan {
            span: Span::styled(format!("  {hint}"), Style::default().fg(theme.dim)),
            target: Some(target),
        });
    }
    LinkedLine::from_linked(spans)
}

fn lane_hint(lane: &SubagentLane) -> String {
    if let Some(hint) = lane.items.iter().rev().find_map(tool_label) {
        return hint;
    }
    if !lane.description.is_empty() {
        return lane.description.clone();
    }
    lane.name.clone()
}

fn tool_label(item: &TranscriptItem) -> Option<String> {
    let TranscriptItem::Tool {
        title, subtitle, ..
    } = item
    else {
        return None;
    };
    if subtitle.is_empty() {
        Some(title.clone())
    } else {
        Some(format!("{title} {subtitle}"))
    }
}
