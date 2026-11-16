//! Ask modal.
use crate::tui::*;

pub(super) fn draw_ask(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(ask) = &app.pending_ask else {
        return;
    };
    let width = 64u16.min(area.width.saturating_sub(4));
    let height = (ask.options.len() as u16 + 8).min(area.height.saturating_sub(2));
    if width < 20 || height < 5 {
        return;
    }
    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);
    let mut lines = vec![Line::from("")];
    for wrapped in wrap_text(&ask.question, width.saturating_sub(4) as usize) {
        lines.push(Line::styled(
            format!(" {wrapped}"),
            Style::default().fg(theme.text),
        ));
    }
    lines.push(Line::from(""));
    for (index, option) in ask.options.iter().enumerate() {
        let chosen = index == ask.selected;
        let marker = if chosen { " ▸ " } else { "   " };
        let style = if chosen {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };
        let rec = if option.recommended { "  (recommended)" } else { "" };
        lines.push(Line::styled(format!("{marker}{}{rec}", option.label), style));
        if let Some(desc) = &option.description {
            lines.push(Line::styled(
                format!("     {desc}"),
                Style::default().fg(theme.dim),
            ));
        }
    }
    lines.push(Line::from(""));
    let left = ask
        .deadline
        .saturating_duration_since(Instant::now())
        .as_secs();
    lines.push(Line::styled(
        format!(" {left}s"),
        Style::default().fg(theme.dim),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warn))
        .style(Style::default().bg(theme.panel))
        .title(Span::styled(" ask ", Style::default().fg(theme.warn)));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}
