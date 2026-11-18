//! Compact tool headers, collapsible activity groups, and empty-session welcome pane.
use crate::tui::links::{normalize_display_path, LinkedSpan};
use crate::tui::*;
use std::path::Path;

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(super) fn spinner() -> &'static str {
    SPIN[((now_ms() / 90) as usize) % SPIN.len()]
}

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
        parts.push(format!("{} more", other));
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
    let mut spans = vec![
        LinkedSpan {
            span: Span::styled(format!("  {glyph} "), Style::default().fg(theme.dim)),
            target: Some(HitTarget::ToggleToolGroup(ids.clone())),
        },
        LinkedSpan {
            span: Span::styled(
                truncate_width(&sentence, width.saturating_sub(16)),
                Style::default().fg(theme.dim),
            ),
            target: Some(HitTarget::ToggleToolGroup(ids.clone())),
        },
    ];
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
    let mut any_err = false;
    let mut latest_title = "";
    let mut min_start = u64::MAX;
    let mut total_dur = 0u64;

    for item in group {
        let TranscriptItem::Tool {
            id,
            name,
            title,
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
        if *status == CardStatus::Error {
            any_err = true;
        }
        latest_title = title.as_str();
        min_start = min_start.min(*started_ms);
        total_dur += duration_ms.unwrap_or_else(|| now_ms().saturating_sub(*started_ms));
    }

    let summary = activity_sentence(group, &[]);
    let elapsed = if min_start == u64::MAX {
        fmt_duration(0)
    } else if any_running {
        fmt_duration(now_ms().saturating_sub(min_start))
    } else {
        fmt_duration(total_dur)
    };

    let (glyph, glyph_fg) = if any_running {
        (spinner(), theme.accent)
    } else if any_err {
        ("✗", theme.bad)
    } else {
        (">", theme.dim)
    };

    let text = if any_running {
        format!(
            "Working - {}  {elapsed}",
            truncate_width(latest_title, width.saturating_sub(18).max(8))
        )
    } else {
        format!("{summary}  {elapsed}")
    };

    let mut line = LinkedLine::from_linked(vec![
        LinkedSpan {
            span: Span::styled(format!("  {glyph} "), Style::default().fg(glyph_fg)),
            target: Some(HitTarget::ToggleToolGroup(ids.clone())),
        },
        LinkedSpan {
            span: Span::styled(
                truncate_width(&text, width.saturating_sub(5)),
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
    let (glyph, glyph_style) = match status {
        CardStatus::Queued => ("o", Style::default().fg(theme.warn)),
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

    let file_verb = match name {
        "edit" => title.strip_prefix("edit ").map(|p| ("Edited", "Editing", p)),
        "write" => title.strip_prefix("write ").map(|p| ("Wrote", "Writing", p)),
        "read" => title.strip_prefix("read ").map(|p| ("Read", "Reading", p)),
        _ => None,
    };

    let mut spans = vec![LinkedSpan {
        span: Span::styled(format!("  {glyph} "), glyph_style),
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
        let reserved = 4 + display_width(&meta) + out_pill_w + info_pill_w;
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

pub(super) fn draw_welcome(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let w = area.width as usize;
    let mut rows = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  Envoy  ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_width(&app.cwd_label(), w.saturating_sub(12)),
                Style::default().fg(theme.text),
            ),
        ]),
    ];

    if let Some((prov, model)) = resolve_active_provider(&app.data_dir) {
        let budget =
            crate::agent::context::resolve_budget(&app.data_dir, &prov.id, &model.model_id);
        rows.push(Line::from(vec![
            Span::styled("  model  ", Style::default().fg(theme.dim)),
            Span::styled(
                format!(
                    "{}  ({} - compact {})",
                    model.model_id,
                    budget.tier,
                    fmt_tokens(budget.compact_at)
                ),
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
                "{} mcp - {} skills - perm:{}",
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
            "  Click any underlined file link to review in the right Inspector - Drag divider to resize",
            w,
        ),
        Style::default().fg(theme.dim),
    ));
    rows.push(Line::styled(
        truncate_width(
            "  Shift+Tab permissions - Ctrl+B panel - Ctrl+O expand - / commands",
            w,
        ),
        Style::default().fg(theme.dim),
    ));

    frame.render_widget(Paragraph::new(rows), area);
}
