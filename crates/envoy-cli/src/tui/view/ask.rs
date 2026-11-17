//! Bottom-docked Ask / Permission card (keeps transcript and right Inspector visible).
use crate::tui::*;

pub(super) fn ask_inline_height(app: &App, width: u16) -> u16 {
    let Some(ask) = &app.pending_ask else {
        return 0;
    };
    let inner_w = width.saturating_sub(4).max(16) as usize;
    let q_lines = wrap_text(&ask.question, inner_w).len() as u16;
    let mut opt_lines = 0u16;
    for opt in &ask.options {
        opt_lines += 1;
        if opt.description.is_some() {
            opt_lines += 1;
        }
    }
    (q_lines + opt_lines + 4).min(14)
}

pub(super) fn draw_ask(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let Some(ask) = &app.pending_ask else {
        return;
    };
    if area.width < 20 || area.height < 5 {
        return;
    }
    frame.render_widget(Clear, area);

    let left = ask
        .deadline
        .saturating_duration_since(Instant::now())
        .as_secs();
    let title = format!(" ask - {left}s - up/down Enter ");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.warn))
        .style(Style::default().bg(theme.panel))
        .title(Span::styled(
            title,
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut lines = Vec::new();
    for wrapped in wrap_text(&ask.question, inner.width.saturating_sub(2) as usize) {
        lines.push(Line::styled(
            format!(" {wrapped}"),
            Style::default().fg(theme.text),
        ));
    }
    lines.push(Line::from(""));

    for (index, option) in ask.options.iter().enumerate() {
        let row_y = inner.y + lines.len() as u16;
        if row_y < inner.y + inner.height {
            app.hit_regions.push(HitRegion {
                rect: Rect {
                    x: inner.x,
                    y: row_y,
                    width: inner.width,
                    height: 1,
                },
                target: HitTarget::SelectAskOption(index),
            });
        }
        let chosen = index == ask.selected;
        let marker = if chosen { " > " } else { "   " };
        let style = if chosen {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };
        let rec = if option.recommended {
            "  (recommended)"
        } else {
            ""
        };
        lines.push(Line::styled(
            format!("{marker}{}{rec}", option.label),
            style,
        ));
        if let Some(desc) = &option.description {
            lines.push(Line::styled(
                format!(
                    "     {}",
                    truncate_width(desc, inner.width.saturating_sub(6) as usize)
                ),
                Style::default().fg(theme.dim),
            ));
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);
}
