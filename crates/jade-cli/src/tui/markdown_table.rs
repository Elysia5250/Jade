//! Markdown pipe-table parsing, column sizing, and inline-linked row rendering.

use super::links::{inline_spans_linked, LinkedSpan, WorkspaceIndex};
use super::theme::Theme;
use super::types::LinkedLine;
use super::{char_width, display_width};
use jade_agent::session::FileChange;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::path::Path;

pub(crate) fn is_table_start(lines: &[&str], i: usize) -> bool {
    if i + 1 >= lines.len() {
        return false;
    }
    let head = lines[i].trim();
    let delim = lines[i + 1].trim();
    if !head.starts_with('|') || !delim.starts_with('|') {
        return false;
    }
    let cells = split_table_row(delim);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let core = c.trim_matches(':');
            !core.is_empty() && core.chars().all(|ch| ch == '-')
        })
}

pub(crate) fn split_table_row(line: &str) -> Vec<String> {
    let mut trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix('|') {
        trimmed = rest;
    }
    if trimmed.ends_with('|') && !trimmed.ends_with("\\|") {
        trimmed = &trimmed[..trimmed.len() - 1];
    }
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut in_code = false;
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '`' => {
                in_code = !in_code;
                cur.push(ch);
            }
            '\\' => {
                if let Some(&next) = chars.peek() {
                    if next == '|' {
                        cur.push('|');
                        chars.next();
                        continue;
                    }
                }
                cur.push(ch);
            }
            '|' if !in_code => {
                cells.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

#[derive(Clone, Copy)]
pub(crate) enum Align {
    Left,
    Right,
    Center,
}

fn spans_display_width(spans: &[LinkedSpan]) -> usize {
    spans.iter().map(|s| display_width(&s.span.content)).sum()
}

fn truncate_linked_spans(spans: &[LinkedSpan], max: usize) -> (Vec<LinkedSpan>, usize) {
    let total = spans_display_width(spans);
    if total <= max {
        return (spans.to_vec(), total);
    }
    let limit = max.saturating_sub(1);
    let mut out = Vec::new();
    let mut used = 0usize;
    'outer: for item in spans {
        let mut piece = String::new();
        for ch in item.span.content.chars() {
            let cw = char_width(ch);
            if used + cw > limit {
                if !piece.is_empty() {
                    out.push(LinkedSpan {
                        span: Span::styled(piece, item.span.style),
                        target: item.target.clone(),
                    });
                }
                break 'outer;
            }
            piece.push(ch);
            used += cw;
        }
        if !piece.is_empty() {
            out.push(LinkedSpan {
                span: Span::styled(piece, item.span.style),
                target: item.target.clone(),
            });
        }
    }
    if max >= 1 {
        out.push(LinkedSpan {
            span: Span::raw("."),
            target: None,
        });
        used += 1;
    }
    (out, used)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_table_linked(
    lines: &[&str],
    width: usize,
    theme: &Theme,
    cwd: &Path,
    changed: &[FileChange],
    index: &WorkspaceIndex,
    out: &mut Vec<LinkedLine>,
) -> usize {
    let header_raw = split_table_row(lines[0]);
    let delim = split_table_row(lines[1]);
    let mut rows_raw: Vec<Vec<String>> = Vec::new();
    let mut consumed = 2usize;
    while consumed < lines.len() && lines[consumed].trim().starts_with('|') {
        rows_raw.push(split_table_row(lines[consumed]));
        consumed += 1;
    }
    let cols = header_raw
        .len()
        .max(delim.len())
        .max(rows_raw.iter().map(Vec::len).max().unwrap_or(0));
    if cols == 0 {
        return consumed;
    }

    let aligns: Vec<Align> = (0..cols)
        .map(|c| {
            let d = delim.get(c).map(String::as_str).unwrap_or("---");
            match (d.starts_with(':'), d.ends_with(':')) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            }
        })
        .collect();

    let parse_row = |raw: &[String]| -> Vec<Vec<LinkedSpan>> {
        (0..cols)
            .map(|c| {
                let s = raw.get(c).map(String::as_str).unwrap_or("");
                inline_spans_linked(s, theme, cwd, changed, index)
            })
            .collect()
    };

    let header = parse_row(&header_raw);
    let rows: Vec<Vec<Vec<LinkedSpan>>> = rows_raw.iter().map(|r| parse_row(r)).collect();

    let mut widths: Vec<usize> = (0..cols)
        .map(|c| {
            let mut w = spans_display_width(&header[c]);
            for row in &rows {
                w = w.max(spans_display_width(&row[c]));
            }
            w.max(3)
        })
        .collect();

    let budget = width.saturating_sub(cols * 3 + 2).max(cols * 3);
    while widths.iter().sum::<usize>() > budget {
        let Some((idx, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
            break;
        };
        if widths[idx] <= 3 {
            break;
        }
        widths[idx] -= 1;
    }

    let border = Style::default().fg(theme.border);
    push_table_row_linked(out, &header, &widths, &aligns, theme, true);
    let div_parts: Vec<String> = widths.iter().map(|w| "─".repeat(*w + 2)).collect();
    out.push(LinkedLine::from_line(Line::styled(
        format!(" {}", div_parts.join("┼")),
        border,
    )));
    for row in &rows {
        push_table_row_linked(out, row, &widths, &aligns, theme, false);
    }
    consumed
}

fn push_table_row_linked(
    out: &mut Vec<LinkedLine>,
    cells: &[Vec<LinkedSpan>],
    widths: &[usize],
    aligns: &[Align],
    theme: &Theme,
    header: bool,
) {
    let border = Style::default().fg(theme.border);
    let base_style = if header {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let mut spans = vec![LinkedSpan {
        span: Span::raw("  "),
        target: None,
    }];
    for (c, width) in widths.iter().enumerate() {
        let cell_spans = cells.get(c).map(Vec::as_slice).unwrap_or(&[]);
        let (shown, used) = truncate_linked_spans(cell_spans, *width);
        let pad = width.saturating_sub(used);
        let (before, after) = match aligns.get(c).copied().unwrap_or(Align::Left) {
            Align::Left => (0, pad),
            Align::Right => (pad, 0),
            Align::Center => (pad / 2, pad - pad / 2),
        };
        if before > 0 {
            spans.push(LinkedSpan {
                span: Span::raw(" ".repeat(before)),
                target: None,
            });
        }
        for mut ls in shown {
            if ls.span.style.fg.is_none() {
                ls.span.style = base_style.patch(ls.span.style);
            } else if header {
                ls.span.style = ls.span.style.add_modifier(Modifier::BOLD);
            }
            spans.push(ls);
        }
        if after > 0 {
            spans.push(LinkedSpan {
                span: Span::raw(" ".repeat(after)),
                target: None,
            });
        }
        if c + 1 < widths.len() {
            spans.push(LinkedSpan {
                span: Span::styled(" │ ", border),
                target: None,
            });
        }
    }
    out.push(LinkedLine::from_linked(spans));
}
