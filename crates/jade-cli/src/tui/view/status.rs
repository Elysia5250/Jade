//! Bottom control & safety bar: mode, permission, sandbox, trust/delegate badges, and hints.
use super::bar::{render_split_bar, BarSegment};
use crate::tui::*;
use jade_agent::agent::delegate_pref::DelegateLevel;
use jade_agent::agent::voice_pref::VoiceStyle;

pub(super) fn draw_status(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let left = build_left_segments(app, theme);
    let right = build_right_segments(app, theme);
    render_split_bar(frame, app, area, Some(theme.panel), 1, "  ", left, right);
}

fn build_left_segments(app: &App, theme: &Theme) -> Vec<BarSegment> {
    let mut left = Vec::new();
    let (mode, mode_color) = match app.ui.mode {
        InputMode::Agent => ("agent", theme.accent),
        InputMode::Shell => ("shell", theme.warn),
    };
    left.push(BarSegment::clickable(
        vec![Span::styled(
            mode,
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        )],
        HitTarget::CycleMode,
        0,
    ));

    let perm_badge = app.session.perm_mode.short_badge();
    let perm_color = match app.session.perm_mode {
        PermissionMode::Default => theme.dim,
        PermissionMode::AcceptEdits => theme.ok,
        PermissionMode::Plan => theme.accent,
        PermissionMode::Auto => theme.warn,
        PermissionMode::Turbo => theme.bad,
    };
    left.push(BarSegment::clickable(
        vec![Span::styled(perm_badge, Style::default().fg(perm_color))],
        HitTarget::CyclePermission,
        0,
    ));

    let remote = !app.jobs.target.is_empty() && app.jobs.target != "local";
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
    left.push(BarSegment::plain(
        vec![Span::styled(
            sandbox_label,
            Style::default().fg(sandbox_color),
        )],
        1,
    ));

    if !remote && !app.inputs.catalog.trusted {
        left.push(BarSegment::clickable(
            vec![Span::styled("untrusted", Style::default().fg(theme.warn))],
            HitTarget::OpenTrustPicker,
            2,
        ));
    }

    if app.inputs.catalog.delegate_level != DelegateLevel::Normal {
        left.push(BarSegment::plain(
            vec![Span::styled(
                format!("delegate:{}", app.inputs.catalog.delegate_level.as_str()),
                Style::default().fg(theme.secondary),
            )],
            3,
        ));
    }
    if app.inputs.catalog.voice_style != VoiceStyle::Engineer {
        left.push(BarSegment::plain(
            vec![Span::styled(
                format!("voice:{}", app.inputs.catalog.voice_style.as_str()),
                Style::default().fg(theme.secondary),
            )],
            3,
        ));
    }
    left
}

fn build_right_segments(app: &App, theme: &Theme) -> Vec<BarSegment> {
    let mut right = Vec::new();
    if app
        .ui
        .leader_at
        .is_some_and(|at| at.elapsed() <= LEADER_TIMEOUT)
    {
        right.push(BarSegment::plain(
            vec![Span::styled(
                "Ctrl+X …  h hub · n new · m models · 1-5 tab · M mouse · b panel",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )],
            0,
        ));
        return right;
    }

    if !app.ui.mouse_capture {
        right.push(BarSegment::plain(
            vec![Span::styled(
                "mouse:off",
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            )],
            0,
        ));
    }
    if app.ui.side_open {
        right.push(BarSegment::clickable(
            vec![Span::styled("plan-chat", Style::default().fg(theme.accent))],
            HitTarget::SelectInspectorTab(InspectorTab::Side),
            1,
        ));
    }
    right.push(BarSegment::plain(
        vec![Span::styled(
            "Shift+Tab perm · Ctrl+B panel",
            Style::default().fg(theme.dim),
        )],
        3,
    ));
    right.push(BarSegment::plain(
        vec![Span::styled(
            "Ctrl+O fold · Ctrl+X h hub",
            Style::default().fg(theme.dim),
        )],
        4,
    ));
    right
}
