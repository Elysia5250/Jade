//! Sub-panels for the Hub: directory browser, naming/rename, skills, memory, and MCP servers.
use crate::tui::*;

fn popup_inner(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    area: Rect,
    height: u16,
    title: &str,
) -> Rect {
    let width = 84u16
        .min(area.width.saturating_sub(4).max(1))
        .min(area.width.max(1));
    let height = height.min(area.height.max(1));
    let popup = centered_rect(area, width, height);
    app.ui.modal_rect = Some(popup);
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

fn place_popup_cursor(
    frame: &mut Frame,
    inner: Rect,
    row_offset: u16,
    prefix_w: u16,
    text: &str,
    cursor: usize,
) {
    if inner.width == 0 || row_offset >= inner.height {
        return;
    }
    let col = line_edit::cursor_col(text, cursor) as u16;
    let x = inner.x.saturating_add(prefix_w).saturating_add(col);
    if x < inner.x.saturating_add(inner.width) {
        frame.set_cursor_position((x, inner.y + row_offset));
    }
}

pub(super) fn draw_path(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let (choosing_parent, browse_dir, path, path_cursor, path_selected, err) = {
        let Some(w) = app.ui.welcome.as_ref() else {
            return;
        };
        (
            w.mode == WelcomeMode::NewFolderPickParent,
            w.browse_dir.clone(),
            w.path.clone(),
            w.path_cursor,
            w.path_selected,
            w.error.clone(),
        )
    };
    let entries = app.inputs.welcome_path_entries.clone();
    let dir_count = entries
        .iter()
        .filter(|e| matches!(e, PathBrowserEntry::Dir { .. }))
        .count();
    let title = if choosing_parent {
        format!("new folder · parent ({dir_count} dirs)")
    } else {
        format!("open workspace ({dir_count} dirs)")
    };
    let inner = popup_inner(frame, app, theme, area, 18, &title);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("  dir ", Style::default().fg(theme.dim)),
        Span::styled(
            truncate_width(&collapse_home(&browse_dir), width.saturating_sub(7)),
            Style::default()
                .fg(theme.secondary)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  > ", Style::default().fg(theme.accent)),
        Span::raw(if path.is_empty() {
            String::new()
        } else {
            path.clone()
        }),
        if path.is_empty() {
            Span::styled(
                "type to filter or enter path (~/... or /...)",
                Style::default().fg(theme.dim),
            )
        } else {
            Span::raw("")
        },
    ]));
    lines.push(Line::from(""));

    let reserved = 4 + usize::from(err.is_some());
    let window = (inner.height as usize)
        .saturating_sub(reserved)
        .clamp(1, 10);
    let sel = path_selected.min(entries.len().saturating_sub(1));
    let start = if sel >= window { sel + 1 - window } else { 0 };

    for (offset, entry) in entries.iter().skip(start).take(window).enumerate() {
        let idx = start + offset;
        let row_y = inner.y + lines.len() as u16;
        if row_y < inner.y + inner.height {
            app.ui.hit_regions.push(HitRegion {
                rect: Rect {
                    x: inner.x,
                    y: row_y,
                    width: inner.width,
                    height: 1,
                },
                target: HitTarget::HubPathEntry(idx),
            });
        }
        let label = match entry {
            PathBrowserEntry::OpenCurrent(p) => {
                let verb = if choosing_parent {
                    "create inside"
                } else {
                    "open"
                };
                format!("✓ .  {verb} {}", collapse_home(p))
            }
            PathBrowserEntry::Parent(_) => "← ../  parent directory".to_string(),
            PathBrowserEntry::Dir { display, .. } => format!("  {display}"),
            PathBrowserEntry::TypedPath { display, .. } => format!("→ {display}"),
        };
        let is_sel = idx == sel;
        let marker = if is_sel { "  › " } else { "    " };
        let fg = match entry {
            PathBrowserEntry::OpenCurrent(_) => {
                if is_sel {
                    theme.ok
                } else {
                    theme.text
                }
            }
            PathBrowserEntry::Parent(_) => {
                if is_sel {
                    theme.secondary
                } else {
                    theme.dim
                }
            }
            _ => {
                if is_sel {
                    theme.accent
                } else {
                    theme.text
                }
            }
        };
        let style = if is_sel {
            Style::default()
                .fg(fg)
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(fg)
        };
        let shown = pad_to_width(&truncate_width(&format!("{marker}{label}"), width), width);
        lines.push(Line::styled(shown, style));
    }

    if let Some(err) = &err {
        lines.push(Line::styled(
            format!("  {err}"),
            Style::default().fg(theme.bad),
        ));
    }
    let pos_hint = if entries.len() > window {
        format!("  ({}/{})", sel + 1, entries.len())
    } else {
        String::new()
    };
    push_dim(
        &mut lines,
        theme,
        &format!("  ↑↓ select · Enter enter/open · ←/Bksp up · →/Tab enter · Esc back{pos_hint}"),
    );
    frame.render_widget(Paragraph::new(lines), inner);
    if app.ui.modal.is_none() {
        place_popup_cursor(frame, inner, 1, 4, &path, path_cursor);
    }
}

pub(super) fn draw_naming(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let (mode, folder_parent, draft, draft_cursor, err) = {
        let Some(w) = app.ui.welcome.as_ref() else {
            return;
        };
        (
            w.mode,
            w.folder_parent.clone(),
            w.draft.clone(),
            w.draft_cursor,
            w.error.clone(),
        )
    };
    let (title, label, hint) = match mode {
        WelcomeMode::NewFolderName => (
            "new folder",
            "folder name",
            "Enter create · Ctrl+U clear · Esc back",
        ),
        WelcomeMode::RenameSession => (
            "rename session",
            "title",
            "Enter save · Ctrl+U clear · Esc back",
        ),
        WelcomeMode::SkillsNew => (
            "new skill",
            "skill name",
            "Enter create · Ctrl+U clear · Esc back",
        ),
        _ => (
            "new memory",
            "title",
            "Enter create · Ctrl+U clear · Esc back",
        ),
    };
    let inner = popup_inner(frame, app, theme, area, 10, title);
    let mut lines = Vec::new();
    if let Some(parent) = &folder_parent {
        if mode == WelcomeMode::NewFolderName {
            push_dim(&mut lines, theme, &format!("  {}", collapse_home(parent)));
        }
    }
    lines.push(Line::from(""));
    let input_row = lines.len() as u16;
    let prefix = format!("  {label}  > ");
    lines.push(Line::from(vec![
        Span::styled(format!("  {label}  "), Style::default().fg(theme.dim)),
        Span::styled(">", Style::default().fg(theme.accent)),
        Span::raw(format!(" {draft}")),
    ]));
    if let Some(err) = &err {
        lines.push(Line::styled(
            format!("  {err}"),
            Style::default().fg(theme.bad),
        ));
    }
    lines.push(Line::from(""));
    push_dim(&mut lines, theme, &format!("  {hint}"));
    frame.render_widget(Paragraph::new(lines), inner);
    if app.ui.modal.is_none() {
        place_popup_cursor(
            frame,
            inner,
            input_row,
            display_width(&prefix) as u16,
            &draft,
            draft_cursor,
        );
    }
}

pub(super) fn draw_skills(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let (skills, list_selected, err) = {
        let Some(w) = app.ui.welcome.as_ref() else {
            return;
        };
        (w.skills.clone(), w.list_selected, w.error.clone())
    };
    let height = panel_height(skills.len(), area.height);
    let inner = popup_inner(frame, app, theme, area, height, "skills");
    let width = inner.width as usize;
    let mut lines = Vec::new();
    push_dim(
        &mut lines,
        theme,
        "  Enter toggle · n new · e edit · Esc back",
    );
    lines.push(Line::from(""));
    if skills.is_empty() {
        push_dim(&mut lines, theme, "  no user skills");
    } else {
        let total = skills.len();
        let sel = list_selected.min(total - 1);
        let window = 10usize;
        let start = if sel >= window { sel + 1 - window } else { 0 };
        let mut prev = "";
        for (offset, skill) in skills.iter().skip(start).take(window).enumerate() {
            let idx = start + offset;
            let flag = app.inputs.catalog.welcome_flags.get(idx);
            let kind = flag.map(|flag| flag.kind).unwrap_or("user");
            if kind != prev {
                push_dim(&mut lines, theme, &format!("  {kind}"));
                prev = kind;
            }
            let row_y = inner.y + lines.len() as u16;
            if row_y < inner.y + inner.height {
                app.ui.hit_regions.push(HitRegion {
                    rect: Rect {
                        x: inner.x,
                        y: row_y,
                        width: inner.width,
                        height: 1,
                    },
                    target: HitTarget::HubListItem(idx),
                });
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
    if let Some(err) = &err {
        lines.push(Line::styled(
            format!("  {err}"),
            Style::default().fg(theme.bad),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn draw_memory(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let (notes, list_selected, confirm_delete, err) = {
        let Some(w) = app.ui.welcome.as_ref() else {
            return;
        };
        (
            w.notes.clone(),
            w.list_selected,
            w.confirm_delete.clone(),
            w.error.clone(),
        )
    };
    let height = panel_height(notes.len(), area.height);
    let inner = popup_inner(frame, app, theme, area, height, "memory");
    let width = inner.width as usize;
    let mut lines = Vec::new();
    push_dim(
        &mut lines,
        theme,
        "  Enter toggle · n new · e edit · d delete · Esc back",
    );
    lines.push(Line::from(""));
    if notes.is_empty() {
        push_dim(&mut lines, theme, "  no notes");
    } else {
        let total = notes.len();
        let sel = list_selected.min(total - 1);
        let window = 10usize;
        let start = if sel >= window { sel + 1 - window } else { 0 };
        for (offset, note) in notes.iter().skip(start).take(window).enumerate() {
            let idx = start + offset;
            let row_y = inner.y + lines.len() as u16;
            if row_y < inner.y + inner.height {
                app.ui.hit_regions.push(HitRegion {
                    rect: Rect {
                        x: inner.x,
                        y: row_y,
                        width: inner.width,
                        height: 1,
                    },
                    target: HitTarget::HubListItem(idx),
                });
            }
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
    if let Some(id) = &confirm_delete {
        let title = notes
            .iter()
            .find(|n| &n.id == id)
            .map(|n| n.title.as_str())
            .unwrap_or("note");
        lines.push(Line::styled(
            format!("  Press d again to delete \"{title}\" · Esc cancel"),
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        ));
    } else if let Some(err) = &err {
        lines.push(Line::styled(
            format!("  {err}"),
            Style::default().fg(theme.bad),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn draw_mcp(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let (list_selected, mcp_probing, mcp_probe) = {
        let Some(w) = app.ui.welcome.as_ref() else {
            return;
        };
        (w.list_selected, w.mcp_probing, w.mcp_probe.clone())
    };
    let mcp_len = app.inputs.catalog.mcp.len();
    let mcp_rows: Vec<(String, String, bool)> = app
        .inputs
        .catalog
        .mcp
        .iter()
        .map(|s| (s.name.clone(), s.scope.clone(), s.disabled))
        .collect();
    let extra = mcp_probe.len().min(8);
    let height = panel_height(mcp_len.min(8) + extra + 2, area.height);
    let inner = popup_inner(frame, app, theme, area, height, "mcp servers");
    let width = inner.width as usize;
    let mut lines = Vec::new();
    push_dim(&mut lines, theme, "  p probe live servers · Esc back");
    lines.push(Line::from(""));
    if mcp_rows.is_empty() {
        push_dim(&mut lines, theme, "  no MCP servers configured");
    } else {
        let window = 8usize;
        let sel = list_selected.min(mcp_rows.len() - 1);
        let start = if sel >= window { sel + 1 - window } else { 0 };
        for (offset, (name, scope, disabled)) in
            mcp_rows.iter().skip(start).take(window).enumerate()
        {
            let idx = start + offset;
            let row_y = inner.y + lines.len() as u16;
            if row_y < inner.y + inner.height {
                app.ui.hit_regions.push(HitRegion {
                    rect: Rect {
                        x: inner.x,
                        y: row_y,
                        width: inner.width,
                        height: 1,
                    },
                    target: HitTarget::HubListItem(idx),
                });
            }
            let status = if *disabled { "disabled" } else { "enabled " };
            push_choice(
                &mut lines,
                theme,
                width,
                idx == sel,
                &format!("{:<18} [{:<7}] {status}", name, scope),
            );
        }
    }
    if mcp_probing {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "  probing configured MCP servers...",
            Style::default().fg(theme.accent),
        ));
    } else if !mcp_probe.is_empty() {
        lines.push(Line::from(""));
        push_dim(&mut lines, theme, "  PROBE OUTPUT");
        for row in mcp_probe.iter().take(8) {
            lines.push(Line::styled(
                truncate_width(&format!("  {row}"), width),
                Style::default().fg(theme.text),
            ));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
