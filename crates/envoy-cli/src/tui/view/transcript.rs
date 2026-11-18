//! Transcript rendering with clear User/Assistant hierarchy and folded intermediate activity.
use super::push_blank_linked;
use super::transcript_cards::{
    draw_welcome, render_explore_expanded_header, render_explore_group, render_tool_header,
    spinner,
};
use crate::tui::links::{inline_spans_linked, LinkedSpan};
use crate::tui::*;

pub(super) fn draw_transcript(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    app.transcript_height = area.height;
    app.transcript_area = area;

    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    if inner.width < 8 || inner.height == 0 {
        return;
    }

    if app.session.items.is_empty() {
        app.transcript_max_scroll = 0;
        app.transcript_scroll = 0;
        draw_welcome(frame, app, theme, inner);
        return;
    }

    let width = inner.width.max(16) as usize;
    let mut lines: Vec<LinkedLine> = Vec::new();
    let items_len = app.session.items.len();
    let mut idx = 0usize;
    let mut last_notice: Option<String> = None;

    while idx < items_len {
        let item = &app.session.items[idx];
        if matches!(item, TranscriptItem::Tool { name, .. } if name == "say") {
            idx += 1;
            continue;
        }
        if let TranscriptItem::Notice {
            text,
            is_error: false,
            ..
        } = item
        {
            if last_notice.as_deref() == Some(text.as_str()) {
                idx += 1;
                continue;
            }
        }

        let is_selected = app.focus == Focus::Transcript && app.selected_card == Some(idx);

        // Group consecutive collapsed explore/internal activity calls across empty thoughts.
        if is_groupable_tool(item) {
            let start = idx;
            let mut end = idx + 1;
            while end < items_len
                && (is_groupable_tool(&app.session.items[end])
                    || is_transparent_separator(&app.session.items[end]))
            {
                end += 1;
            }
            while end > start + 1 && is_transparent_separator(&app.session.items[end - 1]) {
                end -= 1;
            }

            let mut tool_count = 0usize;
            let mut first_id = 0u64;
            let mut all_collapsed = true;
            for entry in &app.session.items[start..end] {
                if let TranscriptItem::Tool {
                    id,
                    name,
                    collapsed,
                    ..
                } = entry
                {
                    if name == "say" {
                        continue;
                    }
                    if first_id == 0 {
                        first_id = *id;
                    }
                    tool_count += 1;
                    if !collapsed {
                        all_collapsed = false;
                    }
                }
            }

            if tool_count >= 2 && all_collapsed {
                last_notice = None;
                if !app.expanded_explore_groups.contains(&first_id) {
                    lines.push(render_explore_group(
                        &app.session.items[start..end],
                        width,
                        theme,
                        is_selected,
                    ));
                } else {
                    lines.push(render_explore_expanded_header(
                        &app.session.items[start..end],
                        width,
                        theme,
                    ));
                    for sub_idx in start..end {
                        if is_transparent_separator(&app.session.items[sub_idx])
                            || matches!(&app.session.items[sub_idx], TranscriptItem::Tool { name, .. } if name == "say")
                        {
                            continue;
                        }
                        let sub_selected =
                            app.focus == Focus::Transcript && app.selected_card == Some(sub_idx);
                        render_single_item(
                            app,
                            sub_idx,
                            width,
                            theme,
                            sub_selected,
                            &mut lines,
                            &mut last_notice,
                        );
                    }
                }
                idx = end;
                continue;
            }
        }

        render_single_item(
            app,
            idx,
            width,
            theme,
            is_selected,
            &mut lines,
            &mut last_notice,
        );
        idx += 1;
    }

    lines.push(LinkedLine::from_line(Line::from("")));
    let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
    app.transcript_max_scroll = max_scroll;
    if app.follow {
        app.transcript_scroll = max_scroll;
    } else {
        app.transcript_scroll = app.transcript_scroll.min(max_scroll);
    }

    super::inspector::push_linked_rows(frame, app, &lines, app.transcript_scroll, inner);
}

fn render_single_item(
    app: &mut App,
    idx: usize,
    width: usize,
    theme: &Theme,
    is_selected: bool,
    lines: &mut Vec<LinkedLine>,
    last_notice: &mut Option<String>,
) {
    let item = app.session.items[idx].clone();
    match &item {
        TranscriptItem::User { text, .. } => {
            *last_notice = None;
            push_blank_linked(lines);
            let bar_style = Style::default()
                .fg(if is_selected { theme.warn } else { theme.accent })
                .add_modifier(Modifier::BOLD);
            let card_bg = if is_selected {
                theme.selection
            } else {
                theme.surface
            };
            let text_style = Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD);
            for raw_line in text.lines() {
                for wrapped in wrap_text(raw_line, width.saturating_sub(3)) {
                    let mut ll = LinkedLine::from_line(Line::from(vec![
                        Span::styled("▌ ", bar_style),
                        Span::styled(wrapped, text_style),
                    ]));
                    ll.line_style = Some(Style::default().bg(card_bg));
                    lines.push(ll);
                }
            }
            push_blank_linked(lines);
        }
        TranscriptItem::Assistant {
            id,
            text,
            reasoning,
            reasoning_collapsed,
            done,
            error,
            ..
        } => {
            if text.trim().is_empty() && reasoning.is_empty() && *done && error.is_none() {
                return;
            }
            *last_notice = None;
            let thought_only = text.trim().is_empty() && *done && error.is_none();
            if !thought_only {
                push_blank_linked(lines);
            }

            if !reasoning.is_empty() {
                if *reasoning_collapsed {
                    let tail = reasoning
                        .lines()
                        .rev()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("");
                    let label = if !*done && !tail.is_empty() {
                        format!(
                            "  {} Thinking - {}",
                            spinner(),
                            truncate_width(tail, width.saturating_sub(18))
                        )
                    } else if !*done {
                        format!("  {} Thinking.", spinner())
                    } else {
                        "  > Thought".to_string()
                    };
                    lines.push(LinkedLine::from_linked(vec![LinkedSpan {
                        span: Span::styled(label, Style::default().fg(theme.dim)),
                        target: Some(HitTarget::ToggleReasoning(*id)),
                    }]));
                } else {
                    lines.push(LinkedLine::from_linked(vec![LinkedSpan {
                        span: Span::styled("  v Thinking", Style::default().fg(theme.dim)),
                        target: Some(HitTarget::ToggleReasoning(*id)),
                    }]));
                    for rline in reasoning.lines() {
                        for w in wrap_text(rline, width.saturating_sub(4)) {
                            lines.push(LinkedLine::from_line(Line::from(vec![
                                Span::styled("    ", Style::default().fg(theme.border)),
                                Span::styled(w, Style::default().fg(theme.dim)),
                            ])));
                        }
                    }
                }
            }

            let trimmed = text.trim_start_matches('\n');
            if trimmed.trim().is_empty() {
                if !*done && reasoning.is_empty() {
                    lines.push(LinkedLine::from_line(Line::styled(
                        format!("  {} Thinking.", spinner()),
                        Style::default().fg(theme.dim),
                    )));
                } else if !*done {
                    lines.push(LinkedLine::from_line(Line::styled(
                        "● ▍",
                        Style::default().fg(theme.accent),
                    )));
                }
            } else {
                let body_w = width.saturating_sub(1).max(10);
                let rendered = if *done {
                    let key = (*id, body_w, app.theme);
                    let fp = trimmed.len() ^ (app.session.changed_files.len() << 24);
                    if let Some((cached_fp, cached)) = app.transcript_cache.get(&key) {
                        if *cached_fp == fp {
                            cached.clone()
                        } else {
                            let r = render_markdown_linked(
                                trimmed,
                                body_w,
                                theme,
                                &app.session.cwd,
                                &app.session.changed_files,
                                &app.workspace_index,
                            );
                            app.transcript_cache.insert(key, (fp, r.clone()));
                            r
                        }
                    } else {
                        let r = render_markdown_linked(
                            trimmed,
                            body_w,
                            theme,
                            &app.session.cwd,
                            &app.session.changed_files,
                            &app.workspace_index,
                        );
                        app.transcript_cache.insert(key, (fp, r.clone()));
                        r
                    }
                } else {
                    render_markdown_linked(
                        trimmed,
                        body_w,
                        theme,
                        &app.session.cwd,
                        &app.session.changed_files,
                        &app.workspace_index,
                    )
                };

                for (line_idx, mut rline) in rendered.into_iter().enumerate() {
                    let prefix = if line_idx == 0 {
                        LinkedSpan {
                            span: Span::styled(
                                "●",
                                Style::default()
                                    .fg(theme.accent)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            target: None,
                        }
                    } else {
                        LinkedSpan {
                            span: Span::raw(" "),
                            target: None,
                        }
                    };
                    rline.spans.insert(0, prefix);
                    lines.push(rline);
                }
            }

            if let Some(err) = error {
                for w in wrap_text(&format!("! {err}"), width.saturating_sub(4)) {
                    lines.push(LinkedLine::from_line(Line::styled(
                        format!("  {w}"),
                        Style::default().fg(theme.bad),
                    )));
                }
            }
            if !thought_only {
                push_blank_linked(lines);
            }
        }
        TranscriptItem::Tool {
            id,
            name,
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
            *last_notice = None;
            lines.push(render_tool_header(
                app,
                *id,
                name,
                title,
                subtitle,
                target,
                output,
                *status,
                *collapsed,
                *duration_ms,
                *exit_code,
                *started_ms,
                width,
                theme,
                is_selected,
            ));

            if !*collapsed && !output.is_empty() {
                let total = output.len();
                let preview_start = if total > 50 && *status != CardStatus::Running {
                    lines.push(LinkedLine::from_line(Line::styled(
                        format!(
                            "    . ({} earlier lines - click [out] for full log)",
                            total - 40
                        ),
                        Style::default().fg(theme.dim),
                    )));
                    total - 40
                } else {
                    0
                };
                for out_line in &output[preview_start..] {
                    let style = if out_line.err {
                        Style::default().fg(theme.bad)
                    } else {
                        Style::default().fg(theme.dim)
                    };
                    for w in wrap_text(&out_line.text, width.saturating_sub(4)) {
                        lines.push(LinkedLine::from_line(Line::styled(
                            format!("    {w}"),
                            style,
                        )));
                    }
                }
            }
        }
        TranscriptItem::Notice { text, is_error, .. } => {
            if !*is_error && text.starts_with("[monitor #") {
                lines.push(LinkedLine::from_line(Line::styled(
                    format!("  · {}", truncate_width(text, width.saturating_sub(4))),
                    Style::default().fg(theme.dim),
                )));
                return;
            }
            *last_notice = if *is_error {
                None
            } else {
                Some(text.clone())
            };
            push_blank_linked(lines);
            let style = Style::default().fg(if *is_error { theme.bad } else { theme.dim });
            for raw in text.lines() {
                let linked = inline_spans_linked(
                    raw,
                    theme,
                    &app.session.cwd,
                    &app.session.changed_files,
                    &app.workspace_index,
                );
                let wrapped = wrap_linked_spans(
                    LinkedSpan {
                        span: Span::styled("  · ", style),
                        target: None,
                    },
                    LinkedSpan {
                        span: Span::styled("    ", style),
                        target: None,
                    },
                    linked,
                    width,
                );
                for row in wrapped {
                    let mut ll = LinkedLine::from_linked(row);
                    ll.line_style = Some(style);
                    lines.push(ll);
                }
            }
        }
        TranscriptItem::Say { text, level, .. } => {
            *last_notice = None;
            let style = Style::default().fg(match level.as_str() {
                "warning" => theme.warn,
                "error" => theme.bad,
                _ => theme.secondary,
            });
            let first = text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("");
            if !first.is_empty() {
                let single = if text.lines().count() > 1 {
                    format!("{}…", truncate_width(first, width.saturating_sub(5)))
                } else {
                    truncate_width(first, width.saturating_sub(4))
                };
                lines.push(LinkedLine::from_line(Line::from(vec![
                    Span::styled("  · ", Style::default().fg(theme.accent)),
                    Span::styled(single, style),
                ])));
            }
        }
    }
}

fn is_groupable_tool(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Tool { name, status, .. }
            if matches!(
                name.as_str(),
                "read" | "glob" | "grep" | "ls" | "todo" | "job" | "monitor" | "say"
            ) && *status != CardStatus::Error
    )
}

fn is_transparent_separator(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Assistant {
            text,
            reasoning_collapsed: true,
            done: true,
            error: None,
            ..
        } if text.trim().is_empty()
    ) || matches!(
        item,
        TranscriptItem::Notice {
            text,
            is_error: false,
            ..
        } if text.starts_with("[monitor #")
    )
}
