//! Top context bar: Hub entry, workspace path, git branch/worktree, session title, and diff/panel status.
use super::bar::{render_split_bar, BarSegment};
use crate::tui::*;

pub(super) fn draw_top_bar(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    area: Rect,
    show_sep: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut left = Vec::new();
    left.push(BarSegment::clickable(
        vec![Span::styled(
            "hub",
            Style::default()
                .fg(theme.secondary)
                .add_modifier(Modifier::BOLD),
        )],
        HitTarget::OpenHub,
        0,
    ));

    let cwd_budget = (area.width as usize / 2).clamp(16, 56);
    let cwd = collapse_middle_path(&app.cwd_label(), cwd_budget);
    left.push(BarSegment::plain(
        vec![Span::styled(cwd, Style::default().fg(theme.text))],
        0,
    ));

    if let Some(git_seg) = git_segment(app, theme) {
        left.push(git_seg);
    }

    let title = app.session.current.title.trim();
    if !title.is_empty() && title != "New session" {
        left.push(BarSegment::plain(
            vec![Span::styled(
                format!("· {}", truncate_width(title, 32)),
                Style::default().fg(theme.dim),
            )],
            3,
        ));
    }

    let mut right = Vec::new();
    if app.agent.offline || !app.jobs.connected {
        right.push(BarSegment::plain(
            vec![Span::styled(
                "● offline",
                Style::default().fg(theme.bad).add_modifier(Modifier::BOLD),
            )],
            0,
        ));
    }

    if !app.jobs.target.is_empty() && app.jobs.target != "local" {
        right.push(BarSegment::plain(
            vec![Span::styled(
                format!("@{}", app.jobs.target),
                Style::default().fg(theme.secondary),
            )],
            1,
        ));
    }

    if !app.session.current.changed_files.is_empty() {
        let count = app.session.current.changed_files.len();
        let added: usize = app
            .session
            .current
            .changed_files
            .iter()
            .map(|f| f.added)
            .sum();
        let removed: usize = app
            .session
            .current
            .changed_files
            .iter()
            .map(|f| f.removed)
            .sum();
        let noun = if count == 1 { "file" } else { "files" };
        right.push(BarSegment::clickable(
            vec![
                Span::styled(format!("{count} {noun} "), Style::default().fg(theme.text)),
                Span::styled(format!("+{added}"), Style::default().fg(theme.ok)),
                Span::styled(format!(" -{removed}"), Style::default().fg(theme.bad)),
            ],
            HitTarget::SelectInspectorTab(InspectorTab::Diff),
            2,
        ));
    }

    let panel_fg = if app.ui.show_sidebar {
        theme.accent
    } else {
        theme.dim
    };
    right.push(BarSegment::clickable(
        vec![Span::styled("[panel]", Style::default().fg(panel_fg))],
        HitTarget::ToggleSidebar,
        1,
    ));

    render_split_bar(
        frame,
        app,
        area,
        Some(theme.panel),
        1,
        "  ",
        left,
        right,
    );

    if show_sep && area.height >= 2 {
        let sep_area = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: 1,
        };
        let sep = "─".repeat(sep_area.width as usize);
        frame.render_widget(
            Paragraph::new(Line::styled(
                sep,
                Style::default().fg(theme.border).bg(theme.bg),
            )),
            sep_area,
        );
    }
}

fn git_segment(app: &App, theme: &Theme) -> Option<BarSegment> {
    let label = if let Some(wt) = &app.inputs.catalog.git_worktree {
        format!("wt:{}", truncate_width(wt, 20))
    } else if let Some(br) = &app.inputs.catalog.git_branch {
        truncate_width(br, 24)
    } else {
        return None;
    };
    let mut spans = vec![
        Span::styled("⎇ ", Style::default().fg(theme.accent)),
        Span::styled(label, Style::default().fg(theme.secondary)),
    ];
    if app.inputs.catalog.git_dirty {
        spans.push(Span::styled(
            "*",
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        ));
    }
    Some(BarSegment::plain(spans, 1))
}
