//! Compact tool headers, collapsible activity groups, and empty-session welcome pane.
use crate::tui::links::{normalize_display_path, LinkedSpan};
use crate::tui::*;
use std::path::Path;

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(super) fn spinner() -> &'static str {
    SPIN[((now_ms() / 90) as usize) % SPIN.len()]
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
    let mut counts: Vec<(&str, usize)> = Vec::new();
    let mut latest_title = "";
    let mut min_start = u64::MAX;
    let mut total_dur = 0u64;
    let mut real_tools = 0usize;

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
        real_tools += 1;
        if *status == CardStatus::Running {
            any_running = true;
        }
        if *status == CardStatus::Error {
            any_err = true;
        }
        latest_title = title.as_str();
        min_start = min_start.min(*started_ms);
        total_dur += duration_ms.unwrap_or_else(|| now_ms().saturating_sub(*started_ms));
        if let Some((_, c)) = counts.iter_mut().find(|(n, _)| *n == name.as_str()) {
            *c += 1;
        } else {
            counts.push((name.as_str(), 1));
        }
    }

    let summary: String = counts
        .iter()
        .map(|(n, c)| format!("{n} x{c}"))
        .collect::<Vec<_>>()
        .join(", ");
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
            "Exploring - {}  ({summary})  {elapsed}",
            truncate_width(
                latest_title,
                width.saturating_sub(summary.len() + 24).max(8)
            )
        )
    } else {
        format!("Explored {real_tools} ops ({summary})  {elapsed}")
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
    let mut counts: Vec<(&str, usize)> = Vec::new();
    let mut real_tools = 0usize;
    for item in group {
        if let TranscriptItem::Tool { id, name, .. } = item {
            ids.push(*id);
            if name == "say" {
                continue;
            }
            real_tools += 1;
            if let Some((_, c)) = counts.iter_mut().find(|(n, _)| *n == name.as_str()) {
                *c += 1;
            } else {
                counts.push((name.as_str(), 1));
            }
        }
    }
    let summary: String = counts
        .iter()
        .map(|(n, c)| format!("{n} x{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let label = format!("  v Explored {real_tools} ops ({summary})  ");
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
