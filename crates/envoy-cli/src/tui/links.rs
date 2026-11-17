//! Workspace file index and automatic underlined clickable file-link resolution.
use super::theme::Theme;
use super::HitTarget;
use crate::session::FileChange;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const INDEX_TTL: Duration = Duration::from_secs(5);
const MAX_INDEXED_FILES: usize = 5000;

#[derive(Default)]
pub(crate) struct WorkspaceIndex {
    cwd: PathBuf,
    files: Vec<String>,
    by_basename: HashMap<String, Vec<String>>,
    built_at: Option<Instant>,
}

impl WorkspaceIndex {
    pub(crate) fn invalidate(&mut self) {
        self.built_at = None;
    }

    pub(crate) fn ensure_fresh(&mut self, cwd: &Path) {
        let stale = self
            .built_at
            .map(|t| t.elapsed() >= INDEX_TTL)
            .unwrap_or(true);
        if self.cwd == cwd && !stale {
            return;
        }
        self.cwd = cwd.to_path_buf();
        self.files.clear();
        self.by_basename.clear();
        walk_workspace(cwd, cwd, &mut self.files);
        for rel in &self.files {
            if let Some(name) = Path::new(rel).file_name().and_then(|s| s.to_str()) {
                self.by_basename
                    .entry(name.to_string())
                    .or_default()
                    .push(rel.clone());
            }
        }
        self.built_at = Some(Instant::now());
    }

    pub(crate) fn resolve_path(
        &self,
        cwd: &Path,
        changed: &[FileChange],
        raw: &str,
    ) -> Option<(String, Option<usize>)> {
        let (candidate, line) = split_path_and_line(raw)?;
        if candidate.is_empty()
            || candidate.starts_with("http://")
            || candidate.starts_with("https://")
        {
            return None;
        }

        // 1. Exact or suffix match in session changed_files (highest priority)
        if let Some(found) = match_in_changed(cwd, changed, &candidate) {
            return Some((found, line));
        }

        // 2. Direct filesystem check (relative to cwd or absolute)
        let p = Path::new(&candidate);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        if joined.is_file() {
            let rel = normalize_display_path(cwd, &joined);
            return Some((rel, line));
        }

        // 3. Indexed workspace suffix / unique basename match
        if candidate.contains('/') {
            let suffix = format!("/{}", candidate.trim_start_matches("./"));
            let mut hits = self
                .files
                .iter()
                .filter(|f| *f == &candidate || f.ends_with(&suffix));
            if let Some(first) = hits.next() {
                if hits.next().is_none() {
                    return Some((first.clone(), line));
                }
            }
        } else if looks_like_filename(&candidate) {
            if let Some(matches) = self.by_basename.get(&candidate) {
                if matches.len() == 1 {
                    return Some((matches[0].clone(), line));
                }
            }
        }

        None
    }
}

fn walk_workspace(root: &Path, dir: &Path, out: &mut Vec<String>) {
    if out.len() >= MAX_INDEXED_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        if out.len() >= MAX_INDEXED_FILES {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                name_str.as_ref(),
                ".git" | "target" | "node_modules" | ".venv" | "dist" | "build" | ".next"
            ) {
                continue;
            }
            dirs.push(path);
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    for sub in dirs {
        if out.len() >= MAX_INDEXED_FILES {
            break;
        }
        walk_workspace(root, &sub, out);
    }
}

fn match_in_changed(cwd: &Path, changed: &[FileChange], candidate: &str) -> Option<String> {
    let clean = candidate.trim_start_matches("./");
    let suffix = format!("/{clean}");
    for fc in changed.iter().rev() {
        let rel = normalize_display_path(cwd, Path::new(&fc.path));
        if fc.path == candidate || rel == clean || rel.ends_with(&suffix) {
            return Some(rel);
        }
    }
    None
}

pub(crate) fn normalize_display_path(cwd: &Path, path: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(cwd) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    if let (Ok(c_cwd), Ok(c_path)) = (std::fs::canonicalize(cwd), std::fs::canonicalize(path)) {
        if let Ok(rel) = c_path.strip_prefix(&c_cwd) {
            return rel.to_string_lossy().replace('\\', "/");
        }
    }
    path.to_string_lossy().replace('\\', "/")
}

pub(crate) fn split_path_and_line(raw: &str) -> Option<(String, Option<usize>)> {
    let mut s = raw.trim().trim_matches(|c| matches!(c, '"' | '\'' | '(' | ')' | '[' | ']' | ',' | ';'));
    if let Some(rest) = s.strip_prefix("file://") {
        s = rest;
    }
    if s.is_empty() {
        return None;
    }
    // Strip `#L42` or `#L42-L50`
    if let Some((base, frag)) = s.rsplit_once('#') {
        if let Some(l) = frag.strip_prefix('L').and_then(|n| {
            n.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|d| d.parse::<usize>().ok())
        }) {
            return Some((base.to_string(), Some(l)));
        }
        return Some((base.to_string(), None));
    }
    // Strip `:42` or `:42:10`
    if let Some((base, tail)) = s.rsplit_once(':') {
        if let Ok(col_or_line) = tail.parse::<usize>() {
            if let Some((base2, line_str)) = base.rsplit_once(':') {
                if let Ok(line) = line_str.parse::<usize>() {
                    return Some((base2.to_string(), Some(line)));
                }
            }
            return Some((base.to_string(), Some(col_or_line)));
        }
    }
    Some((s.to_string(), None))
}

fn looks_like_filename(token: &str) -> bool {
    if matches!(
        token,
        "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
            | "tsconfig.json"
            | "Makefile"
            | "Dockerfile"
            | "README.md"
            | "SKILL.md"
    ) {
        return true;
    }
    let Some((_, ext)) = token.rsplit_once('.') else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "py"
            | "go"
            | "toml"
            | "json"
            | "yaml"
            | "yml"
            | "md"
            | "sh"
            | "css"
            | "html"
            | "sql"
            | "txt"
            | "c"
            | "cc"
            | "cpp"
            | "h"
            | "hpp"
            | "java"
    )
}

#[derive(Clone)]
pub(crate) struct LinkedSpan {
    pub(crate) span: Span<'static>,
    pub(crate) target: Option<HitTarget>,
}

pub(crate) fn inline_spans_linked(
    line: &str,
    theme: &Theme,
    cwd: &Path,
    changed: &[FileChange],
    index: &WorkspaceIndex,
) -> Vec<LinkedSpan> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut i = 0usize;

    let flush_plain = |plain: &mut String, out: &mut Vec<LinkedSpan>| {
        if !plain.is_empty() {
            let text = std::mem::take(plain);
            push_plain_with_bare_links(&text, theme, cwd, changed, index, out);
        }
    };

    while i < line.len() {
        let rest = &line[i..];

        // Markdown link: [label](target)
        if rest.starts_with('[') {
            if let Some(close_bracket) = rest.find("](") {
                let after = &rest[close_bracket + 2..];
                if let Some(close_paren) = after.find(')') {
                    let label = &rest[1..close_bracket];
                    let target_raw = &after[..close_paren];
                    if !label.is_empty() && !target_raw.is_empty() {
                        flush_plain(&mut plain, &mut out);
                        if let Some((path, ln)) = index.resolve_path(cwd, changed, target_raw) {
                            out.push(LinkedSpan {
                                span: Span::styled(
                                    label.to_string(),
                                    Style::default()
                                        .fg(theme.link)
                                        .add_modifier(Modifier::UNDERLINED),
                                ),
                                target: Some(HitTarget::OpenFile { path, line: ln }),
                            });
                        } else {
                            out.push(LinkedSpan {
                                span: Span::styled(
                                    label.to_string(),
                                    Style::default()
                                        .fg(theme.link)
                                        .add_modifier(Modifier::UNDERLINED),
                                ),
                                target: None,
                            });
                        }
                        i += close_bracket + 2 + close_paren + 1;
                        continue;
                    }
                }
            }
        }

        // Inline code: `...`
        if rest.starts_with('`') {
            let after = &rest[1..];
            if let Some(end) = after.find('`') {
                if end > 0 {
                    flush_plain(&mut plain, &mut out);
                    let content = &after[..end];
                    if let Some((path, ln)) = index.resolve_path(cwd, changed, content) {
                        out.push(LinkedSpan {
                            span: Span::styled(
                                content.to_string(),
                                Style::default()
                                    .fg(theme.link)
                                    .bg(theme.surface)
                                    .add_modifier(Modifier::UNDERLINED),
                            ),
                            target: Some(HitTarget::OpenFile { path, line: ln }),
                        });
                    } else {
                        out.push(LinkedSpan {
                            span: Span::styled(
                                content.to_string(),
                                Style::default().fg(theme.secondary).bg(theme.surface),
                            ),
                            target: None,
                        });
                    }
                    i += 1 + end + 1;
                    continue;
                }
            }
        }

        // **bold** or *italic*
        let marker = if rest.starts_with("**") {
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
                    flush_plain(&mut plain, &mut out);
                    let content = &after[..end];
                    if marker == "**" {
                        if let Some((path, ln)) = index.resolve_path(cwd, changed, content) {
                            out.push(LinkedSpan {
                                span: Span::styled(
                                    content.to_string(),
                                    style.fg(theme.link).add_modifier(Modifier::UNDERLINED),
                                ),
                                target: Some(HitTarget::OpenFile { path, line: ln }),
                            });
                            i += marker.len() + end + marker.len();
                            continue;
                        }
                    }
                    out.push(LinkedSpan {
                        span: Span::styled(content.to_string(), style),
                        target: None,
                    });
                    i += marker.len() + end + marker.len();
                    continue;
                }
            }
        }

        let ch = rest.chars().next().expect("non-empty");
        plain.push(ch);
        i += ch.len_utf8();
    }

    flush_plain(&mut plain, &mut out);
    if out.is_empty() {
        out.push(LinkedSpan {
            span: Span::raw(String::new()),
            target: None,
        });
    }
    out
}

fn push_plain_with_bare_links(
    text: &str,
    theme: &Theme,
    cwd: &Path,
    changed: &[FileChange],
    index: &WorkspaceIndex,
    out: &mut Vec<LinkedSpan>,
) {
    let mut start = 0usize;
    let mut buf = String::new();
    for (idx, ch) in text.char_indices() {
        let is_sep = ch.is_whitespace()
            || matches!(ch, '(' | ')' | '[' | ']' | '<' | '>' | ',' | ';' | '"' | '\'' | '，' | '。' | '；' | '（' | '）');
        if is_sep {
            if start < idx {
                let tok = &text[start..idx];
                emit_token(tok, theme, cwd, changed, index, &mut buf, out);
            }
            buf.push(ch);
            start = idx + ch.len_utf8();
        }
    }
    if start < text.len() {
        emit_token(&text[start..], theme, cwd, changed, index, &mut buf, out);
    }
    if !buf.is_empty() {
        out.push(LinkedSpan {
            span: Span::raw(buf),
            target: None,
        });
    }
}

fn emit_token(
    tok: &str,
    theme: &Theme,
    cwd: &Path,
    changed: &[FileChange],
    index: &WorkspaceIndex,
    buf: &mut String,
    out: &mut Vec<LinkedSpan>,
) {
    let trimmed = tok.trim_end_matches('.');
    let suffix_len = tok.len() - trimmed.len();
    if (trimmed.contains('/') || trimmed.contains('.')) && trimmed.len() >= 3 {
        if let Some((path, ln)) = index.resolve_path(cwd, changed, trimmed) {
            if !buf.is_empty() {
                out.push(LinkedSpan {
                    span: Span::raw(std::mem::take(buf)),
                    target: None,
                });
            }
            out.push(LinkedSpan {
                span: Span::styled(
                    trimmed.to_string(),
                    Style::default()
                        .fg(theme.link)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                target: Some(HitTarget::OpenFile { path, line: ln }),
            });
            if suffix_len > 0 {
                buf.push_str(&tok[trimmed.len()..]);
            }
            return;
        }
    }
    buf.push_str(tok);
}
