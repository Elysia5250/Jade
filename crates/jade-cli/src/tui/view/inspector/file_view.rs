//! Inspector Tab 3: Syntax-highlighted, line-numbered workspace file previewer.
use super::{push_linked_rows, render_scrolled_lines};
use crate::tui::highlight::{highlight_code_line, Lang};
use crate::tui::inputs::PreviewBody;
use crate::tui::links::{display_rel, LinkedSpan};
use crate::tui::*;
use std::path::Path;

pub(super) fn draw_file_view(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    if area.height < 3 {
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(area);
    let header_area = chunks[0];
    let body_area = chunks[1];
    app.ui.inspector.height = body_area.height;

    let Some(raw_path) = app.ui.inspector.file_path.clone() else {
        app.ui.inspector.max_scroll = 0;
        app.ui.inspector.scroll = 0;
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::styled(
                    "  Click any underlined file link in the chat",
                    Style::default().fg(theme.dim),
                ),
                Line::styled(
                    "  or run /open <path[:line]> to preview.",
                    Style::default().fg(theme.dim),
                ),
            ]),
            body_area,
        );
        return;
    };

    let (rel, body) = preview_text(app, &raw_path);
    let lang = Lang::from_path(Path::new(&rel));
    let is_md = lang == Lang::Markdown;
    let render_md = is_md && !app.ui.inspector.markdown_raw;
    let has_diff = app.inputs.has_diff;

    let line_label = app
        .ui
        .inspector
        .file_line
        .map(|l| format!(":{l}"))
        .unwrap_or_default();
    let md_label = if !is_md {
        ""
    } else if render_md {
        "[Raw]"
    } else {
        "[Preview]"
    };
    let right_reserve =
        2 + if is_md { md_label.len() + 2 } else { 0 } + if has_diff { 8 } else { 0 };
    let header_title = truncate_width(
        &format!("{rel}{line_label}"),
        header_area.width.saturating_sub(right_reserve as u16) as usize,
    );

    let mut header_spans = vec![LinkedSpan {
        span: Span::styled(
            format!(" {header_title}"),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        target: None,
    }];
    if is_md {
        header_spans.push(LinkedSpan {
            span: Span::raw("  "),
            target: None,
        });
        header_spans.push(LinkedSpan {
            span: Span::styled(
                md_label,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            target: Some(HitTarget::ToggleFileMarkdown),
        });
    }
    if has_diff {
        header_spans.push(LinkedSpan {
            span: Span::raw("  "),
            target: None,
        });
        header_spans.push(LinkedSpan {
            span: Span::styled(
                "[Diff]",
                Style::default()
                    .fg(theme.link)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            target: Some(HitTarget::SelectInspectorTab(InspectorTab::Diff)),
        });
    }
    let sep = "-".repeat(header_area.width as usize);
    push_linked_rows(
        frame,
        app,
        &[
            LinkedLine::from_linked(header_spans),
            LinkedLine::from_line(Line::styled(sep, Style::default().fg(theme.border))),
        ],
        0,
        header_area,
    );

    let FileBody::Text(text) = body else {
        app.ui.inspector.max_scroll = 0;
        app.ui.inspector.scroll = 0;
        let (message, bad) = match body {
            FileBody::Missing(path) => (format!("  cannot read {path}"), true),
            FileBody::Loading => ("  loading preview".to_string(), false),
            FileBody::Binary(bytes) => (format!("  binary file ({bytes} bytes)"), false),
            FileBody::Text(_) => (String::new(), false),
        };
        let color = if bad { theme.bad } else { theme.dim };
        frame.render_widget(
            Paragraph::new(Line::styled(message, Style::default().fg(color))),
            body_area,
        );
        return;
    };

    if render_md {
        let md_w = body_area.width.saturating_sub(1).max(12) as usize;
        let mut rows: Vec<LinkedLine> = Vec::new();
        if app.inputs.truncated {
            rows.push(LinkedLine::from_line(Line::styled(
                "  [preview truncated at 512 KiB]",
                Style::default().fg(theme.warn),
            )));
        }
        rows.extend(render_markdown_linked_with_code_bg(
            &text,
            md_w,
            theme,
            &app.session.current.cwd,
            &app.session.current.changed_files,
            &app.session.workspace_index,
            theme.bg,
        ));
        let max_scroll = rows.len().saturating_sub(body_area.height as usize);
        app.ui.inspector.max_scroll = max_scroll;
        app.ui.inspector.scroll = app.ui.inspector.scroll.min(max_scroll);
        push_linked_rows(frame, app, &rows, app.ui.inspector.scroll, body_area);
        return;
    }

    let target_line = app.ui.inspector.file_line;

    let mut lines: Vec<Line<'static>> = Vec::new();
    if app.inputs.truncated {
        lines.push(Line::styled(
            "  [preview truncated at 512 KiB]",
            Style::default().fg(theme.warn),
        ));
    }
    for (idx, raw) in text.lines().enumerate() {
        let lineno = idx + 1;
        let is_target = target_line == Some(lineno);
        let bg = if is_target {
            theme.selection
        } else {
            theme.panel
        };
        let num_fg = if is_target { theme.accent } else { theme.dim };
        let gutter = format!("{lineno:>4} | ");
        let gutter_w = display_width(&gutter);
        let shown = truncate_width(
            raw,
            (body_area.width as usize).saturating_sub(gutter_w).max(1),
        );
        let mut spans = vec![Span::styled(gutter, Style::default().fg(num_fg).bg(bg))];
        let code_spans =
            highlight_code_line(&shown, lang, theme, Style::default().fg(theme.text).bg(bg));
        for sp in code_spans {
            spans.push(Span::styled(sp.content.into_owned(), sp.style.bg(bg)));
        }
        let used = gutter_w + display_width(&shown);
        if used < body_area.width as usize {
            spans.push(Span::styled(
                " ".repeat(body_area.width as usize - used),
                Style::default().bg(bg),
            ));
        }
        lines.push(Line::from(spans));
    }

    render_scrolled_lines(
        frame,
        lines,
        &mut app.ui.inspector.scroll,
        &mut app.ui.inspector.max_scroll,
        body_area,
    );
}

enum FileBody {
    Missing(String),
    Loading,
    Binary(u64),
    Text(String),
}

fn preview_text(app: &App, raw_path: &str) -> (String, FileBody) {
    match &app.inputs.preview {
        PreviewBody::Text { rel, body } => (rel.clone(), FileBody::Text(body.clone())),
        PreviewBody::Binary { rel, bytes } => (rel.clone(), FileBody::Binary(*bytes)),
        PreviewBody::Missing { rel } => (rel.clone(), FileBody::Missing(rel.clone())),
        PreviewBody::Loading { rel } => (rel.clone(), FileBody::Loading),
        PreviewBody::Absent => (
            display_rel(&app.session.current.cwd, Path::new(raw_path)),
            FileBody::Missing(raw_path.to_string()),
        ),
    }
}
