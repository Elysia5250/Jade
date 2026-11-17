//! Workspace picker drawn before the console attaches.
use crate::tui::*;

pub(super) fn draw_workspace_picker(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let Some(welcome) = app.welcome.as_ref() else {
        return;
    };
    let width = 72u16.min(area.width.saturating_sub(4));
    let height = ((welcome.rows.len() as u16) + 8)
        .min(area.height.saturating_sub(2))
        .max(10);
    let popup = centered_rect(area, width, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            " envoy ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.bg));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    let mut lines = vec![
        Line::styled(
            "  Pick a workspace",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            "  j/k move   enter open   ctrl-c quit",
            Style::default().fg(theme.dim),
        ),
        Line::from(""),
    ];
    if welcome.path_mode {
        lines.push(Line::styled(
            "  Open path",
            Style::default().fg(theme.accent),
        ));
        lines.push(Line::from(vec![
            Span::styled("  > ", Style::default().fg(theme.accent)),
            Span::raw(welcome.path.clone()),
        ]));
        let comps = complete_directories(&app.session.cwd, &welcome.path);
        for (i, (label, _)) in comps.iter().take(6).enumerate() {
            let style = if i == 0 {
                Style::default().fg(theme.text)
            } else {
                Style::default().fg(theme.dim)
            };
            lines.push(Line::styled(format!("    {label}"), style));
        }
        if let Some(err) = &welcome.error {
            lines.push(Line::styled(
                format!("  {err}"),
                Style::default().fg(theme.bad),
            ));
        }
        lines.push(Line::styled(
            "  tab completes   esc back",
            Style::default().fg(theme.dim),
        ));
    } else {
        for (idx, row) in welcome.rows.iter().enumerate() {
            let selected = idx == welcome.selected;
            let marker = if selected { "> " } else { "  " };
            let text = match row {
                WelcomeRow::Here(path) => format!("current   {}", collapse_home(path)),
                WelcomeRow::Recent(_, label) => format!("recent    {label}"),
                WelcomeRow::OpenPath => "open a path...".to_string(),
            };
            let style = if selected {
                Style::default()
                    .fg(theme.bg)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let shown = format!(" {marker}{text}");
            let padded = pad_to_width(&shown, inner.width as usize);
            lines.push(Line::styled(padded, style));
        }
        if let Some(err) = &welcome.error {
            lines.push(Line::styled(
                format!("  {err}"),
                Style::default().fg(theme.bad),
            ));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
