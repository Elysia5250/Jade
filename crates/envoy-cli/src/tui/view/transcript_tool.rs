//! One tool card header: verb, path, duration, and the running decrypt line.

use crate::tui::links::{normalize_display_path, LinkedSpan};
use crate::tui::*;
use std::path::Path;

#[allow(clippy::too_many_arguments)]
pub(super) fn render_tool_header(
    app: &App,
    id: u64,
    name: &str,
    title: &str,
    subtitle: &str,
    target: &str,
    output: &[envoy_agent::session::OutputLine],
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
        "edit" => title
            .strip_prefix("edit ")
            .map(|p| ("Edited", "Editing", p)),
        "write" => title
            .strip_prefix("write ")
            .map(|p| ("Wrote", "Writing", p)),
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
            span: Span::styled(
                format!("  {elapsed}", elapsed = meta_parts[0]),
                Style::default().fg(theme.dim),
            ),
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
        let has_out =
            !output.is_empty() || matches!(name, "exec" | "shell" | "job" | "delegate" | "monitor");
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
