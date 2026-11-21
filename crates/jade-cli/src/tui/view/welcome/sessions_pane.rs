//! Left Hub pane: Sessions & Workspaces list with live daemon badges and preview strip.
use super::fmt_relative_time;
use crate::tui::*;

pub(super) fn draw_left_pane(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    area: Rect,
    wide: bool,
) {
    let Some(w) = app.ui.welcome.as_ref() else {
        return;
    };
    let focused = w.focus == HubFocus::Left;
    let tab = w.tab;
    let sess_count = w.sessions.len();
    let ws_count = w.workspaces.len();

    let border_fg = if focused { theme.accent } else { theme.border };
    let title = build_tab_title(tab, focused, wide, sess_count, ws_count, theme);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_fg))
        .title(title)
        .style(Style::default().bg(theme.bg));

    register_tab_hits(app, area, wide, sess_count, ws_count);

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    match tab {
        HubTab::Sessions => draw_sessions_body(frame, app, theme, inner, focused, wide),
        HubTab::Workspaces => draw_workspaces_body(frame, app, theme, inner, focused),
    }
}

fn build_tab_title(
    tab: HubTab,
    focused: bool,
    wide: bool,
    sess_count: usize,
    ws_count: usize,
    theme: &Theme,
) -> Line<'static> {
    let active_st = Style::default()
        .fg(if focused { theme.accent } else { theme.text })
        .add_modifier(Modifier::BOLD);
    let dim_st = Style::default().fg(theme.dim);
    let mut spans = vec![
        Span::styled(
            format!(" [1] Sessions ({sess_count}) "),
            if tab == HubTab::Sessions {
                active_st
            } else {
                dim_st
            },
        ),
        Span::styled(
            format!(" [2] Workspaces ({ws_count}) "),
            if tab == HubTab::Workspaces {
                active_st
            } else {
                dim_st
            },
        ),
    ];
    if !wide {
        spans.push(Span::styled(" [3] Controls ", dim_st));
    }
    Line::from(spans)
}

fn register_tab_hits(app: &mut App, area: Rect, wide: bool, sess_count: usize, ws_count: usize) {
    let s_lbl = format!(" [1] Sessions ({sess_count}) ");
    let w_lbl = format!(" [2] Workspaces ({ws_count}) ");
    let sx = area.x + 1;
    let sw = display_width(&s_lbl) as u16;
    app.ui.hit_regions.push(HitRegion {
        rect: Rect {
            x: sx,
            y: area.y,
            width: sw.min(area.width.saturating_sub(2)),
            height: 1,
        },
        target: HitTarget::HubSelectTab(HubTab::Sessions),
    });
    let wx = sx + sw;
    let ww = display_width(&w_lbl) as u16;
    if wx < area.x + area.width {
        app.ui.hit_regions.push(HitRegion {
            rect: Rect {
                x: wx,
                y: area.y,
                width: ww.min((area.x + area.width).saturating_sub(wx)),
                height: 1,
            },
            target: HitTarget::HubSelectTab(HubTab::Workspaces),
        });
    }
    if !wide {
        let cx = wx + ww;
        let cw = display_width(" [3] Controls ") as u16;
        if cx < area.x + area.width {
            app.ui.hit_regions.push(HitRegion {
                rect: Rect {
                    x: cx,
                    y: area.y,
                    width: cw.min((area.x + area.width).saturating_sub(cx)),
                    height: 1,
                },
                target: HitTarget::HubSelectFocus(HubFocus::Controls),
            });
        }
    }
}

fn draw_sessions_body(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    inner: Rect,
    focused: bool,
    wide: bool,
) {
    let show_preview = wide && inner.height >= 12;
    let (list_area, preview_area) = if show_preview {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(4)])
            .split(inner);
        (split[0], Some(split[1]))
    } else {
        (inner, None)
    };

    let filtered = app.filtered_hub_sessions();
    let (search, searching, selected) = {
        let Some(w) = app.ui.welcome.as_ref() else {
            return;
        };
        (w.search.clone(), w.searching, w.selected_session)
    };

    let mut y = list_area.y;
    let max_y = list_area.y + list_area.height;
    let width = list_area.width as usize;

    // Search bar row
    let search_line = if searching {
        Line::from(vec![
            Span::styled(
                " / ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{search}_"), Style::default().fg(theme.text)),
        ])
    } else if !search.is_empty() {
        Line::from(vec![
            Span::styled(" / ", Style::default().fg(theme.accent)),
            Span::styled(search.clone(), Style::default().fg(theme.text)),
            Span::styled("  (Esc clear)", Style::default().fg(theme.dim)),
        ])
    } else {
        Line::styled(
            " / search sessions or path   ·   n new session",
            Style::default().fg(theme.dim),
        )
    };
    if y < max_y {
        frame.render_widget(
            Paragraph::new(search_line),
            Rect {
                x: list_area.x,
                y,
                width: list_area.width,
                height: 1,
            },
        );
        y += 1;
    }

    // Reserve 1 bottom row for quick path actions
    let action_y = max_y.saturating_sub(1);
    let body_max_y = if action_y > y + 1 { action_y } else { max_y };

    if filtered.is_empty() {
        if y + 1 < body_max_y {
            let msg = if search.is_empty() {
                "  No saved sessions yet — press Enter or n to start"
            } else {
                "  No sessions match filter"
            };
            frame.render_widget(
                Paragraph::new(Line::styled(msg, Style::default().fg(theme.dim))),
                Rect {
                    x: list_area.x,
                    y: y + 1,
                    width: list_area.width,
                    height: 1,
                },
            );
        }
    } else {
        let rows: Vec<HubSessionRow> = {
            let Some(w) = app.ui.welcome.as_ref() else {
                return;
            };
            filtered
                .iter()
                .filter_map(|&idx| w.sessions.get(idx).cloned())
                .collect()
        };
        let avail_slots = ((body_max_y.saturating_sub(y)) / 2).max(1) as usize;
        let sel = selected.min(rows.len().saturating_sub(1));
        let start = if sel >= avail_slots {
            sel + 1 - avail_slots
        } else {
            0
        };
        let mut last_live: Option<bool> = None;

        for (offset, row) in rows.iter().skip(start).enumerate() {
            let f_idx = start + offset;
            let is_live = row.running || row.queued > 0;
            if last_live != Some(is_live) && y < body_max_y {
                let heading = if is_live {
                    "  ACTIVE IN DAEMON"
                } else {
                    "  RECENT SESSIONS"
                };
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        heading,
                        Style::default()
                            .fg(if is_live { theme.accent } else { theme.dim })
                            .add_modifier(Modifier::BOLD),
                    )),
                    Rect {
                        x: list_area.x,
                        y,
                        width: list_area.width,
                        height: 1,
                    },
                );
                y += 1;
                last_live = Some(is_live);
            }
            if y >= body_max_y {
                break;
            }
            let is_sel = focused && f_idx == sel;
            let row_h = if y + 1 < body_max_y { 2 } else { 1 };
            let rect = Rect {
                x: list_area.x,
                y,
                width: list_area.width,
                height: row_h,
            };
            app.ui.hit_regions.push(HitRegion {
                rect,
                target: HitTarget::HubSession(f_idx),
            });
            render_session_row(frame, row, theme, rect, width, is_sel);
            y += row_h;
        }
    }

    if action_y < max_y && action_y > list_area.y {
        draw_quick_path_actions(frame, app, theme, list_area.x, action_y, list_area.width);
    }

    if let Some(p_area) = preview_area {
        draw_session_preview(frame, app, theme, p_area);
    }
}

fn render_session_row(
    frame: &mut Frame,
    row: &HubSessionRow,
    theme: &Theme,
    rect: Rect,
    width: usize,
    selected: bool,
) {
    // 标记必须是「1 格宽、且常见等宽字体（JetBrains Mono 系）里存在」的字形：
    // U+25D0 ◐ 在 JetBrains Mono 里缺字，回退字体把它的自然宽度画成 2 格，
    // 会向右溢出 1 格盖住标题。U+25D4 ◔ 是同族里 1 格宽的字形。
    let (glyph, glyph_fg) = if row.running {
        ("●", theme.accent)
    } else if row.queued > 0 {
        ("◔", theme.warn)
    } else if row.is_current {
        ("◆", theme.secondary)
    } else {
        ("○", theme.dim)
    };
    let right_badge = if row.running {
        if row.queued > 0 {
            format!("working · q:{}", row.queued)
        } else {
            "working".into()
        }
    } else if row.is_current {
        "current".into()
    } else {
        fmt_relative_time(row.updated_ms)
    };
    let badge_w = display_width(&right_badge);
    let title_budget = width.saturating_sub(badge_w + 6).max(8);
    let title = truncate_width(&row.title, title_budget);
    let pad = width.saturating_sub(display_width(&title) + badge_w + 5);

    let bg = if selected { theme.selection } else { theme.bg };
    let marker = if selected { "▸" } else { " " };
    let top = Line::from(vec![
        Span::styled(
            format!(" {marker}"),
            Style::default()
                .fg(theme.accent)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{glyph} "), Style::default().fg(glyph_fg).bg(bg)),
        Span::styled(
            title,
            Style::default()
                .fg(theme.text)
                .bg(bg)
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        Span::styled(
            format!("{right_badge} "),
            Style::default()
                .fg(if row.running {
                    theme.accent
                } else if row.is_current {
                    theme.secondary
                } else {
                    theme.dim
                })
                .bg(bg),
        ),
    ]);
    let mut lines = vec![top];
    if rect.height >= 2 {
        let mut meta = format!("    {}", collapse_home(&row.cwd));
        if row.item_count > 0 {
            meta.push_str(&format!(" · {} items", row.item_count));
        }
        if row.changed_files > 0 {
            meta.push_str(&format!(" · {} files", row.changed_files));
        }
        let sub = pad_to_width(&truncate_width(&meta, width), width);
        lines.push(Line::styled(sub, Style::default().fg(theme.dim).bg(bg)));
    }
    frame.render_widget(Paragraph::new(lines), rect);
}

fn draw_session_preview(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let sep = "─".repeat(area.width as usize);
    let mut lines = vec![Line::styled(sep, Style::default().fg(theme.border))];
    let width = area.width as usize;
    if let Some(row) = app.selected_hub_session() {
        let ctx_info = if row.prompt_tokens > 0 {
            let cap = app.inputs.catalog.compact_at.max(1);
            let (filled, empty) = context_bar(row.prompt_tokens, cap);
            format!(
                "  ctx {filled}{empty} {}/{}",
                fmt_tokens(row.prompt_tokens),
                fmt_tokens(cap)
            )
        } else {
            String::new()
        };
        let header = truncate_width(&format!(" PREVIEW · {}", row.title), width);
        lines.push(Line::styled(
            header,
            Style::default()
                .fg(theme.secondary)
                .add_modifier(Modifier::BOLD),
        ));
        let path_line = truncate_width(
            &format!(" {}  [{}]{ctx_info}", collapse_home(&row.cwd), row.target),
            width,
        );
        lines.push(Line::styled(path_line, Style::default().fg(theme.text)));
        let preview = if row.last_preview.is_empty() {
            " (empty session)".to_string()
        } else {
            format!(" \"{}\"", row.last_preview)
        };
        lines.push(Line::styled(
            truncate_width(&preview, width),
            Style::default().fg(theme.dim),
        ));
    } else {
        lines.push(Line::styled(
            format!(" current cwd: {}", collapse_home(&app.session.current.cwd)),
            Style::default().fg(theme.dim),
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_workspaces_body(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    inner: Rect,
    focused: bool,
) {
    let (workspaces, selected) = {
        let Some(w) = app.ui.welcome.as_ref() else {
            return;
        };
        (w.workspaces.clone(), w.selected_workspace)
    };
    let width = inner.width as usize;
    let action_y = inner.y + inner.height.saturating_sub(1);
    let body_max_y = if action_y > inner.y + 1 {
        action_y
    } else {
        inner.y + inner.height
    };
    let avail = (body_max_y.saturating_sub(inner.y)) as usize;
    let sel = selected.min(workspaces.len().saturating_sub(1));
    let start = if sel >= avail && avail > 0 {
        sel + 1 - avail
    } else {
        0
    };

    for (y, (offset, ws)) in (inner.y..body_max_y).zip(workspaces.iter().skip(start).enumerate()) {
        let idx = start + offset;
        let is_sel = focused && idx == sel;
        let bg = if is_sel { theme.selection } else { theme.bg };
        let marker = if is_sel { "▸" } else { " " };
        let tag = if ws.is_here { "current" } else { "recent " };
        let mut suffix = String::new();
        if ws.running_count > 0 {
            suffix.push_str(&format!(" ●{}", ws.running_count));
        }
        if ws.resumes {
            suffix.push_str(" (resumes)");
        }
        let shown_path = collapse_home(&ws.cwd);
        let budget = width.saturating_sub(display_width(&suffix) + 12).max(8);
        let body = truncate_width(&shown_path, budget);
        let pad = width.saturating_sub(display_width(&body) + display_width(&suffix) + 12);
        let line = Line::from(vec![
            Span::styled(
                format!(" {marker} "),
                Style::default().fg(theme.accent).bg(bg),
            ),
            Span::styled(
                format!("{tag} "),
                Style::default()
                    .fg(if ws.is_here { theme.accent } else { theme.dim })
                    .bg(bg),
            ),
            Span::styled(
                body,
                Style::default()
                    .fg(theme.text)
                    .bg(bg)
                    .add_modifier(if is_sel {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
            Span::styled(
                format!("{suffix} "),
                Style::default()
                    .fg(if ws.running_count > 0 {
                        theme.accent
                    } else {
                        theme.dim
                    })
                    .bg(bg),
            ),
        ]);
        let rect = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: 1,
        };
        app.ui.hit_regions.push(HitRegion {
            rect,
            target: HitTarget::HubWorkspace(idx),
        });
        frame.render_widget(Paragraph::new(line), rect);
    }

    if action_y < inner.y + inner.height {
        draw_quick_path_actions(frame, app, theme, inner.x, action_y, inner.width);
    }
}

fn draw_quick_path_actions(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    x: u16,
    y: u16,
    width: u16,
) {
    let open_lbl = " + [o] open path... ";
    let new_lbl = " + [N] new folder... ";
    let open_w = display_width(open_lbl) as u16;
    let new_w = display_width(new_lbl) as u16;
    app.ui.hit_regions.push(HitRegion {
        rect: Rect {
            x,
            y,
            width: open_w.min(width),
            height: 1,
        },
        target: HitTarget::HubOpenPath,
    });
    if x + open_w + 1 < x + width {
        app.ui.hit_regions.push(HitRegion {
            rect: Rect {
                x: x + open_w + 1,
                y,
                width: new_w.min(width.saturating_sub(open_w + 1)),
                height: 1,
            },
            target: HitTarget::HubNewFolder,
        });
    }
    let line = Line::from(vec![
        Span::styled(open_lbl, Style::default().fg(theme.link)),
        Span::raw(" "),
        Span::styled(new_lbl, Style::default().fg(theme.link)),
    ]);
    frame.render_widget(
        Paragraph::new(line),
        Rect {
            x,
            y,
            width,
            height: 1,
        },
    );
}
