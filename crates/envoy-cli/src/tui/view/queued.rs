//! Queued follow-up overlay.
use crate::tui::*;

pub(super) fn draw_queued(frame: &mut Frame, app: &App, theme: &Theme, editor: Rect) {
    if app.agent.queued.is_empty() {
        return;
    }
    let take = app.agent.queued.len().min(2);
    let height = take as u16;
    let area = Rect {
        x: editor.x + 2,
        y: editor.y.saturating_sub(height),
        width: editor.width.saturating_sub(4),
        height,
    };
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let mut lines = Vec::new();
    for text in app.agent.queued.iter().take(take) {
        lines.push(Line::from(vec![
            Span::styled("queued  ", Style::default().fg(theme.warn)),
            Span::styled(
                truncate_width(text, area.width.saturating_sub(10) as usize),
                Style::default().fg(theme.dim),
            ),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        area,
    );
}
