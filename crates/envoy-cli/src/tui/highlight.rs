//! Lightweight single-line syntax highlighting for code blocks and file previews.
use super::theme::Theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lang {
    Rust,
    JsTs,
    Python,
    Toml,
    Json,
    Yaml,
    Shell,
    Markdown,
    Plain,
}

impl Lang {
    pub(crate) fn from_hint(hint: &str) -> Self {
        let lower = hint.trim().to_ascii_lowercase();
        match lower.as_str() {
            "rs" | "rust" => Self::Rust,
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "typescript" | "javascript" => Self::JsTs,
            "py" | "python" => Self::Python,
            "toml" => Self::Toml,
            "json" | "jsonc" => Self::Json,
            "yml" | "yaml" => Self::Yaml,
            "sh" | "bash" | "zsh" | "shell" => Self::Shell,
            "md" | "markdown" => Self::Markdown,
            _ => Self::Plain,
        }
    }

    pub(crate) fn from_path(path: &Path) -> Self {
        path.extension()
            .and_then(|e| e.to_str())
            .map(Self::from_hint)
            .unwrap_or(Self::Plain)
    }
}

fn is_keyword(lang: Lang, word: &str) -> bool {
    match lang {
        Lang::Rust => matches!(
            word,
            "as" | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "crate"
                | "dyn"
                | "else"
                | "enum"
                | "extern"
                | "false"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "self"
                | "Self"
                | "static"
                | "struct"
                | "super"
                | "trait"
                | "true"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
        ),
        Lang::JsTs => matches!(
            word,
            "as" | "async"
                | "await"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "default"
                | "else"
                | "enum"
                | "export"
                | "extends"
                | "false"
                | "finally"
                | "for"
                | "from"
                | "function"
                | "if"
                | "implements"
                | "import"
                | "in"
                | "interface"
                | "let"
                | "new"
                | "null"
                | "of"
                | "return"
                | "static"
                | "switch"
                | "throw"
                | "true"
                | "try"
                | "type"
                | "typeof"
                | "undefined"
                | "var"
                | "void"
                | "while"
        ),
        Lang::Python => matches!(
            word,
            "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "False"
                | "finally"
                | "for"
                | "from"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "None"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "True"
                | "try"
                | "while"
                | "with"
                | "yield"
        ),
        Lang::Shell => matches!(
            word,
            "if" | "then" | "else" | "elif" | "fi" | "for" | "in" | "do" | "done" | "case"
                | "esac" | "while" | "function" | "export" | "local" | "return" | "true"
                | "false"
        ),
        Lang::Toml | Lang::Yaml | Lang::Json => matches!(word, "true" | "false" | "null"),
        Lang::Markdown | Lang::Plain => false,
    }
}

fn comment_prefix(lang: Lang) -> Option<&'static str> {
    match lang {
        Lang::Rust | Lang::JsTs => Some("//"),
        Lang::Python | Lang::Toml | Lang::Yaml | Lang::Shell => Some("#"),
        Lang::Json | Lang::Markdown | Lang::Plain => None,
    }
}

pub(crate) fn highlight_code_line<'a>(
    line: &str,
    lang: Lang,
    theme: &Theme,
    base_style: Style,
) -> Vec<Span<'a>> {
    if lang == Lang::Plain || line.is_empty() {
        return vec![Span::styled(line.to_string(), base_style)];
    }
    if lang == Lang::Markdown {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            return vec![Span::styled(
                line.to_string(),
                base_style.fg(theme.accent).add_modifier(Modifier::BOLD),
            )];
        }
    }
    if lang == Lang::Toml && line.trim_start().starts_with('[') {
        return vec![Span::styled(
            line.to_string(),
            base_style.fg(theme.syntax_type).add_modifier(Modifier::BOLD),
        )];
    }

    let c_prefix = comment_prefix(lang);
    let mut spans = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut plain = String::new();

    let flush = |plain: &mut String, spans: &mut Vec<Span<'a>>| {
        if !plain.is_empty() {
            spans.push(Span::styled(std::mem::take(plain), base_style));
        }
    };

    while i < line.len() {
        let rest = &line[i..];
        if let Some(cp) = c_prefix {
            if rest.starts_with(cp) {
                flush(&mut plain, &mut spans);
                spans.push(Span::styled(
                    rest.to_string(),
                    base_style.fg(theme.syntax_comment),
                ));
                break;
            }
        }
        let b = bytes[i];
        if b == b'"' || (b == b'\'' && lang != Lang::Rust) {
            flush(&mut plain, &mut spans);
            let quote = b as char;
            let mut end = i + 1;
            let mut escaped = false;
            while end < line.len() {
                let ch = line[end..].chars().next().expect("non-empty");
                end += ch.len_utf8();
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                } else if ch == quote {
                    break;
                }
            }
            spans.push(Span::styled(
                line[i..end].to_string(),
                base_style.fg(theme.syntax_str),
            ));
            i = end;
            continue;
        }
        if b.is_ascii_digit()
            && (i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_'))
        {
            flush(&mut plain, &mut spans);
            let mut end = i + 1;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'.' | b'_'))
            {
                end += 1;
            }
            spans.push(Span::styled(
                line[i..end].to_string(),
                base_style.fg(theme.syntax_num),
            ));
            i = end;
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let mut end = i + 1;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            let word = &line[i..end];
            if is_keyword(lang, word) {
                flush(&mut plain, &mut spans);
                spans.push(Span::styled(
                    word.to_string(),
                    base_style.fg(theme.syntax_kw),
                ));
            } else if word.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && matches!(lang, Lang::Rust | Lang::JsTs | Lang::Python)
            {
                flush(&mut plain, &mut spans);
                spans.push(Span::styled(
                    word.to_string(),
                    base_style.fg(theme.syntax_type),
                ));
            } else {
                plain.push_str(word);
            }
            i = end;
            continue;
        }
        let ch = rest.chars().next().expect("non-empty");
        plain.push(ch);
        i += ch.len_utf8();
    }
    flush(&mut plain, &mut spans);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base_style));
    }
    spans
}
