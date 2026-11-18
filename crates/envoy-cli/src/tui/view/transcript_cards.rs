//! Compact tool headers, collapsible activity groups, and empty-session welcome pane.
use crate::tui::links::{normalize_display_path, LinkedSpan};
use crate::tui::*;
use std::path::Path;

pub(super) fn activity_sentence(
    items: &[TranscriptItem],
    changes: &[crate::session::FileChange],
) -> String {
    let mut edited: Vec<String> = Vec::new();
    let mut explored: Vec<String> = Vec::new();
    let mut explored_extra = 0usize;
    let mut commands = 0usize;
    let mut other = 0usize;
    let mut any_err = false;
    for item in items {
        let TranscriptItem::Tool {
            name,
            title,
            status,
            ..
        } = item
        else {
            continue;
        };
        if name == "say" {
            continue;
        }
        if *status == CardStatus::Error {
            any_err = true;
        }
        match name.as_str() {
            "edit" | "write" => {
                let path = title_path(title, name);
                if !path.is_empty() && !edited.iter().any(|p| p == &path) {
                    edited.push(path);
                }
            }
            "read" | "ls" => {
                let path = title_path(title, name);
                if path.is_empty() {
                    explored_extra += 1;
                } else if !explored.iter().any(|p| p == &path) {
                    explored.push(path);
                }
            }
            "glob" | "grep" => explored_extra += 1,
            "exec" | "shell" => commands += 1,
            _ => other += 1,
        }
    }
    let mut added = 0usize;
    let mut removed = 0usize;
    for path in &edited {
        if let Some(change) = changes.iter().find(|c| {
            c.path == *path || c.path.ends_with(&format!("/{path}")) || path.ends_with(&c.path)
        }) {
            added += change.added;
            removed += change.removed;
        }
    }
    let explored_n = explored.len() + explored_extra;
    let mut parts = Vec::new();
    if !edited.is_empty() {
        parts.push(format!("Edited {}", count_words(edited.len(), "file", "files")));
    }
    if explored_n > 0 {
        parts.push(format!(
            "explored {}",
            count_words(explored_n, "file", "files")
        ));
    }
    if commands > 0 {
        parts.push(format!(
            "ran {}",
            count_words(commands, "command", "commands")
        ));
    }
    if other > 0 {
        if parts.is_empty() {
            parts.push(count_words(other, "tool", "tools"));
        } else {
            parts.push(format!("{} more", other));
        }
    }
    let mut text = if parts.is_empty() {
        "Worked".to_string()
    } else {
        let mut joined = parts.join(", ");
        if let Some(rest) = joined.strip_prefix("explored") {
            joined = format!("Explored{rest}");
        } else if let Some(rest) = joined.strip_prefix("ran") {
            joined = format!("Ran{rest}");
        }
        joined
    };
    if added > 0 || removed > 0 {
        text.push_str(&format!(" +{added} -{removed}"));
    }
    if any_err {
        text.push_str(" (failed)");
    }
    text
}

fn count_words(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

fn title_path(title: &str, name: &str) -> String {
    title
        .strip_prefix(&format!("{name} "))
        .unwrap_or("")
        .trim()
        .to_string()
}

pub(super) fn render_turn_summary(
    items: &[TranscriptItem],
    changes: &[crate::session::FileChange],
    width: usize,
    theme: &Theme,
    is_selected: bool,
    expanded: bool,
) -> LinkedLine {
    let mut ids = Vec::new();
    for item in items {
        if let TranscriptItem::Tool { id, name, .. } = item {
            if name != "say" {
                ids.push(*id);
            }
        }
    }
    let sentence = activity_sentence(items, changes);
    let glyph = if expanded { "v" } else { ">" };
    let glyph_color = if sentence.ends_with("(failed)") {
        theme.bad
    } else {
        theme.accent
    };
    let mut spans = vec![LinkedSpan {
        span: Span::styled(format!("  {glyph} "), Style::default().fg(glyph_color)),
        target: Some(HitTarget::ToggleToolGroup(ids.clone())),
    }];
    let budget = width.saturating_sub(16);
    if let Some((head, diff_tail)) = sentence.rsplit_once(" +") {
        let diff_part = format!(" +{diff_tail}");
        let head_shown = truncate_width(head, budget.saturating_sub(display_width(&diff_part)));
        spans.push(LinkedSpan {
            span: Span::styled(head_shown, Style::default().fg(theme.dim)),
            target: Some(HitTarget::ToggleToolGroup(ids.clone())),
        });
        spans.push(LinkedSpan {
            span: Span::styled(diff_part, Style::default().fg(theme.ok)),
            target: Some(HitTarget::ToggleToolGroup(ids.clone())),
        });
    } else {
        spans.push(LinkedSpan {
            span: Span::styled(
                truncate_width(&sentence, budget),
                Style::default().fg(theme.dim),
            ),
            target: Some(HitTarget::ToggleToolGroup(ids.clone())),
        });
    }
    if expanded {
        spans.push(LinkedSpan {
            span: Span::raw("  "),
            target: None,
        });
        spans.push(LinkedSpan {
            span: Span::styled(
                "[collapse]",
                Style::default()
                    .fg(theme.link)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            target: Some(HitTarget::ToggleToolGroup(ids)),
        });
    }
    let mut line = LinkedLine::from_linked(spans);
    if is_selected {
        line.line_style = Some(Style::default().bg(theme.selection));
    }
    line
}

pub(super) fn render_explore_group(
    group: &[TranscriptItem],
    width: usize,
    theme: &Theme,
    is_selected: bool,
) -> LinkedLine {
    let mut ids: Vec<u64> = Vec::new();
    let mut any_running = false;
    let mut min_start = u64::MAX;
    let mut total_dur = 0u64;

    for item in group {
        let TranscriptItem::Tool {
            id,
            name,
            status,
            started_ms,
            duration_ms,
            ..
        } = item
        else {
            continue;
        };
        ids.push(*id);
        if name == "say" {
            continue;
        }
        if *status == CardStatus::Running {
            any_running = true;
        }
        min_start = min_start.min(*started_ms);
        total_dur += duration_ms.unwrap_or_else(|| now_ms().saturating_sub(*started_ms));
    }

    let elapsed = fmt_duration(if min_start == u64::MAX {
        0
    } else if any_running {
        now_ms().saturating_sub(min_start)
    } else {
        total_dur
    });
    let mut line = if any_running {
        let phrase = running_group_phrase(group);
        let salt = ids.first().copied().unwrap_or(1);
        let target = Some(HitTarget::ToggleToolGroup(ids));
        let mut spans = vec![LinkedSpan {
            span: Span::raw("  "),
            target: target.clone(),
        }];
        spans.extend(decrypt_spans(
            phrase,
            0,
            now_ms().saturating_sub(min_start),
            salt,
            theme,
            target.clone(),
        ));
        spans.push(LinkedSpan {
            span: Span::styled(format!("  {elapsed}"), Style::default().fg(theme.dim)),
            target,
        });
        LinkedLine::from_linked(spans)
    } else {
        let summary = activity_sentence(group, &[]);
        let text = format!("{summary}  {elapsed}");
        LinkedLine::from_linked(vec![
            LinkedSpan {
                span: Span::styled("  > ", Style::default().fg(theme.dim)),
                target: Some(HitTarget::ToggleToolGroup(ids.clone())),
            },
            LinkedSpan {
                span: Span::styled(
                    truncate_width(&text, width.saturating_sub(5)),
                    Style::default().fg(theme.dim),
                ),
                target: Some(HitTarget::ToggleToolGroup(ids)),
            },
        ])
    };
    if is_selected {
        line.line_style = Some(Style::default().bg(theme.selection));
    }
    line
}

pub(super) fn render_explore_expanded_header(
    group: &[TranscriptItem],
    width: usize,
    theme: &Theme,
) -> LinkedLine {
    let mut ids: Vec<u64> = Vec::new();
    for item in group {
        if let TranscriptItem::Tool { id, name, .. } = item {
            if name == "say" {
                continue;
            }
            ids.push(*id);
        }
    }
    let summary = activity_sentence(group, &[]);
    let label = format!("  v {summary}  ");
    LinkedLine::from_linked(vec![
        LinkedSpan {
            span: Span::styled(
                truncate_width(&label, width.saturating_sub(14)),
                Style::default().fg(theme.dim),
            ),
            target: Some(HitTarget::ToggleToolGroup(ids.clone())),
        },
        LinkedSpan {
            span: Span::styled(
                "[collapse]",
                Style::default()
                    .fg(theme.link)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            target: Some(HitTarget::ToggleToolGroup(ids)),
        },
    ])
}

pub(super) fn push_thinking_lines(
    lines: &mut Vec<LinkedLine>,
    ids: Vec<u64>,
    bodies: &[&str],
    any_running: bool,
    all_collapsed: bool,
    width: usize,
    theme: &Theme,
) {
    if all_collapsed {
        let target = Some(HitTarget::ToggleReasoning(ids.clone()));
        let mut spans = vec![LinkedSpan {
            span: Span::raw("  "),
            target: target.clone(),
        }];
        if any_running {
            let salt = ids.first().copied().unwrap_or(1);
            spans.extend(decrypt_spans("Thinking", 0, now_ms(), salt, theme, target));
        } else {
            spans.push(LinkedSpan {
                span: Span::styled("> Thinking", Style::default().fg(theme.dim)),
                target,
            });
        }
        lines.push(LinkedLine::from_linked(spans));
        return;
    }
    let target = Some(HitTarget::ToggleReasoning(ids.clone()));
    let mut header = vec![LinkedSpan {
        span: Span::styled("  v ", Style::default().fg(theme.dim)),
        target: target.clone(),
    }];
    if any_running {
        let salt = ids.first().copied().unwrap_or(1);
        header.extend(decrypt_spans("Thinking", 0, now_ms(), salt, theme, target));
    } else {
        header.push(LinkedSpan {
            span: Span::styled("Thinking", Style::default().fg(theme.dim)),
            target,
        });
    }
    lines.push(LinkedLine::from_linked(header));
    for (idx, body) in bodies.iter().enumerate() {
        if idx > 0 {
            super::push_blank_linked(lines);
        }
        for rline in body.lines() {
            for w in wrap_text(rline, width.saturating_sub(4)) {
                lines.push(LinkedLine::from_line(ratatui::text::Line::from(vec![
                    Span::styled("    ", Style::default().fg(theme.border)),
                    Span::styled(w, Style::default().fg(theme.dim)),
                ])));
            }
        }
    }
}

pub(super) fn render_thinking_run(
    slice: &[TranscriptItem],
    width: usize,
    theme: &Theme,
    lines: &mut Vec<LinkedLine>,
) -> bool {
    let mut ids = Vec::new();
    let mut bodies = Vec::new();
    let mut any_running = false;
    let mut all_collapsed = true;
    for item in slice {
        let TranscriptItem::Assistant {
            id,
            reasoning,
            reasoning_collapsed,
            done,
            ..
        } = item
        else {
            continue;
        };
        if !*done {
            any_running = true;
        }
        if !reasoning.is_empty() {
            ids.push(*id);
            bodies.push(reasoning.as_str());
            if !*reasoning_collapsed {
                all_collapsed = false;
            }
        }
    }
    if bodies.is_empty() {
        if !any_running {
            return false;
        }
        super::push_blank_linked(lines);
        let mut spans = vec![LinkedSpan {
            span: Span::raw("  "),
            target: None,
        }];
        spans.extend(decrypt_spans("Thinking", 0, now_ms(), 1, theme, None));
        lines.push(LinkedLine::from_linked(spans));
        super::push_blank_linked(lines);
        return true;
    }
    super::push_blank_linked(lines);
    push_thinking_lines(lines, ids, &bodies, any_running, all_collapsed, width, theme);
    super::push_blank_linked(lines);
    true
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_tool_header(
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
    let elapsed = duration_ms.unwrap_or_else(|| now_ms().saturating_sub(started_ms));
    let mut meta_parts = vec![fmt_duration(elapsed)];
    if let Some(code) = exit_code.filter(|c| *c != 0) {
        meta_parts.push(format!("exit {code}"));
    }
    if target != "local" {
        meta_parts.push(target.to_string());
    }

    let file_verb = match name {
        "edit" => title.strip_prefix("edit ").map(|p| ("Edited", "Editing", p)),
        "write" => title.strip_prefix("write ").map(|p| ("Wrote", "Writing", p)),
        "read" => title.strip_prefix("read ").map(|p| ("Read", "Reading", p)),
        _ => None,
    };

    let mut spans = match status {
        CardStatus::Running => vec![LinkedSpan {
            span: Span::raw("  "),
            target: Some(HitTarget::ToggleToolCard(id)),
        }],
        _ => {
            let (glyph, glyph_style) = match status {
                CardStatus::Queued => ("o", Style::default().fg(theme.warn)),
                CardStatus::Done => ("✓", Style::default().fg(theme.ok)),
                CardStatus::Error => ("✗", Style::default().fg(theme.bad)),
                CardStatus::Running => unreachable!(),
            };
            vec![LinkedSpan {
                span: Span::styled(format!("  {glyph} "), glyph_style),
                target: Some(HitTarget::ToggleToolCard(id)),
            }]
        }
    };

    if status == CardStatus::Running && collapsed {
        let phrase = file_verb
            .map(|(_, run_verb, _)| run_verb)
            .unwrap_or_else(|| running_tool_phrase(name));
        spans.extend(decrypt_spans(
            phrase,
            0,
            elapsed,
            id,
            theme,
            Some(HitTarget::ToggleToolCard(id)),
        ));
        spans.push(LinkedSpan {
            span: Span::styled(format!("  {elapsed}", elapsed = meta_parts[0]), Style::default().fg(theme.dim)),
            target: Some(HitTarget::ToggleToolCard(id)),
        });
    } else if let Some((done_verb, run_verb, raw_path)) = file_verb {
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
        let meta = format!("  {}", meta_parts.join(" - "));
        let hide_len = if collapsed { 0 } else { 8 };
        let budget = width
            .saturating_sub(
                display_width(verb)
                    + display_width(&diff_badge)
                    + display_width(&meta)
                    + hide_len
                    + 6,
            )
            .max(10);

        if status == CardStatus::Running {
            spans.extend(decrypt_spans(
                verb,
                0,
                elapsed,
                id,
                theme,
                Some(HitTarget::ToggleToolCard(id)),
            ));
            spans.push(LinkedSpan {
                span: Span::raw(" "),
                target: Some(HitTarget::ToggleToolCard(id)),
            });
        } else {
            spans.push(LinkedSpan {
                span: Span::styled(
                    format!("{verb} "),
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ),
                target: Some(HitTarget::ToggleToolCard(id)),
            });
        }
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
        if !collapsed {
            spans.push(LinkedSpan {
                span: Span::raw("  "),
                target: None,
            });
            spans.push(LinkedSpan {
                span: Span::styled(
                    "[hide]",
                    Style::default()
                        .fg(theme.dim)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                target: Some(HitTarget::ToggleToolCard(id)),
            });
        }
    } else {
        let live_tail = if status == CardStatus::Running {
            output
                .iter()
                .rev()
                .map(|l| l.text.trim())
                .find(|l| !l.is_empty())
                .map(|l| format!(" - {l}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let has_out = !output.is_empty() || matches!(name, "exec" | "shell" | "job" | "delegate" | "monitor");
        let out_pill_w = if has_out { 7 } else { 0 };
        let info_pill_w = if !collapsed { 15 } else { 0 };
        let meta = format!("  {}", meta_parts.join(" - "));
        let run_phrase = (status == CardStatus::Running).then(|| running_tool_phrase(name));
        let phrase_w = run_phrase.map(|p| display_width(p) + 2).unwrap_or(0);
        let reserved = 4 + phrase_w + display_width(&meta) + out_pill_w + info_pill_w;
        let title_budget = width.saturating_sub(reserved).max(10);
        let raw_title = if run_phrase.is_some() {
            title.strip_prefix("$ ").unwrap_or(title)
        } else {
            title
        };
        let main_label = if !live_tail.is_empty() {
            format!("{raw_title}{live_tail}")
        } else if !subtitle.is_empty() && title_budget > 24 {
            format!("{raw_title}  {subtitle}")
        } else {
            raw_title.to_string()
        };
        if let Some(phrase) = run_phrase {
            spans.extend(decrypt_spans(
                phrase,
                0,
                elapsed,
                id,
                theme,
                Some(HitTarget::ToggleToolCard(id)),
            ));
            spans.push(LinkedSpan {
                span: Span::raw("  "),
                target: Some(HitTarget::ToggleToolCard(id)),
            });
        }
        spans.push(LinkedSpan {
            span: Span::styled(
                truncate_width(&main_label, title_budget),
                Style::default().fg(theme.dim),
            ),
            target: Some(HitTarget::ToggleToolCard(id)),
        });
        spans.push(LinkedSpan {
            span: Span::styled(meta, Style::default().fg(theme.dim)),
            target: Some(HitTarget::ToggleToolCard(id)),
        });
        if has_out {
            spans.push(LinkedSpan {
                span: Span::raw("  "),
                target: None,
            });
            spans.push(LinkedSpan {
                span: Span::styled(
                    "[out]",
                    Style::default()
                        .fg(theme.link)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                target: Some(HitTarget::OpenToolOutput(id)),
            });
        }
        if !collapsed {
            spans.push(LinkedSpan {
                span: Span::raw("  "),
                target: None,
            });
            spans.push(LinkedSpan {
                span: Span::styled(
                    "[info]",
                    Style::default()
                        .fg(theme.dim)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                target: Some(HitTarget::OpenToolInspect(id)),
            });
            spans.push(LinkedSpan {
                span: Span::raw("  "),
                target: None,
            });
            spans.push(LinkedSpan {
                span: Span::styled(
                    "[hide]",
                    Style::default()
                        .fg(theme.dim)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                target: Some(HitTarget::ToggleToolCard(id)),
            });
        }
    }

    let mut ll = LinkedLine::from_linked(spans);
    if is_selected {
        ll.line_style = Some(Style::default().bg(theme.selection));
    }
    ll
}
