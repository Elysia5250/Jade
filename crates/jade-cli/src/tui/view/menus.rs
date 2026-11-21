//! Slash and directory completion menus.
use crate::tui::*;

pub(super) fn draw_slash_menu(frame: &mut Frame, app: &App, theme: &Theme, editor: Rect) {
    let items = app.filtered_slash();
    if items.is_empty() {
        return;
    }
    let height = (items.len() as u16 + 2)
        .min(editor.y.saturating_sub(1))
        .min(14);
    if height < 3 {
        return;
    }
    let width = editor.width.saturating_sub(2).min(78);
    let area = Rect {
        x: editor.x + 1,
        y: editor.y.saturating_sub(height),
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let rows: Vec<ListItem> = items
        .iter()
        .map(|cmd| {
            let label = if cmd.args.is_empty() {
                format!("/{:<10} {}", cmd.name, cmd.about)
            } else {
                format!("/{:<10} {:<10} {}", cmd.name, cmd.args, cmd.about)
            };
            ListItem::new(Line::raw(format!(" {label}")))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.ui.menu_selected.min(items.len() - 1)));
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(" commands ", Style::default().fg(theme.dim))),
        )
        .highlight_style(
            Style::default()
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

pub(super) fn draw_cd_menu(frame: &mut Frame, app: &App, theme: &Theme, editor: Rect) {
    let items = &app.inputs.cd_comps;
    if items.is_empty() {
        return;
    }
    let height = (items.len() as u16 + 2)
        .min(editor.y.saturating_sub(1))
        .min(12);
    if height < 3 {
        return;
    }
    let width = editor.width.saturating_sub(2).min(64);
    let area = Rect {
        x: editor.x + 1,
        y: editor.y.saturating_sub(height),
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let rows: Vec<ListItem> = items
        .iter()
        .map(|(display, _)| ListItem::new(Line::raw(format!(" {display}"))))
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.ui.menu_selected.min(items.len() - 1)));
    let title = if items.len() > 10 {
        format!(" directory ({}) ", items.len())
    } else {
        " directory ".to_string()
    };
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(title, Style::default().fg(theme.dim))),
        )
        .highlight_style(
            Style::default()
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

pub(super) fn draw_at_menu(frame: &mut Frame, app: &App, theme: &Theme, editor: Rect) {
    let items = app.file_completions();
    if items.is_empty() {
        return;
    }
    let height = (items.len() as u16 + 2)
        .min(editor.y.saturating_sub(1))
        .min(12);
    if height < 3 {
        return;
    }
    let width = editor.width.saturating_sub(2).min(72);
    let area = Rect {
        x: editor.x + 1,
        y: editor.y.saturating_sub(height),
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let rows: Vec<ListItem> = items
        .iter()
        .map(|path| ListItem::new(Line::raw(format!(" @{path}"))))
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.ui.menu_selected.min(items.len() - 1)));
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(" files (@) ", Style::default().fg(theme.dim))),
        )
        .highlight_style(
            Style::default()
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}
