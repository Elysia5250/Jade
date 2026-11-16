//! Frame layout. The screen shows the transcript, the prompt, and one status line.
use super::*;

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
    let theme = app.theme();
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg)),
        area,
    );

    let (main_area, sidebar_area) = if app.show_sidebar && area.width >= 100 {
        let halves = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(48), Constraint::Length(32)])
            .split(area);
        let mut main = halves[0];
        main.width = main.width.saturating_sub(1);
        (main, Some(halves[1]))
    } else {
        (area, None)
    };
    let column = centered_column(main_area, 96);
    let editor_inner_width = column.width.saturating_sub(6).max(10) as usize;
    let (visual_lines, cursor_row, cursor_col) =
        layout_editor_text(&app.input, app.input_cursor, editor_inner_width);
    let editor_height = visual_lines.len().clamp(1, 8) as u16;
    let max_rows = editor_height as usize;
    let editor_skip = if cursor_row >= max_rows {
        (cursor_row + 1 - max_rows).min(visual_lines.len().saturating_sub(max_rows))
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(editor_height),
            Constraint::Length(1),
        ])
        .split(column);
    let body_area = chunks[0];
    let editor_area = chunks[2];
    let status_area = chunks[3];

    draw_transcript(frame, app, &theme, body_area);
    if let Some(sidebar) = sidebar_area {
        draw_sidebar(frame, app, &theme, sidebar);
    }
    draw_editor(frame, app, &theme, editor_area, &visual_lines, editor_skip);
    draw_status(frame, app, &theme, status_area);
    draw_queued(frame, app, &theme, editor_area);

    if app.pending_ask.is_some() {
        draw_ask(frame, app, &theme, area);
    } else {
        if app.focus == Focus::Input && app.modal.is_none() {
            if !app.cd_completions().is_empty() {
                draw_cd_menu(frame, app, &theme, editor_area);
            } else if !app.filtered_slash().is_empty() {
                draw_slash_menu(frame, app, &theme, editor_area);
            }
        }
        if app.modal.is_some() {
            draw_modal(frame, app, &theme, area);
        }
    }

    if app.modal.is_none() && app.pending_ask.is_none() && app.focus == Focus::Input {
        let x = editor_area.x + 2 + cursor_col as u16;
        let visible_row = cursor_row
            .saturating_sub(editor_skip)
            .min(max_rows.saturating_sub(1));
        let y = editor_area.y + visible_row as u16;
        if x < editor_area.x + editor_area.width.saturating_sub(1) && y < editor_area.y + editor_area.height
        {
            frame.set_cursor_position((x, y));
        }
    }
}

/// Keep prose readable on wide terminals: the transcript is a centered column
/// rather than edge-to-edge text.
/// Add a separator only when the previous line is not already blank (and not
/// at the very top of the transcript).
fn push_blank(lines: &mut Vec<Line<'_>>) {
    let already = lines
        .last()
        .map(|line| line.spans.iter().all(|s| s.content.trim().is_empty()))
        .unwrap_or(true);
    if !already {
        lines.push(Line::from(""));
    }
}

fn centered_column(area: Rect, max: u16) -> Rect {
    let width = area.width.min(max);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y,
        width,
        height: area.height,
    }
}

fn draw_transcript(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    app.transcript_height = area.height;

    if app.session.items.is_empty() {
        return;
    }

    let inner = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(2),
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let width = inner.width.max(16) as usize;
    let mut lines: Vec<Line> = Vec::new();

    for (idx, item) in app.session.items.iter().enumerate() {
        let is_selected = app.focus == Focus::Transcript && app.selected_card == Some(idx);
        match item {
            TranscriptItem::User { text, .. } => {
                push_blank(&mut lines);
                let bar_style = Style::default().fg(if is_selected {
                    theme.warn
                } else {
                    theme.accent
                });
                for raw_line in text.lines() {
                    for wrapped in wrap_text(raw_line, width.saturating_sub(2)) {
                        lines.push(Line::from(vec![
                            Span::styled("▌ ", bar_style),
                            Span::styled(wrapped, Style::default().fg(theme.text)),
                        ]));
                    }
                }
                push_blank(&mut lines);
            }
            TranscriptItem::Assistant {
                text,
                reasoning,
                reasoning_collapsed,
                done,
                error,
                ..
            } => {
                if text.trim().is_empty()
                    && reasoning.is_empty()
                    && *done
                    && error.is_none()
                {
                    // A pure tool-call step has no prose; do not leave a gap.
                    continue;
                }
                push_blank(&mut lines);
                if !reasoning.is_empty() {
                    if *reasoning_collapsed {
                        lines.push(Line::from(vec![
                            Span::styled("▸ ", Style::default().fg(theme.dim)),
                            Span::styled("Thinking", Style::default().fg(theme.dim)),
                        ]));
                    } else {
                        lines.push(Line::styled(
                            "▾ Thinking",
                            Style::default().fg(theme.dim),
                        ));
                        for rline in reasoning.lines() {
                            for w in wrap_text(rline, width.saturating_sub(2)) {
                                lines.push(Line::from(vec![
                                    Span::styled("  ", Style::default().fg(theme.border)),
                                    Span::styled(w, Style::default().fg(theme.dim)),
                                ]));
                            }
                        }
                    }
                }
                if text.trim().is_empty() {
                    if !*done {
                        lines.push(Line::styled("▍", Style::default().fg(theme.accent)));
                    }
                } else {
                    render_markdown_lines(text, width, theme, &mut lines);
                }
                if let Some(err) = error {
                    for w in wrap_text(&format!("! {err}"), width.saturating_sub(2)) {
                        lines.push(Line::styled(
                            format!(" {w}"),
                            Style::default().fg(theme.bad),
                        ));
                    }
                }
                push_blank(&mut lines);
            }
            TranscriptItem::Tool {
                title,
                subtitle,
                target,
                output,
                status,
                collapsed,
                duration_ms,
                exit_code,
                started_ms,
                ..
            } => {
                let (glyph, glyph_style) = match status {
                    CardStatus::Queued => ("○", Style::default().fg(theme.warn)),
                    CardStatus::Running => ("●", Style::default().fg(theme.ok)),
                    CardStatus::Done => ("✓", Style::default().fg(theme.ok)),
                    CardStatus::Error => ("✗", Style::default().fg(theme.bad)),
                };
                let mut meta_parts = Vec::new();
                let elapsed = duration_ms.unwrap_or_else(|| now_ms().saturating_sub(*started_ms));
                meta_parts.push(fmt_duration(elapsed));
                if let Some(code) = exit_code.filter(|code| *code != 0) {
                    meta_parts.push(format!("exit {code}"));
                }
                if target != "local" {
                    meta_parts.push(target.clone());
                }
                let meta = format!("  {}", meta_parts.join(" · "));
                let meta_w = display_width(&meta);
                let budget = width.saturating_sub(2 + meta_w).max(10);
                let title_shown = truncate_width(title, budget);
                let title_w = display_width(&title_shown);
                let sub_budget = budget.saturating_sub(title_w + 2);

                let mut spans = vec![
                    Span::styled(format!("{glyph} "), glyph_style),
                    Span::styled(
                        title_shown,
                        Style::default()
                            .fg(theme.text)
                            .add_modifier(Modifier::BOLD),
                    ),
                ];
                if !subtitle.is_empty() && sub_budget >= 6 {
                    spans.push(Span::styled(
                        format!("  {}", truncate_width(subtitle, sub_budget)),
                        Style::default().fg(theme.dim),
                    ));
                }
                spans.push(Span::styled(meta, Style::default().fg(theme.dim)));
                if is_selected {
                    lines.push(Line::from(spans).style(Style::default().bg(theme.selection)));
                } else {
                    lines.push(Line::from(spans));
                }

                if !*collapsed && !output.is_empty() {
                    let total = output.len();
                    let preview_start = if total > 60 && *status != CardStatus::Running {
                        lines.push(Line::styled(
                            format!("  … ({} earlier lines)", total - 50),
                            Style::default().fg(theme.dim),
                        ));
                        total - 50
                    } else {
                        0
                    };
                    for out_line in &output[preview_start..] {
                        let style = if out_line.err {
                            Style::default().fg(theme.bad)
                        } else {
                            Style::default().fg(theme.text)
                        };
                        for w in wrap_text(&out_line.text, width.saturating_sub(4)) {
                            lines.push(Line::styled(format!("    {w}"), style));
                        }
                    }
                }
            }
            TranscriptItem::Notice { text, is_error, .. } => {
                push_blank(&mut lines);
                let style = Style::default().fg(if *is_error { theme.bad } else { theme.dim });
                for w in wrap_text(text, width.saturating_sub(2)) {
                    lines.push(Line::styled(format!("· {w}"), style));
                }
            }
            TranscriptItem::Say { text, level, .. } => {
                push_blank(&mut lines);
                let style = Style::default().fg(match level.as_str() {
                    "warning" => theme.warn,
                    "error" => theme.bad,
                    _ => theme.secondary,
                });
                for (i, w) in wrap_text(text, width.saturating_sub(4)).iter().enumerate() {
                    let prefix = if i == 0 { "· " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(theme.accent)),
                        Span::styled(w.clone(), style),
                    ]));
                }
            }
        }
    }

    lines.push(Line::from(""));
    let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
    app.transcript_max_scroll = max_scroll;
    if app.follow {
        app.transcript_scroll = max_scroll;
    } else {
        app.transcript_scroll = app.transcript_scroll.min(max_scroll);
    }
    frame.render_widget(
        Paragraph::new(lines).scroll((app.transcript_scroll, 0)),
        inner,
    );
}

fn draw_sidebar(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let border_color = if app.focus == Focus::Sidebar {
        theme.accent
    } else {
        theme.border
    };
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "jobs",
                Style::default()
                    .fg(theme.dim)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {}", app.jobs.len()), Style::default().fg(theme.dim)),
        ])),
        parts[0],
    );

    let items: Vec<ListItem> = if app.jobs.is_empty() {
        vec![ListItem::new(Line::styled(
            "none",
            Style::default().fg(theme.dim),
        ))]
    } else {
        app.jobs
            .iter()
            .map(|job| {
                let (glyph, style) = match &job.state {
                    JobState::Queued { .. } => ("○", Style::default().fg(theme.warn)),
                    JobState::Running { .. } => ("●", Style::default().fg(theme.ok)),
                    JobState::Done { result, .. } if result.ok() => {
                        ("✓", Style::default().fg(theme.dim))
                    }
                    JobState::Done { .. } => ("✗", Style::default().fg(theme.bad)),
                };
                let cmd = truncate_chars(&job.command(), inner.width.saturating_sub(6) as usize);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{glyph} "), style),
                    Span::raw(cmd),
                ]))
            })
            .collect()
    };
    let mut state = ListState::default();
    if !app.jobs.is_empty() {
        state.select(Some(app.job_selected));
    }
    let list = List::new(items).highlight_style(
        Style::default()
            .bg(theme.selection)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, parts[1], &mut state);
}

fn draw_editor(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    area: Rect,
    visual_lines: &[String],
    skip: usize,
) {
    let mode_color = match app.mode {
        InputMode::Agent => theme.accent,
        InputMode::Shell => theme.warn,
    };
    let frame_color = if app.focus == Focus::Input {
        mode_color
    } else {
        theme.border
    };
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(frame_color))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let max_rows = inner.height as usize;
    let skip = skip.min(visual_lines.len().saturating_sub(max_rows));
    let end = (skip + max_rows).min(visual_lines.len());
    let content = Rect {
        x: inner.x + 1,
        y: inner.y,
        width: inner.width.saturating_sub(2),
        height: inner.height,
    };
    let lines: Vec<Line> = if app.input.is_empty() {
        let placeholder = match app.mode {
            InputMode::Agent => "Ask",
            InputMode::Shell => "Run",
        };
        vec![Line::styled(placeholder, Style::default().fg(theme.dim))]
    } else {
        visual_lines[skip..end]
            .iter()
            .map(|text| Line::raw(text.clone()))
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), content);
}

fn draw_status(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (mode, mode_color) = match app.mode {
        InputMode::Agent => ("agent", theme.accent),
        InputMode::Shell => ("shell", theme.warn),
    };

    let mut right_spans: Vec<Span> = Vec::new();
    if let Some((_, model)) = resolve_active_provider(&app.data_dir) {
        let (used, estimated) = context_used(&app.session);
        let window = context_window(&model.model_id);
        let ratio = if window == 0 {
            0.0
        } else {
            (used as f64 / window as f64).clamp(0.0, 1.0)
        };
        let bar_color = if ratio >= 0.9 {
            theme.bad
        } else if ratio >= 0.7 {
            theme.warn
        } else {
            theme.accent
        };
        if app.agent_running {
            right_spans.push(Span::styled("● ", Style::default().fg(theme.accent)));
        }
        right_spans.push(Span::styled(
            model.model_id.clone(),
            Style::default().fg(theme.secondary),
        ));
        right_spans.push(Span::raw("  "));
        let (filled, empty) = context_bar(used, window);
        right_spans.push(Span::styled(filled, Style::default().fg(bar_color)));
        right_spans.push(Span::styled(empty, Style::default().fg(theme.border)));
        let mark = if estimated { "~" } else { "" };
        right_spans.push(Span::styled(
            format!("  {mark}{}/{}", fmt_tokens(used), fmt_tokens(window)),
            Style::default().fg(theme.dim),
        ));
    } else {
        right_spans.push(Span::styled("sign in", Style::default().fg(theme.warn)));
    }

    let right = Line::from(right_spans);
    let right_w = (right.width() as u16).min(area.width.saturating_sub(4));
    let left_budget = area.width.saturating_sub(right_w + 1) as usize;

    let remote = app.target != "local" && !app.target.is_empty();
    let offline = app.agent_offline || !app.connected;
    let toast = app.active_toast().map(|(msg, err)| {
        (
            format!("  {}", truncate_width(msg, 36)),
            err,
        )
    });
    let target_label = format!("  {}", app.target);
    let offline_label = "  offline";
    let suffix_w = if let Some((label, _)) = &toast {
        display_width(label)
    } else {
        (if remote { display_width(&target_label) } else { 0 })
            + (if offline { display_width(offline_label) } else { 0 })
    };
    let mode_w = display_width(mode) + 2;
    let cwd_budget = left_budget.saturating_sub(mode_w + suffix_w).max(1);
    let cwd = truncate_width(&app.cwd_label(), cwd_budget);

    let mut left = vec![
        Span::styled(
            mode,
            Style::default()
                .fg(mode_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(cwd.clone(), Style::default().fg(theme.text)),
    ];
    if let Some((label, err)) = toast {
        left.push(Span::styled(
            label,
            Style::default().fg(if err { theme.bad } else { theme.ok }),
        ));
    } else {
        if remote && left_budget > mode_w + display_width(&cwd) + 4 {
            left.push(Span::styled(target_label, Style::default().fg(theme.secondary)));
        }
        if offline {
            left.push(Span::styled(offline_label, Style::default().fg(theme.bad)));
        }
    }

    let left_area = Rect {
        width: area.width.saturating_sub(right_w),
        ..area
    };
    frame.render_widget(Paragraph::new(Line::from(left)), left_area);
    if right_w > 0 {
        frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
    }
}

fn draw_queued(frame: &mut Frame, app: &App, theme: &Theme, editor: Rect) {
    if app.queued.is_empty() {
        return;
    }
    let take = app.queued.len().min(2);
    let height = take as u16;
    let area = Rect {
        x: editor.x + 2,
        y: editor.y.saturating_sub(height),
        width: editor.width.saturating_sub(4),
        height,
    };
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let mut lines = Vec::new();
    for text in app.queued.iter().take(take) {
        lines.push(Line::from(vec![
            Span::styled("queued  ", Style::default().fg(theme.warn)),
            Span::styled(
                truncate_width(text, area.width.saturating_sub(10) as usize),
                Style::default().fg(theme.dim),
            ),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        area,
    );
}

fn draw_ask(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(ask) = &app.pending_ask else {
        return;
    };
    let width = 64u16.min(area.width.saturating_sub(4));
    let height = (ask.options.len() as u16 + 8).min(area.height.saturating_sub(2));
    if width < 20 || height < 5 {
        return;
    }
    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);
    let mut lines = vec![Line::from("")];
    for wrapped in wrap_text(&ask.question, width.saturating_sub(4) as usize) {
        lines.push(Line::styled(
            format!(" {wrapped}"),
            Style::default().fg(theme.text),
        ));
    }
    lines.push(Line::from(""));
    for (index, option) in ask.options.iter().enumerate() {
        let chosen = index == ask.selected;
        let marker = if chosen { " ▸ " } else { "   " };
        let style = if chosen {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };
        let rec = if option.recommended { "  (recommended)" } else { "" };
        lines.push(Line::styled(format!("{marker}{}{rec}", option.label), style));
        if let Some(desc) = &option.description {
            lines.push(Line::styled(
                format!("     {desc}"),
                Style::default().fg(theme.dim),
            ));
        }
    }
    lines.push(Line::from(""));
    let left = ask
        .deadline
        .saturating_duration_since(Instant::now())
        .as_secs();
    lines.push(Line::styled(
        format!(" {left}s"),
        Style::default().fg(theme.dim),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warn))
        .style(Style::default().bg(theme.panel))
        .title(Span::styled(" ask ", Style::default().fg(theme.warn)));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn draw_slash_menu(frame: &mut Frame, app: &App, theme: &Theme, editor: Rect) {
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
    state.select(Some(app.menu_selected.min(items.len() - 1)));
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

fn draw_cd_menu(frame: &mut Frame, app: &App, theme: &Theme, editor: Rect) {
    let items = app.cd_completions();
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
    state.select(Some(app.menu_selected.min(items.len() - 1)));
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    " directory ",
                    Style::default().fg(theme.dim),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_modal(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
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
            let title = if m.fetching {
                " model "
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
                    let is_active = active
                        .as_ref()
                        .is_some_and(|a| a.provider_id == item.provider_id && a.model_id == item.model_id);
                    let dot = if is_active { "● " } else { "  " };
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
                        let cur = if item.id == app.session.id { "● " } else { "  " };
                        ListItem::new(Line::from(vec![
                            Span::styled(cur, Style::default().fg(theme.ok)),
                            Span::raw(format!("{:<32} ", truncate_chars(&item.title, 30))),
                            Span::styled(
                                format!(
                                    "{} · {} items",
                                    collapse_home(&item.cwd),
                                    item.item_count
                                ),
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
        Modal::Login(login) => draw_login_modal(frame, app, theme, area, login),
    }
}

fn draw_login_modal(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    area: Rect,
    login: &LoginModal,
) {
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
                .title(Span::styled(
                    " login ",
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
                        Span::styled(format!("{:<32} ", p.subtitle), Style::default().fg(theme.dim)),
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
            let height = ((prov.methods.len() as u16) * 2 + 4)
                .min(area.height.saturating_sub(2));
            let popup = centered_rect(area, width, height);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(
                    format!(" {} — Select auth method (Esc back/close) ", prov.name),
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
                    format!(" {} — API Key ", prov.name),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let masked = if *reveal || key.is_empty() {
                key.clone()
            } else {
                "•".repeat(key.chars().count())
            };
            let field = |idx: usize, label: &str, val: &str| {
                let marker = if *focus_field == idx { "▸ " } else { "  " };
                let style = if *focus_field == idx {
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
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
                    "  Tab switch field · Ctrl+R reveal · Enter save · Esc cancel",
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
                "•".repeat(key.chars().count())
            };
            let field = |idx: usize, label: &str, val: &str| {
                let marker = if *focus_field == idx { "▸ " } else { "  " };
                let style = if *focus_field == idx {
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
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
                    "  Tab field · ←/→ protocol · Enter save · Esc cancel",
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
                    format!(" {provider_name} — OAuth Browser "),
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
            let mut lines = vec![
                Line::styled(
                    "  Open this URL in your browser to authorize:",
                    Style::default().fg(theme.dim),
                ),
            ];
            for chunk in wrap_text(url, w) {
                lines.push(Line::styled(
                    format!("  {chunk}"),
                    Style::default().fg(theme.secondary),
                ));
            }
            lines.push(Line::from(""));
            lines.push(Line::styled(
                if *busy {
                    "  Exchanging authorization code…"
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
                    format!(" {provider_name} — Device Flow "),
                    Style::default().fg(theme.accent),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let mut lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  1. Open:  ", Style::default().fg(theme.dim)),
                    Span::styled(verification_uri.clone(), Style::default().fg(theme.secondary)),
                ]),
                Line::from(vec![
                    Span::styled("  2. Code:  ", Style::default().fg(theme.dim)),
                    Span::styled(
                        user_code.clone(),
                        Style::default()
                            .fg(theme.ok)
                            .add_modifier(Modifier::BOLD),
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

// ------------------------------------------------------------------ text / markdown helpers

