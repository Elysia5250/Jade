//! Markdown, inline-formatted table, blockquote, list, and code-block rendering.
use super::highlight::{highlight_code_line, Lang};
use super::links::{inline_spans_linked, LinkedSpan, WorkspaceIndex};
pub(crate) use super::markdown_table::is_table_start;
use super::markdown_table::render_table_linked;
#[cfg(test)]
pub(crate) use super::markdown_table::split_table_row;
use super::theme::Theme;
use super::types::LinkedLine;
use super::{char_width, display_width, truncate_width, wrap_text};
use envoy_agent::session::FileChange;
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
    render_markdown_linked_with_code_bg(text, width, theme, cwd, changed, index, theme.panel)
}

pub(crate) fn render_markdown_linked_with_code_bg(
    text: &str,
    width: usize,
    theme: &Theme,
    cwd: &Path,
    changed: &[FileChange],
    index: &WorkspaceIndex,
    code_bg_color: ratatui::style::Color,
) -> Vec<LinkedLine> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut in_code = false;
    let mut code_lang = Lang::Plain;
    let mut in_list = false;
    let mut in_quote = false;
    let mut prev_list_wrapped = false;
    let code_bg = Style::default().bg(code_bg_color);
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
            let code_spans =
                highlight_code_line(&shown, code_lang, theme, Style::default().fg(theme.text));
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
        if let Some((level, heading)) = heading_level(trimmed) {
            in_list = false;
            in_quote = false;
            prev_list_wrapped = false;
            push_md_blank_if_needed(&mut out);
            let h_style = Style::default()
                .fg(if level >= 3 {
                    theme.secondary
                } else {
                    theme.accent
                })
                .add_modifier(Modifier::BOLD);
            for w in wrap_text(heading, width.saturating_sub(2)) {
                out.push(LinkedLine::from_line(Line::styled(
                    format!(" {w}"),
                    h_style,
                )));
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

fn heading_level(trimmed: &str) -> Option<(usize, &str)> {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    let title = rest.trim();
    (!title.is_empty()).then_some((hashes, title))
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
                                                last_ls.span =
                                                    Span::styled(owned, last_ls.span.style);
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
