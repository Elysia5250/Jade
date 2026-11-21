//! Bottom-docked Ask / Permission card (keeps transcript and right Inspector visible).
use crate::tui::*;

pub(super) fn ask_inline_height(app: &App, width: u16) -> u16 {
    let Some(ask) = &app.agent.pending_ask else {
        return 0;
    };
    let inner_w = width.saturating_sub(4).max(16) as usize;
    let q_lines = wrap_text(&ask.question, inner_w).len() as u16;
    let opt_lines: u16 = ask
        .options
        .iter()
        .map(|opt| 1 + u16::from(opt.description.is_some()))
        .sum();
    (q_lines.min(6) + opt_lines + 3)
        .max(ask.options.len() as u16 + 4)
        .min(16)
}

pub(super) fn draw_ask(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let Some(ask) = &app.agent.pending_ask else {
        return;
    };
    if area.width < 20 || area.height < 5 {
        return;
    }
    frame.render_widget(Clear, area);

    let n = ask.options.len().min(9);
    let num_hint = if n > 1 {
        format!("/1-{n}")
    } else {
        String::new()
    };
    let title = match ask.paused_left {
        Some(left) => format!(
            " ask - {}s paused - ↑↓/jk{num_hint} Enter · Ctrl+C cancel ",
            left.as_secs()
        ),
        None => format!(
            " ask - {}s - ↑↓/jk{num_hint} Enter · Ctrl+C cancel ",
            ask.deadline
                .saturating_duration_since(Instant::now())
                .as_secs()
        ),
    };
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

    let inner_h = inner.height as usize;
    let q_width = inner.width.saturating_sub(2).max(8) as usize;
    let wrapped_q = wrap_text(&ask.question, q_width);
    let full_opt_lines: usize = ask
        .options
        .iter()
        .map(|opt| 1 + usize::from(opt.description.is_some()))
        .sum();
    let show_desc = inner_h >= full_opt_lines + 2;
    let opt_lines = if show_desc {
        full_opt_lines
    } else {
        ask.options.len()
    };
    let gap = usize::from(inner_h >= opt_lines + 2);
    let max_q_lines = inner_h.saturating_sub(opt_lines + gap).max(1);
    let q_truncated = wrapped_q.len() > max_q_lines;

    let mut lines = Vec::new();
    for (idx, wrapped) in wrapped_q.into_iter().take(max_q_lines).enumerate() {
        let text = if q_truncated && idx + 1 == max_q_lines {
            format!("{}…", truncate_width(&wrapped, q_width.saturating_sub(1)))
        } else {
            wrapped
        };
        lines.push(Line::styled(
            format!(" {text}"),
            Style::default().fg(theme.text),
        ));
    }
    if gap > 0 {
        lines.push(Line::from(""));
    }

    for (index, option) in ask.options.iter().enumerate() {
        let row_y = inner.y + lines.len() as u16;
        if row_y >= inner.y + inner.height {
            break;
        }
        app.ui.hit_regions.push(HitRegion {
            rect: Rect {
                x: inner.x,
                y: row_y,
                width: inner.width,
                height: 1,
            },
            target: HitTarget::SelectAskOption(index),
        });
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
        let label = format!("{marker}{}. {}{rec}", index + 1, option.label);
        lines.push(Line::styled(
            truncate_width(&label, inner.width as usize),
            style,
        ));
        if show_desc && lines.len() < inner_h {
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
    }

    frame.render_widget(Paragraph::new(lines), inner);
}
