//! Status line.
use crate::tui::*;

pub(super) fn draw_status(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (mode, mode_color) = match app.mode {
        InputMode::Agent => ("agent", theme.accent),
        InputMode::Shell => ("shell", theme.warn),
    };

    let mut right_spans: Vec<Span> = Vec::new();
    if let Some((_, model)) = resolve_active_provider(&app.data_dir) {
        let (used, estimated) = context_used(&app.session);
        let window = context_window(&model.model_id);
        let ratio = if window == 0 {
            0.0
        } else {
            (used as f64 / window as f64).clamp(0.0, 1.0)
        };
        let bar_color = if ratio >= 0.9 {
            theme.bad
        } else if ratio >= 0.7 {
            theme.warn
        } else {
            theme.accent
        };
        if app.agent_running {
            right_spans.push(Span::styled("● ", Style::default().fg(theme.accent)));
        }
        right_spans.push(Span::styled(
            model.model_id.clone(),
            Style::default().fg(theme.secondary),
        ));
        right_spans.push(Span::raw("  "));
        let (filled, empty) = context_bar(used, window);
        right_spans.push(Span::styled(filled, Style::default().fg(bar_color)));
        right_spans.push(Span::styled(empty, Style::default().fg(theme.border)));
        let mark = if estimated { "~" } else { "" };
        right_spans.push(Span::styled(
            format!("  {mark}{}/{}", fmt_tokens(used), fmt_tokens(window)),
            Style::default().fg(theme.dim),
        ));
    } else {
        right_spans.push(Span::styled("sign in", Style::default().fg(theme.warn)));
    }

    let right = Line::from(right_spans);
    let right_w = (right.width() as u16).min(area.width.saturating_sub(4));
    let left_budget = area.width.saturating_sub(right_w + 1) as usize;

    let remote = app.target != "local" && !app.target.is_empty();
    let offline = app.agent_offline || !app.connected;
    let toast = app.active_toast().map(|(msg, err)| {
        (
            format!("  {}", truncate_width(msg, 36)),
            err,
        )
    });
    let target_label = format!("  {}", app.target);
    let offline_label = "  offline";
    let suffix_w = if let Some((label, _)) = &toast {
        display_width(label)
    } else {
        (if remote { display_width(&target_label) } else { 0 })
            + (if offline { display_width(offline_label) } else { 0 })
    };
    let mode_w = display_width(mode) + 2;
    let cwd_budget = left_budget.saturating_sub(mode_w + suffix_w).max(1);
    let cwd = truncate_width(&app.cwd_label(), cwd_budget);

    let mut left = vec![
        Span::styled(
            mode,
            Style::default()
                .fg(mode_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(cwd.clone(), Style::default().fg(theme.text)),
    ];
    if let Some((label, err)) = toast {
        left.push(Span::styled(
            label,
            Style::default().fg(if err { theme.bad } else { theme.ok }),
        ));
    } else {
        if remote && left_budget > mode_w + display_width(&cwd) + 4 {
            left.push(Span::styled(target_label, Style::default().fg(theme.secondary)));
        }
        if offline {
            left.push(Span::styled(offline_label, Style::default().fg(theme.bad)));
        }
    }

    let left_area = Rect {
        width: area.width.saturating_sub(right_w),
        ..area
    };
    frame.render_widget(Paragraph::new(Line::from(left)), left_area);
    if right_w > 0 {
        frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
    }
}
