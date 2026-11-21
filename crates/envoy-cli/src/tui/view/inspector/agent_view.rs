//! Right-hand replica of the main transcript for one delegated child.
//! The composer stays on the left; this pane only shows that child's work.
use super::super::delegate::status_diamond;
use super::super::transcript::{AGENT_MARK, SAY_MARK};
use super::super::transcript_tool::render_tool_header;
use super::push_linked_rows;
use crate::tui::links::LinkedSpan;
use crate::tui::*;

pub(super) fn draw_agent_view(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let Some((call_id, index)) = app.ui.inspector.subagent.clone() else {
        draw_note(frame, theme, area, "No subagent selected.");
        return;
    };
    let lane = app
        .session
        .current
        .subagents
        .get(&call_id)
        .and_then(|lanes| lanes.iter().find(|lane| lane.index == index))
        .cloned();
    let Some(lane) = lane else {
        draw_note(frame, theme, area, "This subagent has no transcript yet.");
        return;
    };

    let width = area.width.max(16) as usize;
    let mut lines = header_lines(&lane, width, theme);
    for item in &lane.items {
        push_item(app, item, width, theme, &mut lines);
    }
    if lines.is_empty() {
        lines.push(LinkedLine::from_line(Line::styled(
            "  (waiting)",
            Style::default().fg(theme.dim),
        )));
    }

    let max = lines.len().saturating_sub(area.height as usize);
    let pinned = app.ui.inspector.scroll >= app.ui.inspector.max_scroll;
    app.ui.inspector.max_scroll = max;
    app.ui.inspector.scroll = if pinned {
        max
    } else {
        app.ui.inspector.scroll.min(max)
    };
    push_linked_rows(frame, app, &lines, app.ui.inspector.scroll, area);
}

fn header_lines(lane: &SubagentLane, width: usize, theme: &Theme) -> Vec<LinkedLine> {
    let state = match lane.status {
        CardStatus::Running => "running",
        CardStatus::Done => "done",
        CardStatus::Error => "stopped",
        CardStatus::Queued => "queued",
    };
    let title = format!(" subagent {}  {}  {state}", lane.index, lane.name);
    let mut lines = vec![LinkedLine::from_linked(vec![
        LinkedSpan {
            span: status_diamond(lane.status, theme),
            target: None,
        },
        LinkedSpan {
            span: Span::styled(
                truncate_width(&title, width.saturating_sub(2)),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
            target: None,
        },
    ])];
    if !lane.description.is_empty() {
        lines.push(LinkedLine::from_line(Line::styled(
            format!(
                "  {}",
                truncate_width(&lane.description, width.saturating_sub(2))
            ),
            Style::default().fg(theme.dim),
        )));
    }
    lines.push(LinkedLine::from_line(Line::styled(
        "-".repeat(width),
        Style::default().fg(theme.border),
    )));
    lines
}

pub(super) fn push_item(
    app: &App,
    item: &TranscriptItem,
    width: usize,
    theme: &Theme,
    lines: &mut Vec<LinkedLine>,
) {
    match item {
        TranscriptItem::User { text, .. } => push_user(text, width, theme, lines),
        TranscriptItem::Assistant {
            text, done, error, ..
        } => {
            push_assistant(app, text, *done, error.as_deref(), width, theme, lines);
        }
        TranscriptItem::Tool {
            id,
            name,
            title,
            subtitle,
            target,
            output,
            status,
            collapsed,
            duration_ms,
            exit_code,
            started_ms,
            ..
        } => {
            lines.push(render_tool_header(
                app,
                *id,
                name,
                title,
                subtitle,
                target,
                output,
                *status,
                *collapsed,
                *duration_ms,
                *exit_code,
                *started_ms,
                width,
                theme,
                false,
            ));
        }
        TranscriptItem::Say { text, .. } => {
            lines.push(mark_line(theme, text, width));
        }
        TranscriptItem::Notice { .. } => {}
    }
}

fn push_user(text: &str, width: usize, theme: &Theme, lines: &mut Vec<LinkedLine>) {
    let bar = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let body = Style::default().fg(theme.text).add_modifier(Modifier::BOLD);
    let raw: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.lines().collect()
    };
    for raw_line in raw {
        for wrapped in wrap_text(raw_line, width.saturating_sub(3)) {
            lines.push(LinkedLine::from_line(Line::from(vec![
                Span::styled("▌ ", bar),
                Span::styled(wrapped, body),
            ])));
        }
    }
}

fn push_assistant(
    app: &App,
    text: &str,
    done: bool,
    error: Option<&str>,
    width: usize,
    theme: &Theme,
    lines: &mut Vec<LinkedLine>,
) {
    let trimmed = text.trim();
    if trimmed.is_empty() && error.is_none() {
        if !done {
            lines.push(LinkedLine::from_line(Line::styled(
                "  …",
                Style::default().fg(theme.dim),
            )));
        }
        return;
    }
    if !trimmed.is_empty() {
        let rendered = render_markdown_linked_with_code_bg(
            trimmed,
            width.saturating_sub(2).max(8),
            theme,
            &app.session.current.cwd,
            &app.session.current.changed_files,
            &app.session.workspace_index,
            theme.bg,
        );
        let cont = " ".repeat(display_width(AGENT_MARK));
        for (idx, mut row) in rendered.into_iter().enumerate() {
            let prefix = if idx == 0 {
                LinkedSpan {
                    span: Span::styled(
                        AGENT_MARK,
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    target: None,
                }
            } else {
                LinkedSpan {
                    span: Span::raw(cont.clone()),
                    target: None,
                }
            };
            row.spans.insert(0, prefix);
            lines.push(row);
        }
    }
    if let Some(err) = error {
        lines.push(LinkedLine::from_line(Line::styled(
            format!("  {err}"),
            Style::default().fg(theme.bad),
        )));
    }
}

fn mark_line(theme: &Theme, text: &str, width: usize) -> LinkedLine {
    LinkedLine::from_linked(vec![
        LinkedSpan {
            span: Span::styled(
                SAY_MARK,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            target: None,
        },
        LinkedSpan {
            span: Span::styled(
                format!(" {}", truncate_width(text, width.saturating_sub(2))),
                Style::default().fg(theme.text),
            ),
            target: None,
        },
    ])
}

fn draw_note(frame: &mut Frame, theme: &Theme, area: Rect, text: &str) {
    frame.render_widget(
        Paragraph::new(Line::styled(text, Style::default().fg(theme.dim))),
        area,
    );
}
