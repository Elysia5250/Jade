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
            "  j/k move   enter open   n new session   ctrl-c quit",
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
        let sel = welcome.path_selected.min(comps.len().saturating_sub(1));
        let window = 6usize;
        let start = if sel >= window { sel + 1 - window } else { 0 };
        for (offset, (label, _)) in comps.iter().skip(start).take(window).enumerate() {
            let idx = start + offset;
            let is_sel = idx == sel;
            let marker = if is_sel { "  › " } else { "    " };
            let style = if is_sel {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.dim)
            };
            lines.push(Line::styled(format!("{marker}{label}"), style));
        }
        if let Some(err) = &welcome.error {
            lines.push(Line::styled(
                format!("  {err}"),
                Style::default().fg(theme.bad),
            ));
        }
        lines.push(Line::styled(
            "  ↑/↓ select   tab completes   enter open   esc back",
            Style::default().fg(theme.dim),
        ));
    } else {
        for (idx, row) in welcome.rows.iter().enumerate() {
            let selected = idx == welcome.selected;
            let marker = if selected { "> " } else { "  " };
            let text = match row {
                WelcomeRow::Here(path, resumes) => {
                    if *resumes {
                        format!("current   {}  (resumes)", collapse_home(path))
                    } else {
                        format!("current   {}", collapse_home(path))
                    }
                }
                WelcomeRow::Recent(_, label) => format!("recent    {label}  (resumes)"),
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

pub(super) fn draw_welcome(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let w = area.width as usize;
    let mut rows = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  Envoy  ",
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

    if let Some((prov, model)) = resolve_active_provider(&app.data_dir) {
        let budget =
            crate::agent::context::resolve_budget(&app.data_dir, &prov.id, &model.model_id);
        rows.push(Line::from(vec![
            Span::styled("  model  ", Style::default().fg(theme.dim)),
            Span::styled(
                format!(
                    "{}  ({} - compact {})",
                    model.model_id,
                    budget.tier,
                    fmt_tokens(budget.compact_at)
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

    let mcp = crate::mcp::config::load_mcp_configs(&app.data_dir, &app.session.cwd);
    let skills = crate::skills::discover_skills(&app.data_dir, &app.session.cwd);
    rows.push(Line::from(vec![
        Span::styled("  tools  ", Style::default().fg(theme.dim)),
        Span::styled(
            format!(
                "{} mcp - {} skills - perm:{}",
                mcp.servers.len(),
                skills.len(),
                app.perm_mode.label()
            ),
            Style::default().fg(theme.text),
        ),
    ]));
    rows.push(Line::from(""));
    rows.push(Line::styled(
        truncate_width(
            "  Click any underlined file link to review in the right Inspector - Drag divider to resize",
            w,
        ),
        Style::default().fg(theme.dim),
    ));
    rows.push(Line::styled(
        truncate_width(
            "  Shift+Tab permissions - Ctrl+B panel - Ctrl+O expand - / commands",
            w,
        ),
        Style::default().fg(theme.dim),
    ));

    frame.render_widget(Paragraph::new(rows), area);
}
