//! Universal right-hand Inspector drawer (`Overview`, `Diff`, `File`, `Out`, `Info`).
use crate::tui::*;

mod diff_view;
mod file_view;
mod output_view;
mod overview;

pub(in crate::tui) fn draw_inspector(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    app.inspector.last_area = area;
    if area.width < 12 || area.height < 4 {
        return;
    }

    frame.render_widget(
        Block::default().style(Style::default().bg(theme.panel)),
        area,
    );

    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    if inner.width == 0 || inner.height < 3 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    draw_tab_bar(frame, app, theme, chunks[0]);
    let sep = "─".repeat(chunks[1].width as usize);
    frame.render_widget(
        Paragraph::new(Line::styled(sep, Style::default().fg(theme.border))),
        chunks[1],
    );

    let body = chunks[2];
    app.inspector.height = body.height;

    match app.inspector.tab {
        InspectorTab::Overview => overview::draw_overview(frame, app, theme, body),
        InspectorTab::Diff => diff_view::draw_diff_view(frame, app, theme, body),
        InspectorTab::File => file_view::draw_file_view(frame, app, theme, body),
        InspectorTab::Output => output_view::draw_output_view(frame, app, theme, body),
        InspectorTab::Inspect => output_view::draw_inspect_view(frame, app, theme, body),
    }
}

fn draw_tab_bar(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let mut spans: Vec<Span> = Vec::new();
    let mut cursor_x = area.x;
    let focused = app.focus == Focus::Sidebar;

    for tab in InspectorTab::ALL {
        let active = app.inspector.tab == tab;
        let label = if tab == InspectorTab::Diff && !app.session.changed_files.is_empty() {
            format!(" 2 Diff({}) ", app.session.changed_files.len())
        } else {
            format!(" {} ", tab.label())
        };
        let w = display_width(&label) as u16;
        if cursor_x + w > area.x + area.width {
            break;
        }
        let style = if active {
            Style::default()
                .fg(if focused { theme.bg } else { theme.accent })
                .bg(if focused { theme.accent } else { theme.selection })
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.dim)
        };
        app.hit_regions.push(HitRegion {
            rect: Rect {
                x: cursor_x,
                y: area.y,
                width: w,
                height: 1,
            },
            target: HitTarget::SelectInspectorTab(tab),
        });
        spans.push(Span::styled(label, style));
        cursor_x += w;
    }

    let close_label = "[×]";
    let close_w = display_width(close_label) as u16;
    if cursor_x + close_w < area.x + area.width {
        let close_x = area.x + area.width - close_w;
        let pad = close_x.saturating_sub(cursor_x) as usize;
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
        app.hit_regions.push(HitRegion {
            rect: Rect {
                x: close_x,
                y: area.y,
                width: close_w,
                height: 1,
            },
            target: HitTarget::ToggleSidebar,
        });
        spans.push(Span::styled(close_label, Style::default().fg(theme.dim)));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(super) fn push_linked_rows(
    frame: &mut Frame,
    app: &mut App,
    rows: &[LinkedLine],
    scroll: u16,
    area: Rect,
) {
    let start = scroll as usize;
    let end = (start + area.height as usize).min(rows.len());
    let mut display_lines: Vec<Line<'static>> = Vec::with_capacity(end.saturating_sub(start));

    for (vis_idx, row) in rows[start..end].iter().enumerate() {
        let screen_y = area.y + vis_idx as u16;
        let mut cursor_x = area.x;
        let mut line_spans = Vec::with_capacity(row.spans.len());

        for ls in &row.spans {
            let sw = display_width(&ls.span.content) as u16;
            if let Some(target) = &ls.target {
                let clipped_w = sw.min((area.x + area.width).saturating_sub(cursor_x));
                if clipped_w > 0 {
                    app.hit_regions.push(HitRegion {
                        rect: Rect {
                            x: cursor_x,
                            y: screen_y,
                            width: clipped_w,
                            height: 1,
                        },
                        target: target.clone(),
                    });
                }
            }
            cursor_x = cursor_x.saturating_add(sw);
            line_spans.push(ls.span.clone());
        }
        let mut l = Line::from(line_spans);
        if let Some(st) = row.line_style {
            l = l.style(st);
        }
        display_lines.push(l);
    }

    frame.render_widget(Paragraph::new(display_lines), area);
}
