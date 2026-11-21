//! Generic modals (command picker, models, sessions, rewind) with mouse hit regions.
use super::login_modal::{draw_login_modal, place_cursor};
use crate::tui::*;

pub(super) fn modal_rect(area: Rect, want_w: u16, want_h: u16) -> Rect {
    let width = want_w
        .min(area.width.saturating_sub(4).max(1))
        .min(area.width);
    let height = want_h
        .min(area.height.saturating_sub(2).max(1))
        .min(area.height);
    centered_rect(area, width, height)
}

pub(super) fn draw_modal(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    if area.width < 4 || area.height < 4 {
        return;
    }
    let Some(modal) = app.ui.modal.take() else {
        return;
    };
    match &modal {
        Modal::Picker(p) => draw_picker(frame, app, theme, area, p),
        Modal::Models(m) => draw_models(frame, app, theme, area, m),
        Modal::Sessions(s) => draw_sessions(frame, app, theme, area, s),
        Modal::Rewind(r) => draw_rewind(frame, app, theme, area, r),
        Modal::Login(login) => draw_login_modal(frame, app, theme, area, login),
    }
    app.ui.modal = Some(modal);
}

fn draw_picker(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect, p: &Picker) {
    let filtered: Vec<&(String, Command)> = p
        .items
        .iter()
        .filter(|(label, _)| {
            p.query.is_empty()
                || label
                    .to_ascii_lowercase()
                    .contains(&p.query.to_ascii_lowercase())
        })
        .collect();
    let want_h = ((filtered.len() as u16) + 4).clamp(6, 18);
    let popup = modal_rect(area, 62, want_h);
    app.ui.modal_rect = Some(popup);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            format!(" {} ", p.title),
            Style::default().fg(theme.accent),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Search: ", Style::default().fg(theme.dim)),
            Span::raw(p.query.clone()),
        ])),
        parts[0],
    );
    place_cursor(frame, parts[0], 9, &p.query, app.ui.modal_cursor);

    let list_h = parts[1].height as usize;
    let sel = p.selected.min(filtered.len().saturating_sub(1));
    let offset = if list_h > 0 && sel >= list_h {
        sel + 1 - list_h
    } else {
        0
    };
    let list_items: Vec<ListItem> = filtered
        .iter()
        .skip(offset)
        .take(list_h.max(1))
        .enumerate()
        .map(|(vis, (label, _))| {
            let idx = offset + vis;
            push_modal_hit(app, parts[1], vis, idx);
            ListItem::new(Line::raw(format!(" {label}")))
        })
        .collect();
    let mut state = ListState::default();
    if !list_items.is_empty() {
        state.select(Some(sel.saturating_sub(offset)));
    }
    let list = List::new(list_items).highlight_style(
        Style::default()
            .bg(theme.selection)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, parts[1], &mut state);
}

fn draw_models(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect, m: &ModelsModal) {
    let filtered: Vec<&AvailableModel> = m
        .models
        .iter()
        .filter(|item| {
            m.filter.is_empty()
                || item
                    .model_id
                    .to_ascii_lowercase()
                    .contains(&m.filter.to_ascii_lowercase())
                || item
                    .provider_id
                    .to_ascii_lowercase()
                    .contains(&m.filter.to_ascii_lowercase())
        })
        .collect();
    let want_h = ((filtered.len() as u16) + 6).clamp(8, 20);
    let popup = modal_rect(area, 66, want_h);
    app.ui.modal_rect = Some(popup);
    frame.render_widget(Clear, popup);
    let title = if m.fetching {
        " model (refreshing...) "
    } else {
        " model "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(title, Style::default().fg(theme.accent)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Filter: ", Style::default().fg(theme.dim)),
            Span::raw(m.filter.clone()),
        ])),
        parts[0],
    );
    place_cursor(frame, parts[0], 9, &m.filter, app.ui.modal_cursor);

    let active_provider = app.inputs.catalog.provider_id.clone();
    let active_model = app.inputs.catalog.model_id.clone();
    let signed_in = app.inputs.catalog.signed_in;
    let list_h = parts[1].height as usize;
    let sel = m.selected.min(filtered.len().saturating_sub(1));
    let offset = if list_h > 0 && sel >= list_h {
        sel + 1 - list_h
    } else {
        0
    };
    let rows: Vec<ListItem> = if filtered.is_empty() && !m.filter.trim().is_empty() {
        let prov = if active_provider.is_empty() {
            "provider".to_string()
        } else {
            active_provider.clone()
        };
        vec![ListItem::new(Line::styled(
            format!(" Enter to use custom model: {prov}/{}", m.filter.trim()),
            Style::default().fg(theme.warn),
        ))]
    } else {
        filtered
            .iter()
            .skip(offset)
            .take(list_h.max(1))
            .enumerate()
            .map(|(vis, item)| {
                let idx = offset + vis;
                push_modal_hit(app, parts[1], vis, idx);
                let is_active = signed_in
                    && item.provider_id == active_provider
                    && item.model_id == active_model;
                let dot = if is_active { "* " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        dot,
                        Style::default().fg(if is_active { theme.ok } else { theme.dim }),
                    ),
                    Span::raw(format!("{:<30} ", item.model_id)),
                    Span::styled(
                        format!("{} ({})", item.provider_name, item.auth_kind),
                        Style::default().fg(theme.dim),
                    ),
                ]))
            })
            .collect()
    };
    let mut state = ListState::default();
    if !filtered.is_empty() {
        state.select(Some(sel.saturating_sub(offset)));
    }
    let list = List::new(rows).highlight_style(
        Style::default()
            .bg(theme.selection)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, parts[1], &mut state);
    frame.render_widget(
        Paragraph::new(Line::styled(
            " ↑↓ select · Enter choose · Ctrl+U clear · Esc close",
            Style::default().fg(theme.dim),
        )),
        parts[2],
    );
}

fn draw_sessions(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect, s: &SessionsModal) {
    let want_h = ((s.sessions.len() as u16) + 5).clamp(7, 19);
    let popup = modal_rect(area, 72, want_h);
    app.ui.modal_rect = Some(popup);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            " sessions ",
            Style::default().fg(theme.accent),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let list_h = parts[0].height as usize;
    let sel = s.selected.min(s.sessions.len().saturating_sub(1));
    let offset = if list_h > 0 && sel >= list_h {
        sel + 1 - list_h
    } else {
        0
    };
    let current_id = app.session.current.id.clone();
    let rows: Vec<ListItem> = if s.sessions.is_empty() {
        vec![ListItem::new(Line::styled(
            " (no saved sessions yet)",
            Style::default().fg(theme.dim),
        ))]
    } else {
        s.sessions
            .iter()
            .skip(offset)
            .take(list_h.max(1))
            .enumerate()
            .map(|(vis, item)| {
                let idx = offset + vis;
                push_modal_hit(app, parts[0], vis, idx);
                let cur = if item.id == current_id { "* " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(cur, Style::default().fg(theme.ok)),
                    Span::raw(format!("{:<32} ", truncate_chars(&item.title, 30))),
                    Span::styled(
                        format!("{} - {} items", collapse_home(&item.cwd), item.item_count),
                        Style::default().fg(theme.dim),
                    ),
                ]))
            })
            .collect()
    };
    let mut state = ListState::default();
    if !s.sessions.is_empty() {
        state.select(Some(sel.saturating_sub(offset)));
    }
    let list = List::new(rows).highlight_style(
        Style::default()
            .bg(theme.selection)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, parts[0], &mut state);

    let footer = if let Some(id) = &s.confirm_delete {
        let title = s
            .sessions
            .iter()
            .find(|x| &x.id == id)
            .map(|x| x.title.as_str())
            .unwrap_or("session");
        Line::styled(
            format!(" Press d again to delete \"{title}\" · Esc cancel"),
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        )
    } else {
        Line::styled(
            " ↑↓/jk select · Enter open · d delete · Esc close",
            Style::default().fg(theme.dim),
        )
    };
    frame.render_widget(Paragraph::new(footer), parts[1]);
}

fn draw_rewind(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect, r: &RewindModal) {
    let want_h = ((r.checkpoints.len() as u16) + 5).clamp(8, 18);
    let popup = modal_rect(area, 76, want_h);
    app.ui.modal_rect = Some(popup);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            " rewind checkpoint (Enter confirm · Esc cancel) ",
            Style::default().fg(theme.accent),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let list_h = inner.height as usize;
    let sel = r.selected.min(r.checkpoints.len().saturating_sub(1));
    let offset = if list_h > 0 && sel >= list_h {
        sel + 1 - list_h
    } else {
        0
    };
    let rows: Vec<ListItem> = r
        .checkpoints
        .iter()
        .skip(offset)
        .take(list_h.max(1))
        .enumerate()
        .map(|(vis, cp)| {
            let idx = offset + vis;
            push_modal_hit(app, inner, vis, idx);
            let badge = if idx == 0 { "* " } else { "  " };
            ListItem::new(Line::from(vec![
                Span::styled(badge, Style::default().fg(theme.ok)),
                Span::styled(format!("#{:<3} ", cp.id), Style::default().fg(theme.accent)),
                Span::raw(format!("{:<38} ", truncate_chars(&cp.label, 36))),
                Span::styled(
                    format!("{} files", cp.files_touched),
                    Style::default().fg(theme.dim),
                ),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    if !r.checkpoints.is_empty() {
        state.select(Some(sel.saturating_sub(offset)));
    }
    let list = List::new(rows).highlight_style(
        Style::default()
            .bg(theme.selection)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, inner, &mut state);
}

fn push_modal_hit(app: &mut App, area: Rect, vis: usize, idx: usize) {
    let row_y = area.y + vis as u16;
    if row_y < area.y + area.height {
        app.ui.hit_regions.push(HitRegion {
            rect: Rect {
                x: area.x,
                y: row_y,
                width: area.width,
                height: 1,
            },
            target: HitTarget::ModalItem(idx),
        });
    }
}
