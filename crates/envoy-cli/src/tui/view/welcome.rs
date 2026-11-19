//! Home screen drawn before the console attaches.
use crate::tui::*;

pub(super) fn draw_workspace_picker(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let (naming, path_mode, folder_mode, skills_mode, memory_mode) = {
        let Some(welcome) = app.welcome.as_ref() else {
            return;
        };
        (
            welcome.naming,
            welcome.path_mode,
            welcome.folder_mode,
            welcome.skills_mode,
            welcome.memory_mode,
        )
    };
    if naming {
        super::welcome_panels::draw_naming(frame, app, theme, area);
    } else if path_mode || folder_mode {
        super::welcome_panels::draw_path(frame, app, theme, area);
    } else if skills_mode {
        super::welcome_panels::draw_skills(frame, app, theme, area);
    } else if memory_mode {
        super::welcome_panels::draw_memory(frame, app, theme, area);
    } else {
        draw_home(frame, app, theme, area);
    }
}

pub(super) fn popup_inner(frame: &mut Frame, theme: &Theme, area: Rect, height: u16) -> Rect {
    let width = 84u16
        .min(area.width.saturating_sub(4).max(1))
        .min(area.width.max(1));
    let height = height.min(area.height.max(1));
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
    inner
}

pub(super) fn push_choice(
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
    width: usize,
    selected: bool,
    text: &str,
) {
    let marker = if selected { "> " } else { "  " };
    let style = if selected {
        Style::default()
            .fg(theme.bg)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let shown = format!(" {marker}{text}");
    lines.push(Line::styled(pad_to_width(&shown, width), style));
}

pub(super) fn push_dim(lines: &mut Vec<Line<'static>>, theme: &Theme, text: &str) {
    lines.push(Line::styled(
        text.to_string(),
        Style::default().fg(theme.dim),
    ));
}

fn draw_home(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(welcome) = app.welcome.as_ref() else {
        return;
    };
    let height = (welcome.rows.len() as u16).saturating_add(14);
    let inner = popup_inner(frame, theme, area, height);
    let width = inner.width as usize;
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
    let mut section = "";
    for (idx, row) in welcome.rows.iter().enumerate() {
        let next = match row {
            WelcomeRow::Here(_, _)
            | WelcomeRow::Recent(_, _)
            | WelcomeRow::OpenPath
            | WelcomeRow::NewFolder => "workspaces",
            WelcomeRow::Account => "account",
            WelcomeRow::Skills => "skills",
            WelcomeRow::Memory => "memory",
        };
        if next != section {
            push_dim(&mut lines, theme, &format!("  {next}"));
            section = next;
        }
        push_choice(
            &mut lines,
            theme,
            width,
            idx == welcome.selected,
            &row_text(app, welcome, row),
        );
        if matches!(row, WelcomeRow::Account) {
            let status = if daemon::socket_path(&app.data_dir).exists() {
                "daemon online"
            } else {
                "daemon stopped"
            };
            push_dim(&mut lines, theme, &format!("    {status}"));
        }
    }
    if let Some(err) = &welcome.error {
        lines.push(Line::styled(
            format!("  {err}"),
            Style::default().fg(theme.bad),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn row_text(app: &App, welcome: &Welcome, row: &WelcomeRow) -> String {
    match row {
        WelcomeRow::Here(path, resumes) => {
            if *resumes {
                format!("current   {}  (resumes)", collapse_home(path))
            } else {
                format!("current   {}", collapse_home(path))
            }
        }
        WelcomeRow::Recent(_, label) => format!("recent    {label}  (resumes)"),
        WelcomeRow::OpenPath => "open a path...".to_string(),
        WelcomeRow::NewFolder => "new folder...".to_string(),
        WelcomeRow::Account => account_text(app),
        WelcomeRow::Skills => format!("skills    {}", welcome.skill_count),
        WelcomeRow::Memory => format!("memory    {}", welcome.memory_count),
    }
}

fn account_text(app: &App) -> String {
    match resolve_active_provider(&app.data_dir) {
        Some((prov, model)) => format!("account   {}/{}", prov.id, model.model_id),
        None => "account   sign in".to_string(),
    }
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
