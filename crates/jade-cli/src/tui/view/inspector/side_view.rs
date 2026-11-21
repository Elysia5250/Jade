//! Plan sidechat: the main transcript's drawing, plus a composer.
use super::agent_view::push_item;
use crate::tui::*;

pub(super) fn draw_side_view(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let input_h = 3u16.min(area.height.saturating_sub(1));
    let body_h = area.height.saturating_sub(input_h);
    let body = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: body_h,
    };
    let input = Rect {
        x: area.x,
        y: area.y + body_h,
        width: area.width,
        height: input_h,
    };
    draw_body(frame, app, theme, body);
    draw_input(frame, app, theme, input);
}

fn draw_body(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    if area.height == 0 {
        return;
    }
    let width = area.width.max(16) as usize;
    let mut lines = Vec::new();
    if app.ui.side_items.is_empty() {
        lines.push(LinkedLine::from_line(Line::styled(
            "  plan sidechat",
            Style::default().fg(theme.dim),
        )));
    }
    for item in &app.ui.side_items {
        push_item(app, item, width, theme, &mut lines);
    }
    let max = lines.len().saturating_sub(area.height as usize);
    let pinned = app.ui.inspector.scroll >= app.ui.inspector.max_scroll;
    app.ui.inspector.max_scroll = max;
    app.ui.inspector.scroll = if pinned {
        max
    } else {
        app.ui.inspector.scroll.min(max)
    };
    super::push_linked_rows(frame, app, &lines, app.ui.inspector.scroll, area);
}

fn draw_input(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let focused = app.ui.focus == Focus::Side;
    let color = if focused { theme.accent } else { theme.border };
    let (text, style, scroll_x, cursor_col) = if app.ui.side_input.is_empty() {
        let hint = if focused { "plan…" } else { "" };
        (hint.to_string(), Style::default().fg(theme.dim), 0u16, 0u16)
    } else {
        let prefix: String = app.ui.side_input.chars().take(app.ui.side_cursor).collect();
        let cursor_w = display_width(&prefix);
        let max_w = area.width.saturating_sub(1) as usize;
        let scroll = if max_w > 0 && cursor_w > max_w {
            cursor_w - max_w
        } else {
            0
        };
        let col = cursor_w.saturating_sub(scroll).min(max_w);
        (
            app.ui.side_input.clone(),
            Style::default().fg(theme.text),
            scroll.min(u16::MAX as usize) as u16,
            col as u16,
        )
    };
    frame.render_widget(
        Paragraph::new(Line::styled(text, style))
            .scroll((0, scroll_x))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(color)),
            ),
        area,
    );
    if focused && area.width > 1 && area.height > 1 {
        frame.set_cursor_position((area.x + cursor_col, area.y + 1));
    }
}
