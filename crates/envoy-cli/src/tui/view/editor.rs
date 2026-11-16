//! Prompt editor.
use crate::tui::*;

pub(super) fn draw_editor(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    area: Rect,
    visual_lines: &[String],
    skip: usize,
) {
    let mode_color = match app.mode {
        InputMode::Agent => theme.accent,
        InputMode::Shell => theme.warn,
    };
    let frame_color = if app.focus == Focus::Input {
        mode_color
    } else {
        theme.border
    };
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(frame_color))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let max_rows = inner.height as usize;
    let skip = skip.min(visual_lines.len().saturating_sub(max_rows));
    let end = (skip + max_rows).min(visual_lines.len());
    let content = Rect {
        x: inner.x + 1,
        y: inner.y,
        width: inner.width.saturating_sub(2),
        height: inner.height,
    };
    let lines: Vec<Line> = if app.input.is_empty() {
        let placeholder = match app.mode {
            InputMode::Agent => "Ask",
            InputMode::Shell => "Run",
        };
        vec![Line::styled(placeholder, Style::default().fg(theme.dim))]
    } else {
        visual_lines[skip..end]
            .iter()
            .map(|text| Line::raw(text.clone()))
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), content);
}
