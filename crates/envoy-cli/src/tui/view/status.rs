//! Status line with clickable mode/permission badges and context bar.
use crate::tui::*;

pub(super) fn draw_status(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (mode, mode_color) = match app.ui.mode {
        InputMode::Agent => ("agent", theme.accent),
        InputMode::Shell => ("shell", theme.warn),
    };

    let signed_in = app.inputs.catalog.signed_in;
    let model_id = app.inputs.catalog.model_id.clone();
    let compact_at = app.inputs.catalog.compact_at;
    let hard_window = app.inputs.catalog.hard_window;
    let mut right_spans: Vec<Span> = Vec::new();
    let unauthed = if signed_in {
        let (used, estimated) = context_used(&app.session.current);
        let window = compact_at;
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
        if app.agent.running {
            right_spans.extend(decrypt_line_spans(
                "working",
                0,
                now_ms(),
                0x5354_4154,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(theme.dim),
            ));
            right_spans.push(Span::raw("  "));
        }
        right_spans.push(Span::styled(
            model_id,
            Style::default().fg(theme.secondary),
        ));
        right_spans.push(Span::raw("  "));
        let (filled, empty) = context_bar(used, window);
        right_spans.push(Span::styled(filled, Style::default().fg(bar_color)));
        right_spans.push(Span::styled(empty, Style::default().fg(theme.border)));
        let mark = if estimated { "~" } else { "" };
        let cap_suffix = if compact_at + 64_000 < hard_window {
            format!("~{}", fmt_tokens(hard_window))
        } else {
            String::new()
        };
        right_spans.push(Span::styled(
            format!(
                "  {mark}{}/{}{cap_suffix}",
                fmt_tokens(used),
                fmt_tokens(window)
            ),
            Style::default().fg(theme.dim),
        ));
        false
    } else {
        right_spans.push(Span::styled(
            "/login to sign in",
            Style::default()
                .fg(theme.warn)
                .add_modifier(Modifier::UNDERLINED),
        ));
        true
    };

    let right = Line::from(right_spans);
    let right_w = (right.width() as u16).min(area.width.saturating_sub(4));
    let left_budget = area.width.saturating_sub(right_w + 1) as usize;
    if unauthed && right_w > 0 && area.width >= right_w {
        app.ui.hit_regions.push(HitRegion {
            rect: Rect {
                x: area.x + area.width - right_w,
                y: area.y,
                width: right_w,
                height: 1,
            },
            target: HitTarget::OpenLogin,
        });
    }

    let remote = app.jobs.target != "local" && !app.jobs.target.is_empty();
    let offline = app.agent.offline || !app.jobs.connected;
    let perm_badge = app.session.perm_mode.short_badge();
    let perm_color = match app.session.perm_mode {
        PermissionMode::Default => theme.dim,
        PermissionMode::AcceptEdits => theme.ok,
        PermissionMode::Plan => theme.accent,
        PermissionMode::Auto => theme.warn,
    };
    let sandbox_label = if remote {
        "sandbox:peer".to_string()
    } else if app.inputs.catalog.bwrap_present {
        "sandbox:bwrap".to_string()
    } else {
        format!("sandbox:{}", app.inputs.catalog.sandbox_label)
    };
    let sandbox_color = if sandbox_label.ends_with(":none") || sandbox_label.ends_with(":off") {
        theme.warn
    } else if sandbox_label.ends_with(":peer") {
        theme.secondary
    } else {
        theme.ok
    };
    let toast = app
        .active_toast()
        .map(|(msg, err)| (format!("  {}", truncate_width(msg, 36)), err));
    let active_monitors = app.agent.active_monitors.iter().filter(|m| m.active).count();
    let mon_label = if active_monitors > 0 {
        format!("  mon:{active_monitors}")
    } else {
        String::new()
    };
    let target_label = format!("  {}", app.jobs.target);
    let offline_label = "  offline";
    let suffix_w = if let Some((label, _)) = &toast {
        display_width(label)
    } else {
        display_width(&mon_label)
            + (if remote {
                display_width(&target_label)
            } else {
                0
            })
            + (if offline {
                display_width(offline_label)
            } else {
                0
            })
    };
    let sandbox_w = display_width(&sandbox_label) + 2;
    let mode_w = display_width(mode) + 2 + display_width(perm_badge) + 2 + sandbox_w;
    let cwd_budget = left_budget.saturating_sub(mode_w + suffix_w).max(1);
    let cwd = truncate_width(&app.cwd_label(), cwd_budget);

    let mode_w_cells = (display_width(mode) as u16).min(area.width);
    if mode_w_cells > 0 {
        app.ui.hit_regions.push(HitRegion {
            rect: Rect {
                x: area.x,
                y: area.y,
                width: mode_w_cells,
                height: 1,
            },
            target: HitTarget::CycleMode,
        });
    }
    let perm_x = area.x + mode_w_cells + 2;
    if perm_x < area.x + area.width {
        let perm_w_cells = (display_width(perm_badge) as u16)
            .min(area.x + area.width - perm_x);
        if perm_w_cells > 0 {
            app.ui.hit_regions.push(HitRegion {
                rect: Rect {
                    x: perm_x,
                    y: area.y,
                    width: perm_w_cells,
                    height: 1,
                },
                target: HitTarget::CyclePermission,
            });
        }
    }

    let mut left = vec![
        Span::styled(
            mode,
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(perm_badge, Style::default().fg(perm_color)),
        Span::raw("  "),
    ];
    left.push(Span::styled(
        sandbox_label,
        Style::default().fg(sandbox_color),
    ));
    left.push(Span::raw("  "));
    left.push(Span::styled(cwd.clone(), Style::default().fg(theme.text)));
    if let Some((label, err)) = toast {
        left.push(Span::styled(
            label,
            Style::default().fg(if err { theme.bad } else { theme.ok }),
        ));
    } else {
        if !mon_label.is_empty() {
            left.push(Span::styled(mon_label, Style::default().fg(theme.accent)));
        }
        if remote && left_budget > mode_w + display_width(&cwd) + 4 {
            left.push(Span::styled(
                target_label,
                Style::default().fg(theme.secondary),
            ));
        }
        if offline {
            left.push(Span::styled(offline_label, Style::default().fg(theme.bad)));
        }
    }

    let right_w_cells: usize = right.spans.iter().map(|s| display_width(&s.content)).sum();
    let max_left = (area.width as usize)
        .saturating_sub(right_w_cells)
        .saturating_sub(1);
    let mut left_w: usize = left.iter().map(|s| display_width(&s.content)).sum();
    while left_w > max_left && left.len() > 1 {
        left.pop();
        left_w = left.iter().map(|s| display_width(&s.content)).sum();
    }
    let gap = (area.width as usize).saturating_sub(left_w + right_w_cells);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right.spans);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
