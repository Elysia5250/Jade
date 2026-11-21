//! Jade Hub view: responsive two-column mission control + empty-transcript banner.
use crate::tui::*;

mod controls_pane;
mod panels;
mod sessions_pane;

pub(super) fn draw_workspace_picker(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(mode) = app.ui.welcome.as_ref().map(|w| w.mode) else {
        return;
    };
    if mode.is_naming() {
        panels::draw_naming(frame, app, theme, area);
    } else if mode.is_path_input() {
        panels::draw_path(frame, app, theme, area);
    } else if mode == WelcomeMode::Skills {
        panels::draw_skills(frame, app, theme, area);
    } else if mode == WelcomeMode::Memory {
        panels::draw_memory(frame, app, theme, area);
    } else if mode == WelcomeMode::Mcp {
        panels::draw_mcp(frame, app, theme, area);
    } else {
        draw_hub(frame, app, theme, area);
    }
}

fn draw_hub(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let max_w = 132u16.min(area.width.saturating_sub(2).max(1));
    let max_h = area.height.saturating_sub(2).max(1);
    let container = centered_rect(area, max_w, max_h);
    let wide = container.width >= 100;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(1),
        ])
        .split(container);

    draw_header(frame, app, theme, chunks[0]);
    if wide {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(62),
                Constraint::Length(1),
                Constraint::Percentage(38),
            ])
            .split(chunks[1]);
        sessions_pane::draw_left_pane(frame, app, theme, cols[0], true);
        controls_pane::draw_controls_pane(frame, app, theme, cols[2], true);
    } else {
        let focus = app
            .ui
            .welcome
            .as_ref()
            .map(|w| w.focus)
            .unwrap_or(HubFocus::Left);
        if focus == HubFocus::Controls {
            controls_pane::draw_controls_pane(frame, app, theme, chunks[1], false);
        } else {
            sessions_pane::draw_left_pane(frame, app, theme, chunks[1], false);
        }
    }
    draw_footer(frame, app, theme, chunks[2]);
}

fn draw_header(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let daemon_online = app.inputs.catalog.daemon_online;
    let (dot, dot_color, daemon_lbl) = if daemon_online {
        ("●", theme.ok, "daemon online")
    } else {
        ("○", theme.warn, "daemon stopped")
    };
    let sandbox = if app.inputs.catalog.bwrap_present {
        "sandbox:bwrap".to_string()
    } else {
        format!("sandbox:{}", app.inputs.catalog.sandbox_label)
    };
    let version = env!("CARGO_PKG_VERSION");
    let right_title = format!(" {dot} {daemon_lbl} · {sandbox} · {} ", app.jobs.target);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(vec![
            Span::styled(
                " jade hub ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("v{version} "), Style::default().fg(theme.dim)),
        ]))
        .title_top(
            Line::from(Span::styled(right_title, Style::default().fg(dot_color))).right_aligned(),
        )
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let can_return = app.ui.welcome.as_ref().is_some_and(|w| w.can_return);
    let summary = if app.inputs.catalog.signed_in {
        format!(
            " {}  ·  effort:{}  ·  budget:{}",
            app.inputs.catalog.model_id, app.inputs.catalog.effort_level, app.inputs.catalog.tier
        )
    } else {
        " not signed in — select Provider to connect".to_string()
    };
    let ret_badge = if can_return {
        " [Esc] return to session "
    } else {
        ""
    };
    let ret_w = display_width(ret_badge);
    let left_budget = (inner.width as usize).saturating_sub(ret_w + 1);
    let left_text = truncate_width(&summary, left_budget);
    let gap = (inner.width as usize).saturating_sub(display_width(&left_text) + ret_w);

    let mut spans = vec![Span::styled(
        left_text,
        Style::default().fg(if app.inputs.catalog.signed_in {
            theme.text
        } else {
            theme.warn
        }),
    )];
    spans.push(Span::raw(" ".repeat(gap)));
    if can_return {
        let rx = inner.x + inner.width.saturating_sub(ret_w as u16);
        app.ui.hit_regions.push(HitRegion {
            rect: Rect {
                x: rx,
                y: inner.y,
                width: ret_w as u16,
                height: 1,
            },
            target: HitTarget::HubReturn,
        });
        spans.push(Span::styled(
            ret_badge,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn draw_footer(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(w) = app.ui.welcome.as_ref() else {
        return;
    };
    let line = if let Some(err) = &w.error {
        Line::styled(
            format!(" {err}"),
            Style::default().fg(theme.bad).add_modifier(Modifier::BOLD),
        )
    } else if let Some(id) = &w.confirm_delete {
        let title = w
            .sessions
            .iter()
            .find(|s| &s.id == id)
            .map(|s| s.title.as_str())
            .unwrap_or("session");
        Line::styled(
            format!(" Press d again to delete \"{title}\"  ·  Esc cancel"),
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        )
    } else if w.searching {
        Line::styled(
            " Type to filter  ·  Enter apply  ·  Esc clear",
            Style::default().fg(theme.accent),
        )
    } else if w.focus == HubFocus::Controls {
        Line::styled(
            " ↑↓/jk move  ·  ←→/Enter adjust  ·  Tab sessions  ·  1/2/3 tabs  ·  Ctrl+C quit",
            Style::default().fg(theme.dim),
        )
    } else if w.tab == HubTab::Workspaces {
        Line::styled(
            " ↑↓/jk move  ·  Enter open  ·  / search  ·  n new session  ·  o open path  ·  N new folder  ·  Tab controls",
            Style::default().fg(theme.dim),
        )
    } else {
        let esc = if w.can_return { "  ·  Esc return" } else { "" };
        Line::styled(
            format!(" ↑↓/jk move  ·  Enter open  ·  n new session  ·  / search  ·  r rename  ·  d del  ·  Tab controls{esc}"),
            Style::default().fg(theme.dim),
        )
    };
    frame.render_widget(Paragraph::new(line), area);
}

pub(super) fn fmt_relative_time(updated_ms: u64) -> String {
    if updated_ms == 0 {
        return String::new();
    }
    let now = jade_core::now_ms();
    let secs = now.saturating_sub(updated_ms) / 1000;
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

pub(in crate::tui::view) fn draw_welcome(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let w = area.width as usize;
    let mut rows = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  Jade  ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_width(&app.cwd_label(), w.saturating_sub(12)),
                Style::default().fg(theme.text),
            ),
        ]),
    ];

    if app.inputs.catalog.signed_in {
        rows.push(Line::from(vec![
            Span::styled("  model  ", Style::default().fg(theme.dim)),
            Span::styled(
                format!(
                    "{}  ({} - compact {})",
                    app.inputs.catalog.model_id,
                    app.inputs.catalog.tier,
                    fmt_tokens(app.inputs.catalog.compact_at)
                ),
                Style::default().fg(theme.secondary),
            ),
        ]));
    } else {
        rows.push(Line::from(vec![
            Span::styled("  auth   ", Style::default().fg(theme.dim)),
            Span::styled("sign in with /login", Style::default().fg(theme.warn)),
        ]));
    }

    rows.push(Line::from(vec![
        Span::styled("  tools  ", Style::default().fg(theme.dim)),
        Span::styled(
            format!(
                "{} mcp - {} skills - perm:{}",
                app.inputs.catalog.mcp.len(),
                app.inputs.catalog.skills.len(),
                app.session.perm_mode.label()
            ),
            Style::default().fg(theme.text),
        ),
    ]));
    rows.push(Line::from(""));
    rows.push(Line::styled(
        truncate_width(
            "  Ctrl+X h or /home opens Jade Hub - Shift+Tab permissions - Ctrl+B panel - / commands",
            w,
        ),
        Style::default().fg(theme.dim),
    ));

    frame.render_widget(Paragraph::new(rows), area);
}
