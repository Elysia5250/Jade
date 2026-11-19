//! Path, naming, skills, and memory panels for the home screen.
use super::welcome::{popup_inner, push_choice, push_dim};
use crate::tui::*;

fn panel_height(count: usize, area_h: u16) -> u16 {
    let cap = area_h.max(1);
    let wanted = (count as u16).saturating_add(8);
    wanted.clamp(12.min(cap), cap)
}

pub(super) fn draw_path(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(welcome) = app.welcome.as_ref() else {
        return;
    };
    let inner = popup_inner(frame, theme, area, 16);
    let choosing_parent = welcome.folder_mode && !welcome.path_mode;
    let mut lines = vec![Line::styled(
        if choosing_parent {
            "  Pick a parent"
        } else {
            "  Open path"
        },
        Style::default().fg(theme.accent),
    )];
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
    let Some(welcome) = app.welcome.as_ref() else {
        return;
    };
    let inner = popup_inner(frame, theme, area, 12);
    let (heading, label) = if welcome.folder_mode {
        ("  New folder", "folder name")
    } else if welcome.skills_mode {
        ("  New skill", "skill name")
    } else {
        ("  New memory", "title")
    };
    let mut lines = vec![Line::styled(
        heading,
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    )];
    if let Some(parent) = &welcome.folder_parent {
        push_dim(&mut lines, theme, &format!("  {}", collapse_home(parent)));
    }
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
    push_dim(&mut lines, theme, "  enter create   esc back");
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn draw_skills(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(welcome) = app.welcome.as_ref() else {
        return;
    };
    let height = panel_height(welcome.skills.len(), area.height);
    let inner = popup_inner(frame, theme, area, height);
    let width = inner.width as usize;
    let mut lines = vec![Line::styled(
        "  Skills",
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    )];
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
        let window = 8usize;
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
    let Some(welcome) = app.welcome.as_ref() else {
        return;
    };
    let height = panel_height(welcome.notes.len(), area.height);
    let inner = popup_inner(frame, theme, area, height);
    let width = inner.width as usize;
    let mut lines = vec![Line::styled(
        "  Memory",
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    )];
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
        let window = 8usize;
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
