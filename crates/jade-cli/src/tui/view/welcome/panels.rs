//! Sub-panels for the Hub: path picker, naming/rename, skills, memory, and MCP servers.
use crate::tui::*;

fn popup_inner(frame: &mut Frame, theme: &Theme, area: Rect, height: u16, title: &str) -> Rect {
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
            format!(" {title} "),
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

fn panel_height(count: usize, area_h: u16) -> u16 {
    let cap = area_h.max(1);
    let wanted = (count as u16).saturating_add(9);
    wanted.clamp(12.min(cap), cap)
}

fn push_choice(
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
    width: usize,
    selected: bool,
    text: &str,
) {
    let marker = if selected { "▸ " } else { "  " };
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

fn push_dim(lines: &mut Vec<Line<'static>>, theme: &Theme, text: &str) {
    lines.push(Line::styled(
        text.to_string(),
        Style::default().fg(theme.dim),
    ));
}

pub(super) fn draw_path(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(welcome) = app.ui.welcome.as_ref() else {
        return;
    };
    let choosing_parent = welcome.mode == WelcomeMode::NewFolderPickParent;
    let title = if choosing_parent {
        "new folder · parent"
    } else {
        "open path"
    };
    let inner = popup_inner(frame, theme, area, 16, title);
    let mut lines = vec![Line::from(vec![
        Span::styled("  > ", Style::default().fg(theme.accent)),
        Span::raw(welcome.path.clone()),
    ])];
    let comps = &app.inputs.welcome_path_comps;
    let sel = welcome.path_selected.min(comps.len().saturating_sub(1));
    let window = 7usize;
    let start = if sel >= window { sel + 1 - window } else { 0 };
    for (offset, (label, _)) in comps.iter().skip(start).take(window).enumerate() {
        let idx = start + offset;
        let marker = if idx == sel { "  › " } else { "    " };
        let style = if idx == sel {
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
    push_dim(
        &mut lines,
        theme,
        if choosing_parent {
            "  ↑/↓ select   tab completes   enter choose   esc back"
        } else {
            "  ↑/↓ select   tab completes   enter open   esc back"
        },
    );
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn draw_naming(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(welcome) = app.ui.welcome.as_ref() else {
        return;
    };
    let (title, label, hint) = match welcome.mode {
        WelcomeMode::NewFolderName => ("new folder", "folder name", "enter create   esc back"),
        WelcomeMode::RenameSession => ("rename session", "title", "enter save   esc back"),
        WelcomeMode::SkillsNew => ("new skill", "skill name", "enter create   esc back"),
        _ => ("new memory", "title", "enter create   esc back"),
    };
    let inner = popup_inner(frame, theme, area, 10, title);
    let mut lines = Vec::new();
    if let Some(parent) = &welcome.folder_parent {
        if welcome.mode == WelcomeMode::NewFolderName {
            push_dim(&mut lines, theme, &format!("  {}", collapse_home(parent)));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(format!("  {label}  "), Style::default().fg(theme.dim)),
        Span::styled(">", Style::default().fg(theme.accent)),
        Span::raw(format!(" {}", welcome.draft)),
    ]));
    if let Some(err) = &welcome.error {
        lines.push(Line::styled(
            format!("  {err}"),
            Style::default().fg(theme.bad),
        ));
    }
    lines.push(Line::from(""));
    push_dim(&mut lines, theme, &format!("  {hint}"));
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn draw_skills(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(welcome) = app.ui.welcome.as_ref() else {
        return;
    };
    let height = panel_height(welcome.skills.len(), area.height);
    let inner = popup_inner(frame, theme, area, height, "skills");
    let width = inner.width as usize;
    let mut lines = Vec::new();
    push_dim(
        &mut lines,
        theme,
        "  enter toggle   n new   e edit   esc back",
    );
    lines.push(Line::from(""));
    if welcome.skills.is_empty() {
        push_dim(&mut lines, theme, "  no user skills");
    } else {
        let total = welcome.skills.len();
        let sel = welcome.list_selected.min(total - 1);
        let window = 10usize;
        let start = if sel >= window { sel + 1 - window } else { 0 };
        let mut prev = "";
        for (offset, skill) in welcome.skills.iter().skip(start).take(window).enumerate() {
            let idx = start + offset;
            let flag = app.inputs.catalog.welcome_flags.get(idx);
            let kind = flag.map(|flag| flag.kind).unwrap_or("user");
            if kind != prev {
                push_dim(&mut lines, theme, &format!("  {kind}"));
                prev = kind;
            }
            let on = if flag.is_some_and(|flag| flag.injected) {
                "on"
            } else {
                "off"
            };
            push_choice(
                &mut lines,
                theme,
                width,
                idx == sel,
                &format!("{:<8} {}  {on}", kind, skill.name),
            );
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

pub(super) fn draw_memory(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(welcome) = app.ui.welcome.as_ref() else {
        return;
    };
    let height = panel_height(welcome.notes.len(), area.height);
    let inner = popup_inner(frame, theme, area, height, "memory");
    let width = inner.width as usize;
    let mut lines = Vec::new();
    push_dim(
        &mut lines,
        theme,
        "  enter toggle   n new   e edit   d delete   esc back",
    );
    lines.push(Line::from(""));
    if welcome.notes.is_empty() {
        push_dim(&mut lines, theme, "  no notes");
    } else {
        let total = welcome.notes.len();
        let sel = welcome.list_selected.min(total - 1);
        let window = 10usize;
        let start = if sel >= window { sel + 1 - window } else { 0 };
        for (offset, note) in welcome.notes.iter().skip(start).take(window).enumerate() {
            let idx = start + offset;
            let flag = if note.enabled { "on " } else { "off" };
            push_choice(
                &mut lines,
                theme,
                width,
                idx == sel,
                &format!("{flag}  {}", note.title),
            );
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

pub(super) fn draw_mcp(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(welcome) = app.ui.welcome.as_ref() else {
        return;
    };
    let mcp = &app.inputs.catalog.mcp;
    let extra = welcome.mcp_probe.len().min(8);
    let height = panel_height(mcp.len() + extra + 2, area.height);
    let inner = popup_inner(frame, theme, area, height, "mcp servers");
    let width = inner.width as usize;
    let mut lines = Vec::new();
    push_dim(&mut lines, theme, "  p probe live servers   esc back");
    lines.push(Line::from(""));
    if mcp.is_empty() {
        push_dim(&mut lines, theme, "  no MCP servers configured");
    } else {
        let sel = welcome.list_selected.min(mcp.len() - 1);
        for (idx, srv) in mcp.iter().enumerate().take(8) {
            let status = if srv.disabled { "disabled" } else { "enabled " };
            push_choice(
                &mut lines,
                theme,
                width,
                idx == sel,
                &format!("{:<18} [{:<7}] {status}", srv.name, srv.scope),
            );
        }
    }
    if welcome.mcp_probing {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "  probing configured MCP servers...",
            Style::default().fg(theme.accent),
        ));
    } else if !welcome.mcp_probe.is_empty() {
        lines.push(Line::from(""));
        push_dim(&mut lines, theme, "  PROBE OUTPUT");
        for row in welcome.mcp_probe.iter().take(8) {
            lines.push(Line::styled(
                truncate_width(&format!("  {row}"), width),
                Style::default().fg(theme.text),
            ));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
