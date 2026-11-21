//! Login wizard modal rendering with mouse hit regions and cursor placement.
use super::modals::modal_rect;
use crate::tui::*;

pub(super) fn draw_login_modal(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    area: Rect,
    login: &LoginModal,
) {
    match login {
        LoginModal::SelectProvider { filter, selected } => {
            let providers = app.filtered_login_providers(filter);
            let connected = app.inputs.catalog.connected_providers.clone();
            let want_h = ((providers.len() as u16) + 6).clamp(8, 19);
            let popup = modal_rect(area, 70, want_h);
            app.ui.modal_rect = Some(popup);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    " login (Esc close) ",
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
                    Span::raw(filter.clone()),
                ])),
                parts[0],
            );
            place_cursor(frame, parts[0], 9, filter, app.ui.modal_cursor);

            let list_h = parts[1].height as usize;
            let sel = (*selected).min(providers.len().saturating_sub(1));
            let offset = if list_h > 0 && sel >= list_h {
                sel + 1 - list_h
            } else {
                0
            };
            let rows: Vec<ListItem> = providers
                .iter()
                .skip(offset)
                .take(list_h.max(1))
                .enumerate()
                .map(|(vis, (_, p))| {
                    let idx = offset + vis;
                    let row_y = parts[1].y + vis as u16;
                    if row_y < parts[1].y + parts[1].height {
                        app.ui.hit_regions.push(HitRegion {
                            rect: Rect {
                                x: parts[1].x,
                                y: row_y,
                                width: parts[1].width,
                                height: 1,
                            },
                            target: HitTarget::ModalItem(idx),
                        });
                    }
                    let badge = match connected.get(p.id) {
                        Some(label) => format!("[✓ {label}]"),
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
                state.select(Some(sel.saturating_sub(offset)));
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
            let popup = modal_rect(area, 72, (prov.methods.len() as u16) * 2 + 4);
            app.ui.modal_rect = Some(popup);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {} - Select auth method (Esc back) ", prov.name),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let rows: Vec<ListItem> = prov
                .methods
                .iter()
                .enumerate()
                .map(|(idx, m)| {
                    let row_y = inner.y + (idx as u16) * 2;
                    if row_y < inner.y + inner.height {
                        let h = 2u16.min((inner.y + inner.height).saturating_sub(row_y));
                        app.ui.hit_regions.push(HitRegion {
                            rect: Rect {
                                x: inner.x,
                                y: row_y,
                                width: inner.width,
                                height: h,
                            },
                            target: HitTarget::ModalItem(idx),
                        });
                    }
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
            let popup = modal_rect(area, 68, 12);
            app.ui.modal_rect = Some(popup);
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
            for idx in 0..3 {
                let row_y = inner.y + 1 + idx as u16;
                if row_y < inner.y + inner.height {
                    app.ui.hit_regions.push(HitRegion {
                        rect: Rect {
                            x: inner.x,
                            y: row_y,
                            width: inner.width,
                            height: 1,
                        },
                        target: HitTarget::ModalLoginField(idx),
                    });
                }
            }
            let mut lines = vec![
                Line::from(""),
                field(0, "API Key:", &masked),
                field(1, "Base URL:", base_url),
                field(2, "Model:", model),
                Line::from(""),
                Line::styled(
                    "  Tab field · Ctrl+R reveal · Ctrl+U clear · Enter save · Esc back",
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
            let active_val = match *focus_field {
                0 => masked.as_str(),
                1 => base_url.as_str(),
                _ => model.as_str(),
            };
            let row = Rect {
                x: inner.x,
                y: inner.y + 1 + (*focus_field).min(2) as u16,
                width: inner.width,
                height: 1,
            };
            if row.y < inner.y + inner.height {
                place_cursor(frame, row, 15, active_val, app.ui.modal_cursor);
            }
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
            let popup = modal_rect(area, 72, 14);
            app.ui.modal_rect = Some(popup);
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
            for idx in 0..5 {
                let row_y = inner.y + 1 + idx as u16;
                if row_y < inner.y + inner.height {
                    app.ui.hit_regions.push(HitRegion {
                        rect: Rect {
                            x: inner.x,
                            y: row_y,
                            width: inner.width,
                            height: 1,
                        },
                        target: HitTarget::ModalLoginField(idx),
                    });
                }
            }
            let mut lines = vec![
                Line::from(""),
                field(0, "Protocol:", proto_label),
                field(1, "Name:", name),
                field(2, "Base URL:", base_url),
                field(3, "API Key:", &masked_key),
                field(4, "Model ID:", model),
                Line::from(""),
                Line::styled(
                    "  Tab field · ←/→ protocol · Ctrl+U clear · Enter save · Esc back",
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
            if *focus_field > 0 && *focus_field <= 4 {
                let active_val = match *focus_field {
                    1 => name.as_str(),
                    2 => base_url.as_str(),
                    3 => {
                        if key.is_empty() {
                            ""
                        } else {
                            masked_key.as_str()
                        }
                    }
                    _ => model.as_str(),
                };
                let row = Rect {
                    x: inner.x,
                    y: inner.y + 1 + *focus_field as u16,
                    width: inner.width,
                    height: 1,
                };
                if row.y < inner.y + inner.height {
                    place_cursor(frame, row, 15, active_val, app.ui.modal_cursor);
                }
            }
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
            let popup = modal_rect(area, 76, 15);
            app.ui.modal_rect = Some(popup);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {provider_name} - OAuth Browser (Esc back) "),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);
            if inner.width < 2 || inner.height == 0 {
                return;
            }
            let w = inner.width.saturating_sub(4).max(8) as usize;
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
            let input_row_idx = lines.len();
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
            if !*busy && input_row_idx < inner.height as usize {
                let row = Rect {
                    x: inner.x,
                    y: inner.y + input_row_idx as u16,
                    width: inner.width,
                    height: 1,
                };
                place_cursor(frame, row, 4, pasted, app.ui.modal_cursor);
            }
        }
        LoginModal::OAuthCodePaste {
            provider_name,
            url,
            code_input,
            busy,
            error,
            ..
        } => {
            let popup = modal_rect(area, 76, 15);
            app.ui.modal_rect = Some(popup);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {provider_name} (Esc back) "),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);
            if inner.width < 2 || inner.height == 0 {
                return;
            }
            let w = inner.width.saturating_sub(4).max(8) as usize;
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
            let input_row_idx = lines.len();
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
            if !*busy && input_row_idx < inner.height as usize {
                let row = Rect {
                    x: inner.x,
                    y: inner.y + input_row_idx as u16,
                    width: inner.width,
                    height: 1,
                };
                place_cursor(frame, row, 4, code_input, app.ui.modal_cursor);
            }
        }
        LoginModal::OAuthDevice {
            provider_name,
            verification_uri,
            user_code,
            status,
            error,
        } => {
            let popup = modal_rect(area, 66, 12);
            app.ui.modal_rect = Some(popup);
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
                Line::styled("  Press Esc to go back.", Style::default().fg(theme.dim)),
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

pub(super) fn place_cursor(frame: &mut Frame, row: Rect, prefix_w: u16, text: &str, cursor: usize) {
    if row.width == 0 || row.height == 0 {
        return;
    }
    let col = line_edit::cursor_col(text, cursor) as u16;
    let x = row.x.saturating_add(prefix_w).saturating_add(col);
    if x < row.x.saturating_add(row.width) {
        frame.set_cursor_position((x, row.y));
    }
}
