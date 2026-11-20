//! Width, wrapping, completion, and formatting helpers for the console.
use envoy_proto::{Job, JobState};
use ratatui::layout::Rect;
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
    } else if raw == "~" {
        (PathBuf::from(expanded_prefix), String::new(), "~/".to_string())
    } else {
        (cwd.to_path_buf(), raw.to_string(), String::new())
    };

    let Ok(entries) = std::fs::read_dir(&dir_to_read) else {
        return Vec::new();
    };
    let q = name_prefix.to_lowercase();
    let q_chars = q.chars().count();
    let mut dirs: Vec<(u8, String, String, String)> = Vec::new();
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
        let lower = name.to_lowercase();
        let rank = if q.is_empty() || name.starts_with(&name_prefix) {
            0
        } else if lower.starts_with(&q) {
            1
        } else if lower.contains(&q) {
            2
        } else if q_chars >= 2 && crate::tui::links::is_subsequence(&q, &lower) {
            3
        } else {
            continue;
        };
        let completed = format!("{arg_dir_prefix}{name}/");
        dirs.push((rank, lower, format!("{name}/"), completed));
    }
    dirs.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    dirs.into_iter()
        .take(10)
        .map(|(_, _, display, completed)| (display, completed))
        .collect()
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
        out.push('.');
        out
    }
}

pub(crate) fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

pub(crate) fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Pad with spaces so a highlighted row is exactly `width` columns.
pub(crate) fn pad_to_width(text: &str, width: usize) -> String {
    let used = display_width(text);
    if used >= width {
        truncate_width(text, width)
    } else {
        format!("{text}{}", " ".repeat(width - used))
    }
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

pub(crate) fn layout_editor_text(
    input: &str,
    cursor_chars: usize,
    max_width: usize,
) -> (Vec<String>, usize, usize) {
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

pub(crate) fn truncate_width(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let w = char_width(ch);
        if width + w > max {
            if width < max {
                out.push('.');
            }
            return out;
        }
        out.push(ch);
        width += w;
    }
    out
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

pub(crate) use envoy_agent::agent::context::context_used;

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
    ("#".repeat(filled), "-".repeat(CELLS - filled))
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
    use super::{context_bar, fmt_tokens};

    #[test]
    fn token_labels_and_bar() {
        assert_eq!(fmt_tokens(800), "800");
        assert_eq!(fmt_tokens(12_400), "12.4k");
        assert_eq!(fmt_tokens(50_000), "50k");
        assert_eq!(fmt_tokens(1_048_576), "1.0m");
        assert_eq!(context_bar(0, 200_000), (String::new(), "-".repeat(8)));
        let (filled, empty) = context_bar(50_000, 200_000);
        assert_eq!(filled.chars().count() + empty.chars().count(), 8);
        assert!(!filled.is_empty());
        let bar = format!("{filled}{empty}");
        assert!(bar.is_ascii());
        assert_eq!(super::display_width(&bar), 8);
        let row = super::pad_to_width("> current   /tmp/project", 40);
        assert_eq!(super::display_width(&row), 40);
        assert!(row.is_ascii());
    }
}
