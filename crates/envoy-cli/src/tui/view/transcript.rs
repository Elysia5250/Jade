//! Transcript rendering with Cursor-style single-line collapsed activity and clickable links.
use super::push_blank_linked;
use crate::tui::links::{inline_spans_linked, normalize_display_path, LinkedSpan};
use crate::tui::*;
use std::path::Path;

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner() -> &'static str {
    SPIN[((now_ms() / 90) as usize) % SPIN.len()]
}

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

    while idx < items_len {
        let is_selected = app.focus == Focus::Transcript && app.selected_card == Some(idx);

        // Group consecutive collapsed read/search exploration calls into 1 dynamic Cursor-style line.
        if is_explore_tool(&app.session.items[idx]) {
            let start = idx;
            let mut end = idx + 1;
            while end < items_len && is_explore_tool(&app.session.items[end]) {
                end += 1;
            }
            let first_id = app.session.items[start].id();
            let all_collapsed = (start..end).all(|i| {
                matches!(
                    app.session.items[i],
                    TranscriptItem::Tool { collapsed: true, .. }
                )
            });
            if end - start >= 2 && all_collapsed && !app.expanded_explore_groups.contains(&first_id) {
                lines.push(render_explore_group(
                    &app.session.items[start..end],
                    width,
                    theme,
                    is_selected,
                ));
                idx = end;
                continue;
            }
        }

        match &app.session.items[idx] {
            TranscriptItem::User { text, .. } => {
                push_blank_linked(&mut lines);
                let bar_style = Style::default().fg(if is_selected {
                    theme.warn
                } else {
                    theme.accent
                });
                for raw_line in text.lines() {
                    for wrapped in wrap_text(raw_line, width.saturating_sub(2)) {
                        lines.push(LinkedLine::from_line(Line::from(vec![
                            Span::styled("▌ ", bar_style),
                            Span::styled(wrapped, Style::default().fg(theme.text)),
                        ])));
                    }
                }
                push_blank_linked(&mut lines);
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
                if text.trim().is_empty()
                    && reasoning.is_empty()
                    && *done
                    && error.is_none()
                {
                    idx += 1;
                    continue;
                }
                push_blank_linked(&mut lines);

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
                                "{} Thinking · {}",
                                spinner(),
                                truncate_width(tail, width.saturating_sub(16))
                            )
                        } else if !*done {
                            format!("{} Thinking…", spinner())
                        } else {
                            "▸ Thought".to_string()
                        };
                        lines.push(LinkedLine::from_linked(vec![LinkedSpan {
                            span: Span::styled(label, Style::default().fg(theme.dim)),
                            target: Some(HitTarget::ToggleReasoning(*id)),
                        }]));
                    } else {
                        lines.push(LinkedLine::from_linked(vec![LinkedSpan {
                            span: Span::styled("▾ Thinking", Style::default().fg(theme.dim)),
                            target: Some(HitTarget::ToggleReasoning(*id)),
                        }]));
                        for rline in reasoning.lines() {
                            for w in wrap_text(rline, width.saturating_sub(2)) {
                                lines.push(LinkedLine::from_line(Line::from(vec![
                                    Span::styled("  ", Style::default().fg(theme.border)),
                                    Span::styled(w, Style::default().fg(theme.dim)),
                                ])));
                            }
                        }
                    }
                }

                if text.trim().is_empty() {
                    if !*done && reasoning.is_empty() {
                        lines.push(LinkedLine::from_line(Line::styled(
                            format!("{} Thinking…", spinner()),
                            Style::default().fg(theme.dim),
                        )));
                    } else if !*done {
                        lines.push(LinkedLine::from_line(Line::styled(
                            "▍",
                            Style::default().fg(theme.accent),
                        )));
                    }
                } else if *done {
                    let key = (*id, width, app.theme);
                    let fp = text.len() ^ (app.session.changed_files.len() << 24);
                    if let Some((cached_fp, cached)) = app.transcript_cache.get(&key) {
                        if *cached_fp == fp {
                            lines.extend(cached.iter().cloned());
                        } else {
                            let rendered = render_markdown_linked(
                                text,
                                width,
                                theme,
                                &app.session.cwd,
                                &app.session.changed_files,
                                &app.workspace_index,
                            );
                            app.transcript_cache.insert(key, (fp, rendered.clone()));
                            lines.extend(rendered);
                        }
                    } else {
                        let rendered = render_markdown_linked(
                            text,
                            width,
                            theme,
                            &app.session.cwd,
                            &app.session.changed_files,
                            &app.workspace_index,
                        );
                        app.transcript_cache.insert(key, (fp, rendered.clone()));
                        lines.extend(rendered);
                    }
                } else {
                    let rendered = render_markdown_linked(
                        text,
                        width,
                        theme,
                        &app.session.cwd,
                        &app.session.changed_files,
                        &app.workspace_index,
                    );
                    lines.extend(rendered);
                }

                if let Some(err) = error {
                    for w in wrap_text(&format!("! {err}"), width.saturating_sub(2)) {
                        lines.push(LinkedLine::from_line(Line::styled(
                            format!(" {w}"),
                            Style::default().fg(theme.bad),
                        )));
                    }
                }
                push_blank_linked(&mut lines);
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
                            format!("  … ({} earlier lines — click [out] for full log)", total - 40),
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
                            Style::default().fg(theme.text)
                        };
                        for w in wrap_text(&out_line.text, width.saturating_sub(4)) {
                            lines.push(LinkedLine::from_line(Line::styled(format!("    {w}"), style)));
                        }
                    }
                }
            }
            TranscriptItem::Notice { text, is_error, .. } => {
                push_blank_linked(&mut lines);
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
                            span: Span::styled("· ", style),
                            target: None,
                        },
                        LinkedSpan {
                            span: Span::styled("  ", style),
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
                push_blank_linked(&mut lines);
                let style = Style::default().fg(match level.as_str() {
                    "warning" => theme.warn,
                    "error" => theme.bad,
                    _ => theme.secondary,
                });
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
                            span: Span::styled("· ", Style::default().fg(theme.accent)),
                            target: None,
                        },
                        LinkedSpan {
                            span: Span::raw("  "),
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

fn is_explore_tool(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Tool { name, .. } if matches!(name.as_str(), "read" | "glob" | "grep" | "ls")
    )
}

fn render_explore_group(
    group: &[TranscriptItem],
    width: usize,
    theme: &Theme,
    is_selected: bool,
) -> LinkedLine {
    let ids: Vec<u64> = group.iter().map(TranscriptItem::id).collect();
    let any_running = group.iter().any(|i| {
        matches!(
            i,
            TranscriptItem::Tool {
                status: CardStatus::Running,
                ..
            }
        )
    });
    let any_err = group.iter().any(|i| {
        matches!(
            i,
            TranscriptItem::Tool {
                status: CardStatus::Error,
                ..
            }
        )
    });

    let mut counts: Vec<(&str, usize)> = Vec::new();
    let mut latest_title = "";
    let mut min_start = u64::MAX;
    let mut total_dur = 0u64;
    for item in group {
        if let TranscriptItem::Tool {
            name,
            title,
            started_ms,
            duration_ms,
            ..
        } = item
        {
            latest_title = title.as_str();
            min_start = min_start.min(*started_ms);
            total_dur += duration_ms.unwrap_or_else(|| now_ms().saturating_sub(*started_ms));
            if let Some((_, c)) = counts.iter_mut().find(|(n, _)| *n == name.as_str()) {
                *c += 1;
            } else {
                counts.push((name.as_str(), 1));
            }
        }
    }
    let summary: String = counts
        .iter()
        .map(|(n, c)| format!("{n} ×{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let elapsed = if any_running {
        fmt_duration(now_ms().saturating_sub(min_start))
    } else {
        fmt_duration(total_dur)
    };

    let (glyph, glyph_fg) = if any_running {
        (spinner(), theme.accent)
    } else if any_err {
        ("✗", theme.bad)
    } else {
        ("▸", theme.dim)
    };

    let text = if any_running {
        format!(
            "Exploring · {}  ({summary})  {elapsed}",
            truncate_width(latest_title, width.saturating_sub(summary.len() + 22).max(8))
        )
    } else {
        format!("Explored {} ops ({summary})  {elapsed}", group.len())
    };

    let mut line = LinkedLine::from_linked(vec![
        LinkedSpan {
            span: Span::styled(format!("{glyph} "), Style::default().fg(glyph_fg)),
            target: Some(HitTarget::ToggleToolGroup(ids.clone())),
        },
        LinkedSpan {
            span: Span::styled(
                truncate_width(&text, width.saturating_sub(3)),
                Style::default().fg(theme.dim),
            ),
            target: Some(HitTarget::ToggleToolGroup(ids)),
        },
    ]);
    if is_selected {
        line.line_style = Some(Style::default().bg(theme.selection));
    }
    line
}

#[allow(clippy::too_many_arguments)]
fn render_tool_header(
    app: &App,
    id: u64,
    name: &str,
    title: &str,
    subtitle: &str,
    target: &str,
    output: &[crate::session::OutputLine],
    status: CardStatus,
    collapsed: bool,
    duration_ms: Option<u64>,
    exit_code: Option<i32>,
    started_ms: u64,
    width: usize,
    theme: &Theme,
    is_selected: bool,
) -> LinkedLine {
    let (glyph, glyph_style) = match status {
        CardStatus::Queued => ("○", Style::default().fg(theme.warn)),
        CardStatus::Running => (spinner(), Style::default().fg(theme.accent)),
        CardStatus::Done => ("✓", Style::default().fg(theme.ok)),
        CardStatus::Error => ("✗", Style::default().fg(theme.bad)),
    };

    let elapsed = duration_ms.unwrap_or_else(|| now_ms().saturating_sub(started_ms));
    let mut meta_parts = vec![fmt_duration(elapsed)];
    if let Some(code) = exit_code.filter(|c| *c != 0) {
        meta_parts.push(format!("exit {code}"));
    }
    if target != "local" {
        meta_parts.push(target.to_string());
    }

    // Check if this tool acts directly on a file path (`edit`, `write`, `read`, `ls`)
    let file_verb = match name {
        "edit" => title.strip_prefix("edit ").map(|p| ("Edited", "Editing", p)),
        "write" => title.strip_prefix("write ").map(|p| ("Wrote", "Writing", p)),
        "read" => title.strip_prefix("read ").map(|p| ("Read", "Reading", p)),
        _ => None,
    };

    let mut spans = vec![LinkedSpan {
        span: Span::styled(format!("{glyph} "), glyph_style),
        target: Some(HitTarget::ToggleToolCard(id)),
    }];

    if let Some((done_verb, run_verb, raw_path)) = file_verb {
        let rel = normalize_display_path(&app.session.cwd, Path::new(raw_path.trim()));
        let verb = if status == CardStatus::Running {
            run_verb
        } else {
            done_verb
        };
        let mut diff_badge = String::new();
        if matches!(name, "edit" | "write") {
            if let Some(idx) = app.find_changed_file_index(&rel) {
                let fc = &app.session.changed_files[idx];
                diff_badge = format!("  +{} -{}", fc.added, fc.removed);
            }
        }
        let meta = format!("  {}", meta_parts.join(" · "));
        let budget = width
            .saturating_sub(display_width(verb) + display_width(&diff_badge) + display_width(&meta) + 4)
            .max(10);

        spans.push(LinkedSpan {
            span: Span::styled(
                format!("{verb} "),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
            target: Some(HitTarget::ToggleToolCard(id)),
        });
        spans.push(LinkedSpan {
            span: Span::styled(
                truncate_width(&rel, budget),
                Style::default()
                    .fg(theme.link)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            target: Some(HitTarget::OpenFile {
                path: rel,
                line: None,
            }),
        });
        if !diff_badge.is_empty() {
            spans.push(LinkedSpan {
                span: Span::styled(diff_badge, Style::default().fg(theme.ok)),
                target: None,
            });
        }
        spans.push(LinkedSpan {
            span: Span::styled(meta, Style::default().fg(theme.dim)),
            target: Some(HitTarget::ToggleToolCard(id)),
        });
    } else {
        let live_tail = if status == CardStatus::Running {
            output
                .iter()
                .rev()
                .map(|l| l.text.trim())
                .find(|l| !l.is_empty())
                .map(|l| format!(" · {l}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let out_pill = if !output.is_empty() || matches!(name, "exec" | "shell" | "job") {
            "  [out]"
        } else {
            ""
        };
        let meta = format!("  {}", meta_parts.join(" · "));
        let reserved = 2 + display_width(&meta) + display_width(out_pill);
        let title_budget = width.saturating_sub(reserved).max(10);
        let main_label = if !live_tail.is_empty() {
            format!("{title}{live_tail}")
        } else if !subtitle.is_empty() && title_budget > 24 {
            format!("{title}  {subtitle}")
        } else {
            title.to_string()
        };
        spans.push(LinkedSpan {
            span: Span::styled(
                truncate_width(&main_label, title_budget),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
            target: Some(HitTarget::ToggleToolCard(id)),
        });
        spans.push(LinkedSpan {
            span: Span::styled(meta, Style::default().fg(theme.dim)),
            target: Some(HitTarget::ToggleToolCard(id)),
        });
        if !out_pill.is_empty() {
            spans.push(LinkedSpan {
                span: Span::styled(
                    out_pill,
                    Style::default()
                        .fg(theme.link)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                target: Some(HitTarget::OpenToolOutput(id)),
            });
        }
        if !collapsed {
            spans.push(LinkedSpan {
                span: Span::styled(
                    "  [info]",
                    Style::default()
                        .fg(theme.dim)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                target: Some(HitTarget::OpenToolInspect(id)),
            });
        }
    }

    let mut ll = LinkedLine::from_linked(spans);
    if is_selected {
        ll.line_style = Some(Style::default().bg(theme.selection));
    }
    ll
}

fn draw_welcome(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let w = area.width as usize;
    let mut rows = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Envoy  ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
            Span::styled(
                truncate_width(&app.cwd_label(), w.saturating_sub(12)),
                Style::default().fg(theme.text),
            ),
        ]),
    ];

    if let Some((prov, model)) = resolve_active_provider(&app.data_dir) {
        let budget = crate::agent::context::resolve_budget(&app.data_dir, &prov.id, &model.model_id);
        rows.push(Line::from(vec![
            Span::styled("  model  ", Style::default().fg(theme.dim)),
            Span::styled(
                format!("{}  ({} · compact {})", model.model_id, budget.tier, fmt_tokens(budget.compact_at)),
                Style::default().fg(theme.secondary),
            ),
        ]));
    } else {
        rows.push(Line::from(vec![
            Span::styled("  auth   ", Style::default().fg(theme.dim)),
            Span::styled("sign in with /login", Style::default().fg(theme.warn)),
        ]));
    }

    let mcp = crate::mcp::config::load_mcp_configs(&app.data_dir, &app.session.cwd);
    let skills = crate::skills::discover_skills(&app.data_dir, &app.session.cwd);
    rows.push(Line::from(vec![
        Span::styled("  tools  ", Style::default().fg(theme.dim)),
        Span::styled(
            format!(
                "{} mcp · {} skills · perm:{}",
                mcp.servers.len(),
                skills.len(),
                app.perm_mode.label()
            ),
            Style::default().fg(theme.text),
        ),
    ]));
    rows.push(Line::from(""));
    rows.push(Line::styled(
        truncate_width(
            "  Click any underlined file link to review in the right Inspector · Drag divider to resize",
            w,
        ),
        Style::default().fg(theme.dim),
    ));
    rows.push(Line::styled(
        truncate_width(
            "  Shift+Tab permissions · Ctrl+B panel · Ctrl+O expand · / commands",
            w,
        ),
        Style::default().fg(theme.dim),
    ));

    frame.render_widget(Paragraph::new(rows), area);
}
