//! Job/session sidebar.
use crate::tui::*;

pub(super) fn draw_sidebar(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let border_color = if app.focus == Focus::Sidebar {
        theme.accent
    } else {
        theme.border
    };
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "jobs",
                Style::default()
                    .fg(theme.dim)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {}", app.jobs.len()), Style::default().fg(theme.dim)),
        ])),
        parts[0],
    );

    let items: Vec<ListItem> = if app.jobs.is_empty() {
        vec![ListItem::new(Line::styled(
            "none",
            Style::default().fg(theme.dim),
        ))]
    } else {
        app.jobs
            .iter()
            .map(|job| {
                let (glyph, style) = match &job.state {
                    JobState::Queued { .. } => ("○", Style::default().fg(theme.warn)),
                    JobState::Running { .. } => ("●", Style::default().fg(theme.ok)),
                    JobState::Done { result, .. } if result.ok() => {
                        ("✓", Style::default().fg(theme.dim))
                    }
                    JobState::Done { .. } => ("✗", Style::default().fg(theme.bad)),
                };
                let cmd = truncate_chars(&job.command(), inner.width.saturating_sub(6) as usize);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{glyph} "), style),
                    Span::raw(cmd),
                ]))
            })
            .collect()
    };
    let mut state = ListState::default();
    if !app.jobs.is_empty() {
        state.select(Some(app.job_selected));
    }
    let list = List::new(items).highlight_style(
        Style::default()
            .bg(theme.selection)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, parts[1], &mut state);
}
