//! Inspector Tab 2: Diff review view (Unified / Split + Hunk jump + git fallback).
use super::push_linked_rows;
use crate::tui::diff::{render_diff, DiffLayout};
use crate::tui::links::{display_rel, LinkedSpan};
use crate::tui::*;

pub(super) fn draw_diff_view(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
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

    let selected_path = app.inspector.file_path.clone().or_else(|| {
        app.session
            .changed_files
            .get(app.inspector.selected_diff)
            .or_else(|| app.session.changed_files.last())
            .map(|fc| fc.path.clone())
    });

    let Some(raw_path) = selected_path else {
        app.inspector.diff_hunk_lines.clear();
        app.inspector.max_scroll = 0;
        app.inspector.scroll = 0;
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::styled("  No modified files yet.", Style::default().fg(theme.dim)),
                Line::styled(
                    "  Click any edited file link or run /diff <path>.",
                    Style::default().fg(theme.dim),
                ),
            ]),
            body_area,
        );
        return;
    };

    let rel = if app.inputs.diff_rel.is_empty() {
        display_rel(&app.session.cwd, std::path::Path::new(&raw_path))
    } else {
        app.inputs.diff_rel.clone()
    };
    let session_match = app
        .find_changed_file_index(&raw_path)
        .and_then(|idx| app.session.changed_files.get(idx))
        .map(|fc| (fc.before.clone(), fc.after.clone()));

    let pair = session_match.or_else(|| app.inputs.diff_git.clone());

    let Some((before, after)) = pair else {
        app.inspector.diff_hunk_lines.clear();
        app.inspector.max_scroll = 0;
        app.inspector.scroll = 0;
        let open_file_row = LinkedLine::from_linked(vec![
            LinkedSpan {
                span: Span::raw("  No diff against session/HEAD. "),
                target: None,
            },
            LinkedSpan {
                span: Span::styled(
                    "[Open File]",
                    Style::default()
                        .fg(theme.link)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                target: Some(HitTarget::OpenFile {
                    path: rel,
                    line: None,
                }),
            },
        ]);
        push_linked_rows(frame, app, &[open_file_row], 0, body_area);
        return;
    };

    let rendered = render_diff(
        &before,
        &after,
        body_area.width as usize,
        app.inspector.diff_layout,
        theme,
    );
    app.inspector.diff_hunk_lines = rendered.hunk_lines;

    // Header: file selector + stats + controls
    let w = header_area.width as usize;
    let layout_label = match app.inspector.diff_layout {
        DiffLayout::Unified => "[v:Split]",
        DiffLayout::Split => "[v:Unified]",
    };
    let stats = format!(" +{} -{} ", rendered.added, rendered.removed);
    let title_budget = w.saturating_sub(display_width(&stats) + 2).max(10);
    let file_title = truncate_width(&rel, title_budget);

    let header_rows = vec![
        LinkedLine::from_linked(vec![
            LinkedSpan {
                span: Span::styled(
                    format!(" {file_title}"),
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ),
                target: None,
            },
            LinkedSpan {
                span: Span::styled(
                    format!(" +{}", rendered.added),
                    Style::default().fg(theme.ok),
                ),
                target: None,
            },
            LinkedSpan {
                span: Span::styled(
                    format!(" -{}", rendered.removed),
                    Style::default().fg(theme.bad),
                ),
                target: None,
            },
        ]),
        LinkedLine::from_linked(vec![
            LinkedSpan {
                span: Span::styled(" ", Style::default()),
                target: None,
            },
            LinkedSpan {
                span: Span::styled(
                    layout_label,
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                target: Some(HitTarget::ToggleDiffLayout),
            },
            LinkedSpan {
                span: Span::raw("  "),
                target: None,
            },
            LinkedSpan {
                span: Span::styled(
                    "[p:Prev]",
                    Style::default()
                        .fg(theme.secondary)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                target: Some(HitTarget::JumpHunk(-1)),
            },
            LinkedSpan {
                span: Span::raw(" "),
                target: None,
            },
            LinkedSpan {
                span: Span::styled(
                    "[n:Next]",
                    Style::default()
                        .fg(theme.secondary)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                target: Some(HitTarget::JumpHunk(1)),
            },
            LinkedSpan {
                span: Span::raw("  "),
                target: None,
            },
            LinkedSpan {
                span: Span::styled(
                    "[File]",
                    Style::default()
                        .fg(theme.link)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                target: Some(HitTarget::SelectInspectorTab(InspectorTab::File)),
            },
        ]),
    ];
    push_linked_rows(frame, app, &header_rows, 0, header_area);

    let max_scroll = (rendered.lines.len() as u16).saturating_sub(body_area.height);
    app.inspector.max_scroll = max_scroll;
    app.inspector.scroll = app.inspector.scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(rendered.lines).scroll((app.inspector.scroll, 0)),
        body_area,
    );
}
