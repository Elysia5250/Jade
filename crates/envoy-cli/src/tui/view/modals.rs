//! Generic modals and the login wizard.
use crate::tui::*;

pub(super) fn draw_modal(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(modal) = app.modal.as_ref() else {
        return;
    };
    match modal {
        Modal::Picker(p) => {
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
            let width = 62u16.min(area.width.saturating_sub(4));
            let height = ((filtered.len() as u16) + 4)
                .max(6)
                .min(area.height.saturating_sub(2))
                .min(18);
            let popup = centered_rect(area, width, height);
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
            let list_items: Vec<ListItem> = filtered
                .iter()
                .map(|(label, _)| ListItem::new(Line::raw(format!(" {label}"))))
                .collect();
            let mut state = ListState::default();
            if !list_items.is_empty() {
                state.select(Some(p.selected.min(list_items.len() - 1)));
            }
            let list = List::new(list_items).highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, parts[1], &mut state);
        }
        Modal::Models(m) => {
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
            let width = 66u16.min(area.width.saturating_sub(4));
            let height = ((filtered.len() as u16) + 6)
                .max(8)
                .min(area.height.saturating_sub(2))
                .min(20);
            let popup = centered_rect(area, width, height);
            frame.render_widget(Clear, popup);
            let title = if m.fetching { " model " } else { " model " };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(title, Style::default().fg(theme.accent)));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(inner);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" Filter: ", Style::default().fg(theme.dim)),
                    Span::raw(m.filter.clone()),
                ])),
                parts[0],
            );
            let active = resolve_active_provider(&app.data_dir).map(|(_, mr)| mr);
            let rows: Vec<ListItem> = filtered
                .iter()
                .map(|item| {
                    let is_active = active.as_ref().is_some_and(|a| {
                        a.provider_id == item.provider_id && a.model_id == item.model_id
                    });
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
                .collect();
            let mut state = ListState::default();
            if !rows.is_empty() {
                state.select(Some(m.selected.min(rows.len() - 1)));
            }
            let list = List::new(rows).highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, parts[1], &mut state);
        }
        Modal::Sessions(s) => {
            let width = 72u16.min(area.width.saturating_sub(4));
            let height = ((s.sessions.len() as u16) + 4)
                .max(7)
                .min(area.height.saturating_sub(2))
                .min(18);
            let popup = centered_rect(area, width, height);
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

            let rows: Vec<ListItem> = if s.sessions.is_empty() {
                vec![ListItem::new(Line::styled(
                    " (no saved sessions yet)",
                    Style::default().fg(theme.dim),
                ))]
            } else {
                s.sessions
                    .iter()
                    .map(|item| {
                        let cur = if item.id == app.session.id {
                            "* "
                        } else {
                            "  "
                        };
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
                state.select(Some(s.selected.min(s.sessions.len() - 1)));
            }
            let list = List::new(rows).highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, inner, &mut state);
        }
        Modal::Rewind(r) => {
            let width = 76u16.min(area.width.saturating_sub(4));
            let height = ((r.checkpoints.len() as u16) + 5)
                .max(8)
                .min(area.height.saturating_sub(2))
                .min(18);
            let popup = centered_rect(area, width, height);
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

            let rows: Vec<ListItem> = r
                .checkpoints
                .iter()
                .enumerate()
                .map(|(idx, cp)| {
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
                state.select(Some(r.selected.min(r.checkpoints.len() - 1)));
            }
            let list = List::new(rows).highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, inner, &mut state);
        }
        Modal::Login(login) => draw_login_modal(frame, app, theme, area, login),
    }
}

fn draw_login_modal(frame: &mut Frame, app: &App, theme: &Theme, area: Rect, login: &LoginModal) {
    match login {
        LoginModal::SelectProvider { filter, selected } => {
            let providers = app.filtered_login_providers(filter);
            let connected = effective_providers(&app.data_dir);
            let width = 70u16.min(area.width.saturating_sub(4));
            let height = ((providers.len() as u16) + 5)
                .max(8)
                .min(area.height.saturating_sub(2))
                .min(18);
            let popup = centered_rect(area, width, height);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(" login ", Style::default().fg(theme.accent)));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(inner);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" Search: ", Style::default().fg(theme.dim)),
                    Span::raw(filter.clone()),
                ])),
                parts[0],
            );

            let rows: Vec<ListItem> = providers
                .iter()
                .map(|(_, p)| {
                    let badge = match connected.get(p.id) {
                        Some(sp) => format!("[✓ {}]", sp.credential.kind_label()),
                        None => String::new(),
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!(" {:<20} ", p.name),
                            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{:<32} ", p.subtitle),
                            Style::default().fg(theme.dim),
                        ),
                        Span::styled(badge, Style::default().fg(theme.ok)),
                    ]))
                })
                .collect();
            let mut state = ListState::default();
            if !rows.is_empty() {
                state.select(Some((*selected).min(rows.len() - 1)));
            }
            let list = List::new(rows).highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, parts[1], &mut state);
        }
        LoginModal::SelectMethod {
            provider_idx,
            selected,
        } => {
            let prov = &BUILTIN_PROVIDERS[*provider_idx];
            let width = 72u16.min(area.width.saturating_sub(4));
            let height = ((prov.methods.len() as u16) * 2 + 4).min(area.height.saturating_sub(2));
            let popup = centered_rect(area, width, height);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {} - Select auth method (Esc back/close) ", prov.name),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let rows: Vec<ListItem> = prov
                .methods
                .iter()
                .map(|m| {
                    ListItem::new(vec![
                        Line::styled(
                            format!(" {}", m.label),
                            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                        ),
                        Line::styled(format!("   {}", m.hint), Style::default().fg(theme.dim)),
                    ])
                })
                .collect();
            let mut state = ListState::default();
            state.select(Some(*selected));
            let list = List::new(rows).highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, inner, &mut state);
        }
        LoginModal::ApiKeyForm {
            provider_idx,
            key,
            base_url,
            model,
            focus_field,
            reveal,
            error,
        } => {
            let prov = &BUILTIN_PROVIDERS[*provider_idx];
            let width = 68u16.min(area.width.saturating_sub(4));
            let popup = centered_rect(area, width, 12);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {} - API Key ", prov.name),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let masked = if *reveal || key.is_empty() {
                key.clone()
            } else {
                "*".repeat(key.chars().count())
            };
            let field = |idx: usize, label: &str, val: &str| {
                let marker = if *focus_field == idx { "> " } else { "  " };
                let style = if *focus_field == idx {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.dim)
                };
                Line::from(vec![
                    Span::styled(format!("{marker}{label:<12} "), style),
                    Span::raw(val.to_string()),
                ])
            };
            let mut lines = vec![
                Line::from(""),
                field(0, "API Key:", &masked),
                field(1, "Base URL:", base_url),
                field(2, "Model:", model),
                Line::from(""),
                Line::styled(
                    "  Tab switch field - Ctrl+R reveal - Enter save - Esc cancel",
                    Style::default().fg(theme.dim),
                ),
            ];
            if let Some(err) = error {
                lines.push(Line::styled(
                    format!("  Error: {err}"),
                    Style::default().fg(theme.bad),
                ));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        LoginModal::CustomForm {
            protocol_idx,
            name,
            base_url,
            key,
            model,
            focus_field,
            error,
        } => {
            let width = 72u16.min(area.width.saturating_sub(4));
            let popup = centered_rect(area, width, 14);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    " Custom Compatible Provider ",
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let proto_label = if *protocol_idx == 1 {
                "< Anthropic-Compatible >"
            } else {
                "< OpenAI-Compatible >"
            };
            let masked_key = if key.is_empty() {
                "(optional)".to_string()
            } else {
                "*".repeat(key.chars().count())
            };
            let field = |idx: usize, label: &str, val: &str| {
                let marker = if *focus_field == idx { "> " } else { "  " };
                let style = if *focus_field == idx {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.dim)
                };
                Line::from(vec![
                    Span::styled(format!("{marker}{label:<12} "), style),
                    Span::raw(val.to_string()),
                ])
            };
            let mut lines = vec![
                Line::from(""),
                field(0, "Protocol:", proto_label),
                field(1, "Name:", name),
                field(2, "Base URL:", base_url),
                field(3, "API Key:", &masked_key),
                field(4, "Model ID:", model),
                Line::from(""),
                Line::styled(
                    "  Tab field - <-/-> protocol - Enter save - Esc cancel",
                    Style::default().fg(theme.dim),
                ),
            ];
            if let Some(err) = error {
                lines.push(Line::styled(
                    format!("  Error: {err}"),
                    Style::default().fg(theme.bad),
                ));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        LoginModal::OAuthBrowser {
            provider_name,
            url,
            pasted,
            status,
            busy,
            error,
            ..
        } => {
            let width = 76u16.min(area.width.saturating_sub(4));
            let popup = centered_rect(area, width, 15);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {provider_name} - OAuth Browser "),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let w = inner.width.saturating_sub(4) as usize;
            let mut lines = vec![
                Line::styled(
                    format!("  {status}"),
                    Style::default().fg(if *busy { theme.accent } else { theme.ok }),
                ),
                Line::from(""),
                Line::styled("  Authorize URL:", Style::default().fg(theme.dim)),
            ];
            for chunk in wrap_text(url, w) {
                lines.push(Line::styled(
                    format!("  {chunk}"),
                    Style::default().fg(theme.secondary),
                ));
            }
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "  Or paste redirect URL / code (for SSH/headless):",
                Style::default().fg(theme.dim),
            ));
            lines.push(Line::from(vec![
                Span::styled("  > ", Style::default().fg(theme.accent)),
                Span::raw(pasted.clone()),
            ]));
            if let Some(err) = error {
                lines.push(Line::styled(
                    format!("  Error: {err}"),
                    Style::default().fg(theme.bad),
                ));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        LoginModal::OAuthCodePaste {
            provider_name,
            url,
            code_input,
            busy,
            error,
            ..
        } => {
            let width = 76u16.min(area.width.saturating_sub(4));
            let popup = centered_rect(area, width, 15);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {provider_name} "),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let w = inner.width.saturating_sub(4) as usize;
            let mut lines = vec![Line::styled(
                "  Open this URL in your browser to authorize:",
                Style::default().fg(theme.dim),
            )];
            for chunk in wrap_text(url, w) {
                lines.push(Line::styled(
                    format!("  {chunk}"),
                    Style::default().fg(theme.secondary),
                ));
            }
            lines.push(Line::from(""));
            lines.push(Line::styled(
                if *busy {
                    "  Exchanging authorization code."
                } else {
                    "  Paste the `code#state` (or callback URL) and press Enter:"
                },
                Style::default().fg(if *busy { theme.accent } else { theme.text }),
            ));
            lines.push(Line::from(vec![
                Span::styled("  > ", Style::default().fg(theme.accent)),
                Span::raw(code_input.clone()),
            ]));
            if let Some(err) = error {
                lines.push(Line::styled(
                    format!("  Error: {err}"),
                    Style::default().fg(theme.bad),
                ));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        LoginModal::OAuthDevice {
            provider_name,
            verification_uri,
            user_code,
            status,
            error,
        } => {
            let width = 66u16.min(area.width.saturating_sub(4));
            let popup = centered_rect(area, width, 12);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {provider_name} - Device Flow "),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let mut lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  1. Open:  ", Style::default().fg(theme.dim)),
                    Span::styled(
                        verification_uri.clone(),
                        Style::default().fg(theme.secondary),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  2. Code:  ", Style::default().fg(theme.dim)),
                    Span::styled(
                        user_code.clone(),
                        Style::default().fg(theme.ok).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::styled(format!("  {status}"), Style::default().fg(theme.dim)),
                Line::styled("  Press Esc to cancel.", Style::default().fg(theme.dim)),
            ];
            if let Some(err) = error {
                lines.push(Line::styled(
                    format!("  Error: {err}"),
                    Style::default().fg(theme.bad),
                ));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
    }
}
