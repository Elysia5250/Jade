//! Transcript rendering with clear User/Assistant hierarchy and folded intermediate activity.
use super::{ensure_trailing_blanks, push_blank_linked};
use super::transcript_cards::{
    push_thinking_lines, render_explore_expanded_header, render_explore_group,
    render_thinking_run, render_tool_header, render_turn_summary,
};
use super::welcome::draw_welcome;
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
    let folds = settled_activity_folds(&app.session.items, app.agent_running);
    let mut idx = 0usize;
    let mut last_notice: Option<String> = None;

    while idx < items_len {
        if let Some((start, end)) = folds.iter().copied().find(|(s, e)| idx >= *s && idx < *e) {
            let first_id = first_tool_id(&app.session.items[start..end]);
            let expanded = app.expanded_explore_groups.contains(&first_id);
            let selected = app.focus == Focus::Transcript
                && app.selected_card.is_some_and(|sel| (start..end).contains(&sel));
            push_blank_linked(&mut lines);
            if expanded {
                lines.push(render_turn_summary(
                    &app.session.items[start..end],
                    &app.session.changed_files,
                    width,
                    theme,
                    selected,
                    true,
                ));
                for sub_idx in start..end {
                    if is_folded_filler(&app.session.items[sub_idx]) {
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
            } else {
                lines.push(render_turn_summary(
                    &app.session.items[start..end],
                    &app.session.changed_files,
                    width,
                    theme,
                    selected,
                    false,
                ));
            }
            push_blank_linked(&mut lines);
            idx = end;
            last_notice = None;
            continue;
        }

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

        // Merge consecutive reasoning-only Assistant turns into a single Thinking toggle.
        if is_reasoning_only(item) {
            let mut end = idx + 1;
            while end < items_len
                && (is_reasoning_only(&app.session.items[end])
                    || is_transparent_notice(&app.session.items[end]))
            {
                end += 1;
            }
            if render_thinking_run(&app.session.items[idx..end], width, theme, &mut lines) {
                last_notice = None;
            }
            idx = end;
            continue;
        }

        // Group consecutive collapsed explore/command/non-edit calls across empty thoughts.
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
                push_blank_linked(&mut lines);
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
                push_blank_linked(&mut lines);
                idx = end;
                continue;
            }
        }

        let is_tool = matches!(item, TranscriptItem::Tool { .. });
        if is_tool {
            push_blank_linked(&mut lines);
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
        if is_tool {
            push_blank_linked(&mut lines);
        }
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
            // A second user message starts another turn; leave a wider gap.
            ensure_trailing_blanks(lines, 2);
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
            let card_line_bg = Style::default().bg(card_bg);
            let mut top_pad = LinkedLine::from_line(Line::styled("▌ ", bar_style));
            top_pad.line_style = Some(card_line_bg);
            lines.push(top_pad);
            let raw_lines: Vec<&str> = if text.is_empty() {
                vec![""]
            } else {
                text.lines().collect()
            };
            for raw_line in raw_lines {
                for wrapped in wrap_text(raw_line, width.saturating_sub(3)) {
                    let mut ll = LinkedLine::from_line(Line::from(vec![
                        Span::styled("▌ ", bar_style),
                        Span::styled(wrapped, text_style),
                    ]));
                    ll.line_style = Some(card_line_bg);
                    lines.push(ll);
                }
            }
            let mut bot_pad = LinkedLine::from_line(Line::styled("▌ ", bar_style));
            bot_pad.line_style = Some(card_line_bg);
            lines.push(bot_pad);
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
            push_blank_linked(lines);

            if !reasoning.is_empty() {
                push_thinking_lines(
                    lines,
                    vec![*id],
                    &[reasoning.as_str()],
                    !*done,
                    *reasoning_collapsed,
                    width,
                    theme,
                );
                push_blank_linked(lines);
            }

            let trimmed = text.trim_start_matches('\n');
            if trimmed.trim().is_empty() {
                if !*done && reasoning.is_empty() {
                    let mut spans = vec![LinkedSpan {
                        span: Span::raw("  "),
                        target: None,
                    }];
                    spans.extend(decrypt_spans("Thinking", 0, now_ms(), *id, theme, None));
                    lines.push(LinkedLine::from_linked(spans));
                    push_blank_linked(lines);
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

                let bullet = "●";
                let cont = " ".repeat(display_width(bullet));
                for (line_idx, mut rline) in rendered.into_iter().enumerate() {
                    let prefix = if line_idx == 0 {
                        LinkedSpan {
                            span: Span::styled(
                                bullet,
                                Style::default()
                                    .fg(theme.accent)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            target: None,
                        }
                    } else {
                        LinkedSpan {
                            span: Span::raw(cont.clone()),
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
            push_blank_linked(lines);
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
                push_blank_linked(lines);
                let single = if text.lines().count() > 1 {
                    format!("{}…", truncate_width(first, width.saturating_sub(5)))
                } else {
                    truncate_width(first, width.saturating_sub(4))
                };
                lines.push(LinkedLine::from_line(Line::from(vec![
                    Span::styled("  · ", Style::default().fg(theme.accent)),
                    Span::styled(single, style),
                ])));
                push_blank_linked(lines);
            }
        }
    }
}

fn is_groupable_tool(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Tool { name, .. }
            if matches!(
                name.as_str(),
                "read"
                    | "glob"
                    | "grep"
                    | "ls"
                    | "todo"
                    | "job"
                    | "monitor"
                    | "say"
                    | "exec"
                    | "shell"
                    | "skill"
                    | "webfetch"
                    | "delegate"
                    | "ask"
            )
    )
}

fn is_reasoning_only(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Assistant {
            text,
            error: None,
            ..
        } if text.trim().is_empty()
    )
}

fn is_transparent_notice(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Notice {
            text,
            is_error: false,
            ..
        } if text.starts_with("[monitor #")
    )
}

fn first_tool_id(items: &[TranscriptItem]) -> u64 {
    items
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Tool { id, name, .. } if name != "say" => Some(*id),
            _ => None,
        })
        .unwrap_or(0)
}

/// A finished turn collapses its tools into one line. The tail stays open
/// while the agent is still working.
fn settled_activity_folds(items: &[TranscriptItem], agent_running: bool) -> Vec<(usize, usize)> {
    let mut folds = Vec::new();
    let mut body_start = 0usize;
    for i in 0..=items.len() {
        let at_user = i < items.len() && matches!(items[i], TranscriptItem::User { .. });
        if !at_user && i != items.len() {
            continue;
        }
        let body_end = i;
        let is_tail = i == items.len();
        if body_end > body_start && !(is_tail && agent_running) {
            let mut cursor = body_start;
            while cursor < body_end {
                if !is_activity_item(&items[cursor]) {
                    cursor += 1;
                    continue;
                }
                let mut end = cursor + 1;
                while end < body_end && is_activity_item(&items[end]) {
                    end += 1;
                }
                let slice = &items[cursor..end];
                let has_tool = slice.iter().any(|item| {
                    matches!(item, TranscriptItem::Tool { name, .. } if name != "say")
                });
                let running = slice.iter().any(|item| {
                    matches!(
                        item,
                        TranscriptItem::Tool {
                            status: CardStatus::Running,
                            ..
                        }
                    )
                });
                // An opened card stays on screen; collapsing it again returns
                // the one-line turn summary.
                let opened = slice.iter().any(|item| {
                    matches!(
                        item,
                        TranscriptItem::Tool {
                            name,
                            collapsed: false,
                            ..
                        } if name != "say"
                    )
                });
                if has_tool && !running && !opened {
                    folds.push((cursor, end));
                }
                cursor = end;
            }
        }
        body_start = i + 1;
    }
    folds
}

fn is_activity_item(item: &TranscriptItem) -> bool {
    match item {
        TranscriptItem::Tool { .. } | TranscriptItem::Say { .. } => true,
        TranscriptItem::Assistant {
            text,
            done: true,
            error: None,
            ..
        } if text.trim().is_empty() => true,
        TranscriptItem::Notice {
            text,
            is_error: false,
            ..
        } if text.starts_with("[monitor #") => true,
        _ => false,
    }
}

fn is_folded_filler(item: &TranscriptItem) -> bool {
    is_transparent_separator(item)
        || matches!(item, TranscriptItem::Tool { name, .. } if name == "say")
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
