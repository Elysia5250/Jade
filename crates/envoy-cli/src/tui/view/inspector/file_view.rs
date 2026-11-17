//! Inspector Tab 3: Syntax-highlighted, line-numbered workspace file previewer.
use super::push_linked_rows;
use crate::tui::highlight::{highlight_code_line, Lang};
use crate::tui::links::{normalize_display_path, LinkedSpan};
use crate::tui::*;
use std::path::Path;

const FILE_PREVIEW_LIMIT: usize = 512 * 1024;

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
    app.inspector.height = body_area.height;

    let Some(raw_path) = app.inspector.file_path.clone() else {
        app.inspector.max_scroll = 0;
        app.inspector.scroll = 0;
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

    let abs = if Path::new(&raw_path).is_absolute() {
        Path::new(&raw_path).to_path_buf()
    } else {
        app.session.cwd.join(&raw_path)
    };
    let rel = normalize_display_path(&app.session.cwd, &abs);
    let has_diff = app.find_changed_file_index(&rel).is_some()
        || crate::tui::diff::load_git_pair(&app.session.cwd, &rel).is_some();

    let line_label = app
        .inspector
        .file_line
        .map(|l| format!(":{l}"))
        .unwrap_or_default();
    let header_title = truncate_width(
        &format!("{rel}{line_label}"),
        header_area.width.saturating_sub(14) as usize,
    );

    let mut header_spans = vec![LinkedSpan {
        span: Span::styled(
            format!(" {header_title}"),
            Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD),
        ),
        target: None,
    }];
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
    let sep = "─".repeat(header_area.width as usize);
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

    let Ok(bytes) = std::fs::read(&abs) else {
        app.inspector.max_scroll = 0;
        app.inspector.scroll = 0;
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("  cannot read {}", abs.display()),
                Style::default().fg(theme.bad),
            )),
            body_area,
        );
        return;
    };

    if bytes.iter().take(4096).any(|&b| b == 0) {
        app.inspector.max_scroll = 0;
        app.inspector.scroll = 0;
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("  binary file ({} bytes)", bytes.len()),
                Style::default().fg(theme.dim),
            )),
            body_area,
        );
        return;
    }

    let capped = if bytes.len() > FILE_PREVIEW_LIMIT {
        &bytes[..FILE_PREVIEW_LIMIT]
    } else {
        &bytes
    };
    let text = String::from_utf8_lossy(capped);
    let lang = Lang::from_path(&abs);
    let target_line = app.inspector.file_line;
    let code_budget = body_area.width.saturating_sub(7).max(8) as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let lineno = idx + 1;
        let is_target = target_line == Some(lineno);
        let bg = if is_target {
            theme.selection
        } else {
            theme.panel
        };
        let num_fg = if is_target { theme.accent } else { theme.dim };
        let shown = truncate_width(raw, code_budget);
        let mut spans = vec![Span::styled(
            format!("{lineno:>4} │ "),
            Style::default().fg(num_fg).bg(bg),
        )];
        let code_spans = highlight_code_line(&shown, lang, theme, Style::default().fg(theme.text).bg(bg));
        for sp in code_spans {
            spans.push(Span::styled(sp.content.into_owned(), sp.style.bg(bg)));
        }
        let used = 7 + display_width(&shown);
        if used < body_area.width as usize {
            spans.push(Span::styled(
                " ".repeat(body_area.width as usize - used),
                Style::default().bg(bg),
            ));
        }
        lines.push(Line::from(spans));
    }

    let max_scroll = (lines.len() as u16).saturating_sub(body_area.height);
    app.inspector.max_scroll = max_scroll;
    app.inspector.scroll = app.inspector.scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(lines).scroll((app.inspector.scroll, 0)),
        body_area,
    );
}
