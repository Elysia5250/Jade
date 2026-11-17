//! Structured Unified / Split diff rendering powered by `similar`.
use super::text::{char_width, display_width, truncate_width};
use super::theme::Theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffLayout {
    Unified,
    Split,
}

pub(crate) struct RenderedDiff {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) hunk_lines: Vec<usize>,
    pub(crate) added: usize,
    pub(crate) removed: usize,
}

pub(crate) fn render_diff(
    before: &str,
    after: &str,
    width: usize,
    layout: DiffLayout,
    theme: &Theme,
) -> RenderedDiff {
    let diff = TextDiff::from_lines(before, after);
    let mut added = 0usize;
    let mut removed = 0usize;
    for ch in diff.iter_all_changes() {
        match ch.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    let use_split = layout == DiffLayout::Split && width >= 68;
    if use_split {
        render_split(&diff, width, theme, added, removed)
    } else {
        render_unified(&diff, width, theme, added, removed)
    }
}

fn render_unified<'a>(
    diff: &'a TextDiff<'a, 'a, 'a, str>,
    width: usize,
    theme: &Theme,
    added: usize,
    removed: usize,
) -> RenderedDiff {
    let mut lines = Vec::new();
    let mut hunk_lines = Vec::new();
    let ops_groups = diff.grouped_ops(3);
    if ops_groups.is_empty() {
        lines.push(Line::styled(
            "  (no line changes)",
            Style::default().fg(theme.dim),
        ));
        return RenderedDiff {
            lines,
            hunk_lines,
            added,
            removed,
        };
    }

    for group in ops_groups {
        if let (Some(first), Some(last)) = (group.first(), group.last()) {
            hunk_lines.push(lines.len());
            let old_start = first.old_range().start + 1;
            let old_len = last.old_range().end - first.old_range().start;
            let new_start = first.new_range().start + 1;
            let new_len = last.new_range().end - first.new_range().start;
            lines.push(Line::styled(
                format!(" @@ -{old_start},{old_len} +{new_start},{new_len} @@"),
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.surface)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        for op in group {
            for change in diff.iter_inline_changes(&op) {
                let (marker, num, base_bg, fg, word_bg) = match change.tag() {
                    ChangeTag::Delete => (
                        '-',
                        change.old_index().map(|i| i + 1),
                        theme.diff_del_bg,
                        theme.diff_del_fg,
                        theme.diff_word_del,
                    ),
                    ChangeTag::Insert => (
                        '+',
                        change.new_index().map(|i| i + 1),
                        theme.diff_add_bg,
                        theme.diff_add_fg,
                        theme.diff_word_add,
                    ),
                    ChangeTag::Equal => (
                        ' ',
                        change.new_index().map(|i| i + 1),
                        theme.panel,
                        theme.text,
                        theme.panel,
                    ),
                };
                let num_str = num
                    .map(|n| format!("{n:>4} "))
                    .unwrap_or_else(|| "     ".to_string());
                let mut spans = vec![
                    Span::styled(num_str, Style::default().fg(theme.dim).bg(base_bg)),
                    Span::styled(
                        format!("{marker} "),
                        Style::default().fg(fg).bg(base_bg).add_modifier(Modifier::BOLD),
                    ),
                ];
                let code_budget = width.saturating_sub(8).max(8);
                let mut used = 0usize;
                for (emphasized, value) in change.iter_strings_lossy() {
                    let clean = value.trim_end_matches(&['\n', '\r'][..]);
                    if clean.is_empty() || used >= code_budget {
                        continue;
                    }
                    let slice = take_fit(clean, code_budget - used);
                    used += display_width(&slice);
                    let st = if emphasized && change.tag() != ChangeTag::Equal {
                        Style::default()
                            .fg(fg)
                            .bg(word_bg)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(if change.tag() == ChangeTag::Equal {
                            theme.text
                        } else {
                            fg
                        }).bg(base_bg)
                    };
                    spans.push(Span::styled(slice, st));
                }
                if used < code_budget {
                    spans.push(Span::styled(
                        " ".repeat(code_budget - used),
                        Style::default().bg(base_bg),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
    }

    RenderedDiff {
        lines,
        hunk_lines,
        added,
        removed,
    }
}

fn render_split<'a>(
    diff: &TextDiff<'a, 'a, 'a, str>,
    width: usize,
    theme: &Theme,
    added: usize,
    removed: usize,
) -> RenderedDiff {
    let mut lines = Vec::new();
    let mut hunk_lines = Vec::new();
    let ops_groups = diff.grouped_ops(3);
    if ops_groups.is_empty() {
        lines.push(Line::styled(
            "  (no line changes)",
            Style::default().fg(theme.dim),
        ));
        return RenderedDiff {
            lines,
            hunk_lines,
            added,
            removed,
        };
    }

    let side_w = width.saturating_sub(3) / 2;
    let code_w = side_w.saturating_sub(6).max(6);

    for group in ops_groups {
        if let (Some(first), Some(_last)) = (group.first(), group.last()) {
            hunk_lines.push(lines.len());
            let old_start = first.old_range().start + 1;
            let new_start = first.new_range().start + 1;
            lines.push(Line::styled(
                format!(" @@ old:{old_start} | new:{new_start} @@"),
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.surface)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        let mut left_rows: Vec<(Option<usize>, String, bool)> = Vec::new();
        let mut right_rows: Vec<(Option<usize>, String, bool)> = Vec::new();

        for op in group {
            for change in diff.iter_changes(&op) {
                let text = change.value().trim_end_matches(&['\n', '\r'][..]).to_string();
                match change.tag() {
                    ChangeTag::Equal => {
                        flush_split_pair(&mut left_rows, &mut right_rows, code_w, theme, &mut lines);
                        let old_i = change.old_index().map(|i| i + 1);
                        let new_i = change.new_index().map(|i| i + 1);
                        lines.push(format_split_row(
                            old_i.map(|n| (n, text.as_str(), theme.panel, theme.text)),
                            new_i.map(|n| (n, text.as_str(), theme.panel, theme.text)),
                            code_w,
                            theme,
                        ));
                    }
                    ChangeTag::Delete => {
                        left_rows.push((change.old_index().map(|i| i + 1), text, true));
                    }
                    ChangeTag::Insert => {
                        right_rows.push((change.new_index().map(|i| i + 1), text, true));
                    }
                }
            }
        }
        flush_split_pair(&mut left_rows, &mut right_rows, code_w, theme, &mut lines);
    }

    RenderedDiff {
        lines,
        hunk_lines,
        added,
        removed,
    }
}

fn flush_split_pair(
    left: &mut Vec<(Option<usize>, String, bool)>,
    right: &mut Vec<(Option<usize>, String, bool)>,
    code_w: usize,
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
) {
    let rows = left.len().max(right.len());
    for i in 0..rows {
        let l = left.get(i).and_then(|(n, s, _)| {
            n.map(|num| (num, s.as_str(), theme.diff_del_bg, theme.diff_del_fg))
        });
        let r = right.get(i).and_then(|(n, s, _)| {
            n.map(|num| (num, s.as_str(), theme.diff_add_bg, theme.diff_add_fg))
        });
        out.push(format_split_row(l, r, code_w, theme));
    }
    left.clear();
    right.clear();
}

fn format_split_row(
    left: Option<(usize, &str, ratatui::style::Color, ratatui::style::Color)>,
    right: Option<(usize, &str, ratatui::style::Color, ratatui::style::Color)>,
    code_w: usize,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = Vec::new();
    push_split_half(&mut spans, left, code_w, theme);
    spans.push(Span::styled(" │ ", Style::default().fg(theme.border)));
    push_split_half(&mut spans, right, code_w, theme);
    Line::from(spans)
}

fn push_split_half(
    spans: &mut Vec<Span<'static>>,
    cell: Option<(usize, &str, ratatui::style::Color, ratatui::style::Color)>,
    code_w: usize,
    theme: &Theme,
) {
    if let Some((num, text, bg, fg)) = cell {
        let clipped = truncate_width(text, code_w);
        let pad = code_w.saturating_sub(display_width(&clipped));
        spans.push(Span::styled(
            format!("{num:>4} "),
            Style::default().fg(theme.dim).bg(bg),
        ));
        spans.push(Span::styled(
            format!("{clipped}{}", " ".repeat(pad)),
            Style::default().fg(fg).bg(bg),
        ));
    } else {
        spans.push(Span::styled(
            " ".repeat(code_w + 5),
            Style::default().bg(theme.surface),
        ));
    }
}

fn take_fit(text: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = char_width(ch);
        if w + cw > max_width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

/// Fallback: inspect `git show HEAD:<rel>` vs on-disk file when a file wasn't edited
/// directly through the `edit`/`write` tools in this session.
pub(crate) fn load_git_pair(cwd: &Path, rel_path: &str) -> Option<(String, String)> {
    let abs = if Path::new(rel_path).is_absolute() {
        Path::new(rel_path).to_path_buf()
    } else {
        cwd.join(rel_path)
    };
    let after = std::fs::read_to_string(&abs).ok()?;
    let rel = abs
        .strip_prefix(cwd)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| rel_path.to_string());
    let head = Command::new("git")
        .args(["show", &format!("HEAD:{rel}")])
        .current_dir(cwd)
        .output()
        .ok();
    match head {
        Some(out) if out.status.success() => {
            let before = String::from_utf8_lossy(&out.stdout).into_owned();
            if before == after {
                None
            } else {
                Some((before, after))
            }
        }
        _ => None,
    }
}
