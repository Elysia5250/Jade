//! Width, wrapping, markdown, and path labels for the console.
use super::theme::Theme;
use envoy_proto::{Job, JobState};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) fn collapse_home(path: &Path) -> String {
    let home = std::env::var("HOME").ok();
    let text = path.display().to_string();
    match home {
        Some(home) if text == home => "~".to_string(),
        Some(home) if text.starts_with(&format!("{home}/")) => {
            format!("~{}", &text[home.len()..])
        }
        _ => text,
    }
}

pub(crate) fn resolve_cd_path(cwd: &Path, raw: &str) -> PathBuf {
    if raw == "~" {
        return PathBuf::from(std::env::var("HOME").unwrap_or_default());
    }
    if let Some(tail) = raw.strip_prefix("~/") {
        return PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(tail);
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

/// Directory autocomplete entries for `/cd <arg>`: returns `(display, completion_arg)`.
pub(crate) fn complete_directories(cwd: &Path, raw: &str) -> Vec<(String, String)> {
    let expanded_prefix = if raw == "~" || raw.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        if raw == "~" {
            format!("{home}/")
        } else {
            format!("{home}/{}", &raw[2..])
        }
    } else {
        raw.to_string()
    };

    let (dir_to_read, name_prefix, arg_dir_prefix) = if raw.is_empty() {
        (cwd.to_path_buf(), String::new(), String::new())
    } else if raw.ends_with('/') {
        let dir = resolve_cd_path(cwd, raw);
        (dir, String::new(), raw.to_string())
    } else if let Some(idx) = raw.rfind('/') {
        let parent_raw = &raw[..=idx];
        let dir = resolve_cd_path(cwd, parent_raw);
        (dir, raw[idx + 1..].to_string(), parent_raw.to_string())
    } else {
        let _ = expanded_prefix;
        (cwd.to_path_buf(), raw.to_string(), String::new())
    };

    let Ok(entries) = std::fs::read_dir(&dir_to_read) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let is_dir = ft.is_dir() || (ft.is_symlink() && entry.path().is_dir());
        if !is_dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && !name_prefix.starts_with('.') {
            continue;
        }
        if !name_prefix.is_empty() && !name.starts_with(&name_prefix) {
            continue;
        }
        let completed = format!("{arg_dir_prefix}{name}/");
        dirs.push((format!("{name}/"), completed));
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    dirs.truncate(10);
    dirs
}

// ------------------------------------------------------------------ drawing


pub(crate) fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

pub(crate) fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(1).max(1)
}

pub(crate) fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub(crate) fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let max = max_width.max(8);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in text.chars() {
        let w = char_width(ch);
        if cur_w + w > max && !cur.is_empty() {
            // Prefer a word boundary so `exit=0` never becomes `exit=\n0`.
            if let Some(pos) = cur.rfind(' ') {
                let tail: String = cur[pos + 1..].to_string();
                let head = cur[..pos].trim_end().to_string();
                if !head.is_empty() {
                    out.push(head);
                }
                cur = tail.trim_start().to_string();
                cur_w = cur.chars().map(char_width).sum();
            } else {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
        }
        if ch == ' ' && cur.is_empty() {
            continue;
        }
        cur.push(ch);
        cur_w += w;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

pub(crate) fn layout_editor_text(input: &str, cursor_chars: usize, max_width: usize) -> (Vec<String>, usize, usize) {
    let max_width = max_width.max(8);
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    let mut c_row = 0usize;
    let mut c_col = 0usize;

    for (idx, ch) in input.chars().enumerate() {
        if idx == cursor_chars {
            c_row = lines.len();
            c_col = cur_w;
        }
        if ch == '\n' {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
            continue;
        }
        let w = char_width(ch);
        if cur_w + w > max_width && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
            if idx == cursor_chars {
                c_row = lines.len();
                c_col = 0;
            }
        }
        cur.push(ch);
        cur_w += w;
    }
    if cursor_chars >= input.chars().count() {
        c_row = lines.len();
        c_col = cur_w;
    }
    lines.push(cur);
    (lines, c_row, c_col)
}

pub(crate) fn render_markdown_lines<'a>(
    text: &str,
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'a>>,
) {
    let lines: Vec<&str> = text.lines().collect();
    let mut in_code = false;
    let mut i = 0usize;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim_start();

        if let Some(fence) = trimmed.strip_prefix("```") {
            in_code = !in_code;
            if in_code {
                let lang = fence.trim();
                out.push(Line::styled(
                    if lang.is_empty() {
                        " ┌──".to_string()
                    } else {
                        format!(" ┌─ {lang}")
                    },
                    Style::default().fg(theme.border),
                ));
            } else {
                out.push(Line::styled(" └──", Style::default().fg(theme.border)));
            }
            i += 1;
            continue;
        }
        if in_code {
            let shown = truncate_width(raw, width.saturating_sub(4));
            out.push(Line::from(vec![
                Span::styled(" │ ", Style::default().fg(theme.border)),
                Span::styled(shown, Style::default().fg(theme.secondary)),
            ]));
            i += 1;
            continue;
        }
        if is_table_start(&lines, i) {
            i += render_table(&lines[i..], width, theme, out);
            continue;
        }
        if trimmed.starts_with("### ") || trimmed.starts_with("## ") || trimmed.starts_with("# ") {
            let heading = trimmed.trim_start_matches('#').trim();
            for w in wrap_text(heading, width.saturating_sub(2)) {
                out.push(Line::styled(
                    format!(" {w}"),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            i += 1;
            continue;
        }
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let item = trimmed.trim_start_matches(['-', '*']).trim_start();
            let wrapped = wrap_text(item, width.saturating_sub(4));
            for (n, w) in wrapped.iter().enumerate() {
                let prefix = if n == 0 { " • " } else { "   " };
                let mut spans = vec![Span::styled(prefix, Style::default().fg(theme.accent))];
                spans.extend(inline_spans(w, theme));
                out.push(Line::from(spans));
            }
            i += 1;
            continue;
        }
        for w in wrap_text(raw, width.saturating_sub(2)) {
            out.push(Line::from(inline_spans(&format!(" {w}"), theme)));
        }
        i += 1;
    }
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

pub(crate) fn truncate_width(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let w = char_width(ch);
        if width + w > max {
            out.push('…');
            return out;
        }
        out.push(ch);
        width += w;
    }
    out
}

#[derive(Clone, Copy)]
pub(crate) enum Align {
    Left,
    Right,
    Center,
}

pub(crate) fn render_table<'a>(
    lines: &[&str],
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'a>>,
) -> usize {
    let header = split_table_row(lines[0]);
    let delim = split_table_row(lines[1]);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut consumed = 2usize;
    while consumed < lines.len() && lines[consumed].trim().starts_with('|') {
        rows.push(split_table_row(lines[consumed]));
        consumed += 1;
    }
    let cols = header
        .len()
        .max(delim.len())
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
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

    let mut widths: Vec<usize> = (0..cols)
        .map(|c| {
            let mut w: usize = header
                .get(c)
                .map(String::as_str)
                .unwrap_or("")
                .chars()
                .map(char_width)
                .sum();
            for row in &rows {
                if let Some(cell) = row.get(c) {
                    w = w.max(cell.chars().map(char_width).sum());
                }
            }
            w.max(3)
        })
        .collect();

    let budget = width.saturating_sub(cols + 3).max(cols * 3);
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
    let mut top = String::from(" ┌");
    for (c, w) in widths.iter().enumerate() {
        top.push_str(&"─".repeat(*w + 2));
        top.push(if c + 1 == cols { '┐' } else { '┬' });
    }
    out.push(Line::styled(top, border));
    push_table_row(out, &header, &widths, &aligns, theme, true);
    let mut mid = String::from(" ├");
    for (c, w) in widths.iter().enumerate() {
        mid.push_str(&"─".repeat(*w + 2));
        mid.push(if c + 1 == cols { '┤' } else { '┼' });
    }
    out.push(Line::styled(mid, border));
    for row in &rows {
        push_table_row(out, row, &widths, &aligns, theme, false);
    }
    let mut bottom = String::from(" └");
    for (c, w) in widths.iter().enumerate() {
        bottom.push_str(&"─".repeat(*w + 2));
        bottom.push(if c + 1 == cols { '┘' } else { '┴' });
    }
    out.push(Line::styled(bottom, border));
    consumed
}

pub(crate) fn push_table_row<'a>(
    out: &mut Vec<Line<'a>>,
    cells: &[String],
    widths: &[usize],
    aligns: &[Align],
    theme: &Theme,
    header: bool,
) {
    let border = Style::default().fg(theme.border);
    let text_style = if header {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let mut spans = vec![Span::styled(" │ ", border)];
    for (c, width) in widths.iter().enumerate() {
        let raw = cells.get(c).map(String::as_str).unwrap_or("");
        let shown = truncate_width(raw, *width);
        let used: usize = shown.chars().map(char_width).sum();
        let pad = width.saturating_sub(used);
        let (before, after) = match aligns.get(c).copied().unwrap_or(Align::Left) {
            Align::Left => (0, pad),
            Align::Right => (pad, 0),
            Align::Center => (pad / 2, pad - pad / 2),
        };
        spans.push(Span::styled(
            format!("{}{}{}", " ".repeat(before), shown, " ".repeat(after)),
            text_style,
        ));
        spans.push(Span::styled(
            if c + 1 == widths.len() { " │" } else { " │ " },
            border,
        ));
    }
    out.push(Line::from(spans));
}

pub(crate) fn inline_spans<'a>(line: &str, theme: &Theme) -> Vec<Span<'a>> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut i = 0usize;
    while i < line.len() {
        let rest = &line[i..];
        let marker = if rest.starts_with('`') {
            Some(("`", Style::default().fg(theme.secondary).bg(theme.surface)))
        } else if rest.starts_with("**") {
            Some(("**", Style::default().add_modifier(Modifier::BOLD)))
        } else if rest.starts_with('*') {
            Some(("*", Style::default().add_modifier(Modifier::ITALIC)))
        } else {
            None
        };
        if let Some((marker, style)) = marker {
            let after = &rest[marker.len()..];
            if let Some(end) = after.find(marker) {
                if end > 0 {
                    if !plain.is_empty() {
                        spans.push(Span::raw(std::mem::take(&mut plain)));
                    }
                    spans.push(Span::styled(after[..end].to_string(), style));
                    i += marker.len() + end + marker.len();
                    continue;
                }
            }
        }
        let ch = rest.chars().next().expect("non-empty");
        plain.push(ch);
        i += ch.len_utf8();
    }
    if !plain.is_empty() {
        spans.push(Span::raw(plain));
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn job_elapsed(job: &Job) -> u64 {
    let now = now_ms();
    match &job.state {
        JobState::Queued { since_ms } => now.saturating_sub(*since_ms),
        JobState::Running { since_ms } => now.saturating_sub(*since_ms),
        JobState::Done { at_ms, .. } => at_ms.saturating_sub(job.created_ms),
    }
}

/// Standard API context windows. The longest matching fragment wins.
/// An unknown model uses 128k so the meter fills early instead of
/// pretending there is room that was never advertised.
pub(crate) fn context_window(model_id: &str) -> u64 {
    let id = model_id.to_ascii_lowercase();
    const TABLE: &[(&str, u64)] = &[
        ("claude", 200_000),
        ("gemini", 1_048_576),
        ("gpt-4.1", 1_047_576),
        ("gpt-4o", 128_000),
        ("gpt-4-turbo", 128_000),
        ("gpt-5", 272_000),
        ("o1", 200_000),
        ("o3", 200_000),
        ("o4", 200_000),
        ("deepseek-reasoner", 64_000),
        ("deepseek", 64_000),
        ("grok", 131_072),
        ("qwen", 131_072),
        ("llama", 128_000),
    ];
    TABLE
        .iter()
        .filter(|(fragment, _)| id.contains(fragment))
        .max_by_key(|(fragment, _)| fragment.len())
        .map(|(_, window)| *window)
        .unwrap_or(128_000)
}

/// `(tokens, estimated)`. A finished turn stores the provider's prompt
/// size; before that, the transcript is counted at about four characters
/// per token.
pub(crate) fn context_used(session: &crate::session::Session) -> (u64, bool) {
    if session.prompt_tokens > 0 {
        return (session.prompt_tokens, false);
    }
    let chars: usize = session
        .messages
        .iter()
        .map(|message| {
            message.content.chars().count()
                + message
                    .reasoning
                    .as_ref()
                    .map(|text| text.chars().count())
                    .unwrap_or(0)
        })
        .sum();
    if chars == 0 {
        (0, false)
    } else {
        ((chars / 4) as u64, true)
    }
}

pub(crate) fn context_bar(used: u64, window: u64) -> (String, String) {
    const CELLS: usize = 8;
    let ratio = if window == 0 {
        0.0
    } else {
        used as f64 / window as f64
    };
    let filled = if used == 0 {
        0
    } else {
        ((ratio * CELLS as f64).ceil() as usize).clamp(1, CELLS)
    };
    ("█".repeat(filled), "░".repeat(CELLS - filled))
}

pub(crate) fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        let scaled = format!("{:.1}", n as f64 / 1_000.0);
        let scaled = scaled.strip_suffix(".0").unwrap_or(&scaled);
        format!("{scaled}k")
    } else {
        n.to_string()
    }
}

pub(crate) fn fmt_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::{context_bar, context_window, fmt_tokens};

    #[test]
    fn window_prefers_the_longest_fragment() {
        assert_eq!(context_window("claude-sonnet-4-5"), 200_000);
        assert_eq!(context_window("gpt-4.1"), 1_047_576);
        assert_eq!(context_window("gpt-4o"), 128_000);
        assert_eq!(context_window("gpt-5.2-codex"), 272_000);
        assert_eq!(context_window("gemini-2.5-pro"), 1_048_576);
        assert_eq!(context_window("deepseek-chat"), 64_000);
        assert_eq!(context_window("some-local-model"), 128_000);
    }

    #[test]
    fn token_labels_and_bar() {
        assert_eq!(fmt_tokens(800), "800");
        assert_eq!(fmt_tokens(12_400), "12.4k");
        assert_eq!(fmt_tokens(50_000), "50k");
        assert_eq!(fmt_tokens(1_048_576), "1.0m");
        assert_eq!(context_bar(0, 200_000), (String::new(), "░".repeat(8)));
        let (filled, empty) = context_bar(50_000, 200_000);
        assert_eq!(filled.chars().count() + empty.chars().count(), 8);
        assert!(!filled.is_empty());
    }
}

