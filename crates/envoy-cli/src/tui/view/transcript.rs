//! Transcript and tool cards.
use super::push_blank;
use crate::tui::*;

pub(super) fn draw_transcript(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
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
