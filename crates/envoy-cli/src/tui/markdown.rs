//! Markdown, inline-formatted table, blockquote, list, and code-block rendering.
use super::highlight::{highlight_code_line, Lang};
use super::links::{inline_spans_linked, LinkedSpan, WorkspaceIndex};
use super::theme::Theme;
use super::types::LinkedLine;
use super::{char_width, display_width, truncate_width, wrap_text};
use crate::session::FileChange;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::path::Path;

pub(crate) fn render_markdown_linked(
    text: &str,
    width: usize,
    theme: &Theme,
    cwd: &Path,
    changed: &[FileChange],
    index: &WorkspaceIndex,
) -> Vec<LinkedLine> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut in_code = false;
    let mut code_lang = Lang::Plain;
    let mut in_list = false;
    let mut in_quote = false;
    let mut prev_list_wrapped = false;
    let code_bg = Style::default().bg(theme.panel);
    let mut i = 0usize;

    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim_start();

        if let Some(fence) = trimmed.strip_prefix("```") {
            in_list = false;
            in_quote = false;
            prev_list_wrapped = false;
            in_code = !in_code;
            let label = if in_code {
                push_md_blank_if_needed(&mut out);
                let lang = fence.trim();
                code_lang = Lang::from_hint(lang);
                if lang.is_empty() {
                    " +--".to_string()
                } else {
                    format!(" +-- {lang}")
                }
            } else {
                code_lang = Lang::Plain;
                " +--".to_string()
            };
            let mut ll = LinkedLine::from_line(Line::styled(
                truncate_width(&label, width),
                Style::default().fg(theme.dim),
            ));
            ll.line_style = Some(code_bg);
            out.push(ll);
            if !in_code {
                push_md_blank_if_needed(&mut out);
            }
            i += 1;
            continue;
        }
        if in_code {
            let gutter = " | ";
            let shown = truncate_width(raw, width.saturating_sub(display_width(gutter)).max(4));
            let mut spans = vec![LinkedSpan {
                span: Span::styled(gutter, Style::default().fg(theme.border)),
                target: None,
            }];
            let code_spans = highlight_code_line(
                &shown,
                code_lang,
                theme,
                Style::default().fg(theme.text),
            );
            for sp in code_spans {
                spans.push(LinkedSpan {
                    span: Span::styled(sp.content.into_owned(), sp.style),
                    target: None,
                });
            }
            let mut ll = LinkedLine::from_linked(spans);
            ll.line_style = Some(code_bg);
            out.push(ll);
            i += 1;
            continue;
        }
        if trimmed.is_empty() {
            in_list = false;
            in_quote = false;
            prev_list_wrapped = false;
            push_md_blank_if_needed(&mut out);
            i += 1;
            continue;
        }
        if is_table_start(&lines, i) {
            in_list = false;
            in_quote = false;
            prev_list_wrapped = false;
            push_md_blank_if_needed(&mut out);
            i += render_table_linked(&lines[i..], width, theme, cwd, changed, index, &mut out);
            push_md_blank_if_needed(&mut out);
            continue;
        }
        if is_horizontal_rule(trimmed) {
            in_list = false;
            in_quote = false;
            prev_list_wrapped = false;
            push_md_blank_if_needed(&mut out);
            let rule_w = width.saturating_sub(4).clamp(8, 48);
            out.push(LinkedLine::from_line(Line::styled(
                format!(" {}", "─".repeat(rule_w)),
                Style::default().fg(theme.border),
            )));
            push_md_blank_if_needed(&mut out);
            i += 1;
            continue;
        }
        if trimmed.starts_with("### ") || trimmed.starts_with("## ") || trimmed.starts_with("# ") {
            in_list = false;
            in_quote = false;
            prev_list_wrapped = false;
            push_md_blank_if_needed(&mut out);
            let h3 = trimmed.starts_with("### ");
            let heading = trimmed.trim_start_matches('#').trim();
            let h_style = Style::default()
                .fg(if h3 { theme.secondary } else { theme.accent })
                .add_modifier(Modifier::BOLD);
            for w in wrap_text(heading, width.saturating_sub(2)) {
                out.push(LinkedLine::from_line(Line::styled(format!(" {w}"), h_style)));
            }
            push_md_blank_if_needed(&mut out);
            i += 1;
            continue;
        }
        if let Some(quote) = trimmed
            .strip_prefix("> ")
            .or_else(|| (trimmed == ">").then_some(""))
        {
            if !in_quote {
                push_md_blank_if_needed(&mut out);
                in_quote = true;
            }
            in_list = false;
            prev_list_wrapped = false;
            let linked = inline_spans_linked(quote, theme, cwd, changed, index);
            let q_style = Style::default().fg(theme.dim);
            let wrapped = wrap_linked_spans(
                LinkedSpan {
                    span: Span::styled(" │ ", Style::default().fg(theme.border)),
                    target: None,
                },
                LinkedSpan {
                    span: Span::styled(" │ ", Style::default().fg(theme.border)),
                    target: None,
                },
                linked,
                width,
            );
            for mut row in wrapped {
                for ls in row.iter_mut().skip(1) {
                    if ls.span.style.fg.is_none() {
                        ls.span.style = ls.span.style.patch(q_style);
                    }
                }
                out.push(LinkedLine::from_linked(row));
            }
            i += 1;
            continue;
        }
        if let Some((indent, marker, body)) = list_marker(raw) {
            if !in_list || prev_list_wrapped {
                push_md_blank_if_needed(&mut out);
            }
            in_list = true;
            in_quote = false;
            let linked = inline_spans_linked(body, theme, cwd, changed, index);
            let first = format!(" {}{marker}", " ".repeat(indent));
            let cont = " ".repeat(display_width(&first));
            let wrapped = wrap_linked_spans(
                LinkedSpan {
                    span: Span::styled(first, Style::default().fg(theme.accent)),
                    target: None,
                },
                LinkedSpan {
                    span: Span::raw(cont),
                    target: None,
                },
                linked,
                width,
            );
            prev_list_wrapped = wrapped.len() > 1;
            for row in wrapped {
                out.push(LinkedLine::from_linked(row));
            }
            i += 1;
            continue;
        }
        if in_list || in_quote {
            push_md_blank_if_needed(&mut out);
        }
        in_list = false;
        in_quote = false;
        prev_list_wrapped = false;
        let linked = inline_spans_linked(raw, theme, cwd, changed, index);
        let wrapped = wrap_linked_spans(
            LinkedSpan {
                span: Span::raw(" "),
                target: None,
            },
            LinkedSpan {
                span: Span::raw(" "),
                target: None,
            },
            linked,
            width,
        );
        for row in wrapped {
            out.push(LinkedLine::from_linked(row));
        }
        i += 1;
    }
    while out
        .last()
        .is_some_and(|l| l.spans.iter().all(|s| s.span.content.trim().is_empty()))
    {
        out.pop();
    }
    out
}

fn push_md_blank_if_needed(out: &mut Vec<LinkedLine>) {
    if out.is_empty() {
        return;
    }
    let already = out
        .last()
        .map(|l| l.spans.iter().all(|s| s.span.content.trim().is_empty()))
        .unwrap_or(true);
    if !already {
        out.push(LinkedLine::from_line(Line::from("")));
    }
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    let s = trimmed.trim();
    if s.len() < 3 {
        return false;
    }
    let ch = s.chars().next().unwrap_or(' ');
    matches!(ch, '-' | '*' | '_') && s.chars().all(|c| c == ch || c == ' ')
}

/// Leading indent plus an unordered or `1.` marker. Indent is kept so nested
/// items stay to the right of their parent.
fn list_marker(raw: &str) -> Option<(usize, String, &str)> {
    let indent = raw
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 2 } else { 1 })
        .sum();
    let rest = raw.trim_start();
    if let Some(body) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "))
    {
        return Some((indent, "* ".to_string(), body));
    }
    let mut digits = 0usize;
    for c in rest.chars() {
        if c.is_ascii_digit() && digits < 3 {
            digits += 1;
        } else {
            break;
        }
    }
    if digits > 0 && rest[digits..].starts_with(". ") {
        let marker = format!("{}. ", &rest[..digits]);
        return Some((indent, marker, &rest[digits + 2..]));
    }
    None
}

fn is_no_line_start(ch: char) -> bool {
    matches!(
        ch,
        ')' | '）'
            | ']'
            | '】'
            | '}'
            | '」'
            | '』'
            | '》'
            | '〉'
            | '’'
            | '”'
            | ','
            | '，'
            | '.'
            | '。'
            | '、'
            | ';'
            | '；'
            | ':'
            | '：'
            | '!'
            | '！'
            | '?'
            | '？'
            | '%'
            | '％'
    )
}

pub(crate) fn wrap_linked_spans(
    first_prefix: LinkedSpan,
    cont_prefix: LinkedSpan,
    spans: Vec<LinkedSpan>,
    max_width: usize,
) -> Vec<Vec<LinkedSpan>> {
    let first_w = display_width(&first_prefix.span.content);
    let cont_w = display_width(&cont_prefix.span.content);
    let body_budget = max_width.saturating_sub(first_w.max(cont_w)).max(8);

    let mut rows: Vec<Vec<LinkedSpan>> = vec![vec![first_prefix]];
    let mut cur_w = 0usize;

    for token in spans {
        let style = token.span.style;
        let target = token.target;
        let text = token.span.content.into_owned();
        if text.is_empty() {
            continue;
        }
        let mut buf = String::new();
        for ch in text.chars() {
            let cw = char_width(ch);
            if cur_w + cw > body_budget && (cur_w > 0 || !buf.is_empty()) {
                let mut carry = String::new();
                if ch != ' ' {
                    if let Some(pos) = buf.rfind(' ') {
                        let tail = &buf[pos + 1..];
                        let tail_w: usize = tail.chars().map(char_width).sum();
                        if tail_w > 0
                            && tail_w <= 16
                            && tail.is_ascii()
                            && (ch.is_ascii_alphanumeric()
                                || is_no_line_start(ch)
                                || matches!(ch, '-' | '_'))
                        {
                            carry = tail.to_string();
                            buf.truncate(pos);
                        }
                    }
                    if carry.is_empty() && is_no_line_start(ch) {
                        if let Some(prev) = buf.pop() {
                            if prev == ' ' {
                                buf.push(prev);
                            } else {
                                carry.push(prev);
                            }
                        } else if let Some(prev_row) = rows.last_mut() {
                            if prev_row.len() > 1 {
                                if let Some(last_ls) = prev_row.last_mut() {
                                    if last_ls.target.is_none()
                                        && last_ls.span.content.chars().count() >= 2
                                    {
                                        let mut owned = last_ls.span.content.to_string();
                                        if let Some(prev) = owned.pop() {
                                            if prev == ' ' {
                                                owned.push(prev);
                                            } else {
                                                carry.push(prev);
                                                last_ls.span = Span::styled(owned, last_ls.span.style);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if !buf.is_empty() {
                    rows.last_mut().unwrap().push(LinkedSpan {
                        span: Span::styled(std::mem::take(&mut buf), style),
                        target: target.clone(),
                    });
                }
                rows.push(vec![cont_prefix.clone()]);
                cur_w = 0;
                if !carry.is_empty() {
                    cur_w = carry.chars().map(char_width).sum();
                    buf = carry;
                }
                if ch == ' ' {
                    continue;
                }
            }
            buf.push(ch);
            cur_w += cw;
        }
        if !buf.is_empty() {
            rows.last_mut().unwrap().push(LinkedSpan {
                span: Span::styled(buf, style),
                target,
            });
        }
    }
    rows
}

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
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
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
fn render_table_linked(
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
