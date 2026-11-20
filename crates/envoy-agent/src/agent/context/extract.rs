//! Fold source observations down to an outline.
//!
//! Rust uses a lightweight structural scanner for item headers and bodies.
//! Other source files use signature lines and indent. Shell logs and prose stay
//! with the caller.

use std::collections::HashSet;

const SOURCE_EXT: &[&str] = &[
    "rs", "py", "pyi", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "java", "c", "h", "cc", "cpp",
    "cxx", "hpp", "hh", "rb", "swift", "kt", "kts", "cs", "php", "scala", "lua", "zig", "sh",
    "bash", "zsh", "sql", "vue", "svelte",
];

pub fn is_source_path(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("");
    !ext.is_empty() && ext.len() < 8 && SOURCE_EXT.iter().any(|e| ext.eq_ignore_ascii_case(e))
}

/// Outline of a source file. `None` when the path is not source.
pub fn fold_source(path: &str, source: &str, focus: &HashSet<String>) -> Option<String> {
    if !is_source_path(path) {
        return None;
    }
    let folded = if path
        .rsplit('.')
        .next()
        .is_some_and(|e| e.eq_ignore_ascii_case("rs"))
    {
        fold_rust(source, focus).unwrap_or_else(|| fold_generic(source, focus))
    } else {
        fold_generic(source, focus)
    };
    Some(folded)
}

fn fold_rust(source: &str, focus: &HashSet<String>) -> Option<String> {
    let mut out = String::new();
    scan_rust_items(source, focus, &mut out);
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn scan_rust_items(src: &str, focus: &HashSet<String>, out: &mut String) {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        i = skip_trivia_and_attrs(src, i);
        if i >= bytes.len() {
            break;
        }
        let start = i;
        let Some((delim_pos, delim)) = find_item_delimiter(src, start) else {
            break;
        };
        if delim == b';' {
            let item = &src[start..=delim_pos];
            let (kind, _) = classify_rust_item(item);
            if matches!(
                kind,
                Some("use" | "mod" | "type" | "const" | "static" | "fn" | "struct")
            ) {
                push_line(out, &one_line(item));
            }
            i = delim_pos + 1;
            continue;
        }

        let Some(close_brace) = find_matching_brace(src, delim_pos) else {
            break;
        };
        let header = &src[start..delim_pos];
        let (kind, name) = classify_rust_item(header);
        if !name.is_empty() && focus.contains(&name) {
            push_block(out, &src[start..=close_brace]);
        } else if matches!(kind, Some("impl" | "mod")) {
            push_line(out, &one_line(header));
            scan_rust_items(&src[delim_pos + 1..close_brace], focus, out);
        } else if matches!(kind, Some("fn" | "struct" | "enum" | "union" | "trait")) {
            push_line(out, &one_line(header));
        }
        i = close_brace + 1;
    }
}

fn skip_trivia_and_attrs(src: &str, mut i: usize) -> usize {
    let bytes = src.as_bytes();
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
        } else if let Some(next) = skip_comment_or_literal(src, i, false) {
            i = next;
        } else if bytes[i] == b'#'
            && (bytes.get(i + 1) == Some(&b'[')
                || (bytes.get(i + 1) == Some(&b'!') && bytes.get(i + 2) == Some(&b'[')))
        {
            let bracket = if bytes[i + 1] == b'!' { i + 2 } else { i + 1 };
            i = find_matching_delim(src, bracket, b'[', b']')
                .map(|c| c + 1)
                .unwrap_or(bytes.len());
        } else if bytes[i] == b';' {
            i += 1;
        } else {
            break;
        }
    }
    i
}

fn find_item_delimiter(src: &str, mut i: usize) -> Option<(usize, u8)> {
    let bytes = src.as_bytes();
    let mut paren = 0usize;
    let mut bracket = 0usize;
    while i < bytes.len() {
        if let Some(next) = skip_comment_or_literal(src, i, true) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            b';' | b'{' if paren == 0 && bracket == 0 => return Some((i, bytes[i])),
            _ => {}
        }
        i += 1;
    }
    None
}

fn find_matching_brace(src: &str, open_pos: usize) -> Option<usize> {
    find_matching_delim(src, open_pos, b'{', b'}')
}

fn find_matching_delim(src: &str, open_pos: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    let mut i = open_pos;
    while i < bytes.len() {
        if let Some(next) = skip_comment_or_literal(src, i, true) {
            i = next;
            continue;
        }
        if bytes[i] == open {
            depth += 1;
        } else if bytes[i] == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Skip comments (and optionally strings/raw strings/char literals) starting at `i`.
fn skip_comment_or_literal(src: &str, i: usize, include_literals: bool) -> Option<usize> {
    let bytes = src.as_bytes();
    if bytes.get(i..i + 2) == Some(b"//") {
        let mut j = i + 2;
        while j < bytes.len() && bytes[j] != b'\n' {
            j += 1;
        }
        return Some(j);
    }
    if bytes.get(i..i + 2) == Some(b"/*") {
        let mut depth = 1usize;
        let mut j = i + 2;
        while j + 1 < bytes.len() && depth > 0 {
            if &bytes[j..j + 2] == b"/*" {
                depth += 1;
                j += 2;
            } else if &bytes[j..j + 2] == b"*/" {
                depth -= 1;
                j += 2;
            } else {
                j += 1;
            }
        }
        return Some(j);
    }
    if !include_literals {
        return None;
    }
    if bytes[i] == b'r' {
        if let Some(next) = skip_raw_string(bytes, i) {
            return Some(next);
        }
    }
    if bytes[i] == b'"' {
        let mut j = i + 1;
        while j < bytes.len() {
            if bytes[j] == b'\\' {
                j += 2;
                continue;
            }
            if bytes[j] == b'"' {
                return Some(j + 1);
            }
            j += 1;
        }
        return Some(bytes.len());
    }
    if bytes[i] == b'\'' {
        return skip_char_literal(src, i);
    }
    None
}

fn skip_raw_string(bytes: &[u8], i: usize) -> Option<usize> {
    let prev_ident = i > 0
        && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_')
        && !(bytes[i - 1] == b'b'
            && (i == 1 || !(bytes[i - 2].is_ascii_alphanumeric() || bytes[i - 2] == b'_')));
    if prev_ident {
        return None;
    }
    let mut j = i + 1;
    let mut hashes = 0usize;
    while j < bytes.len() && bytes[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    if bytes.get(j) != Some(&b'"') {
        return None;
    }
    j += 1;
    while j < bytes.len() {
        if bytes[j] == b'"' {
            let end = j + 1;
            if end + hashes <= bytes.len() && bytes[end..end + hashes].iter().all(|&b| b == b'#') {
                return Some(end + hashes);
            }
        }
        j += 1;
    }
    Some(bytes.len())
}

fn skip_char_literal(src: &str, i: usize) -> Option<usize> {
    let rest = &src[i + 1..];
    if let Some(after_esc) = rest.strip_prefix('\\') {
        if after_esc.starts_with("u{") {
            if let Some(close) = after_esc.find("}'") {
                if close <= 10 {
                    return Some(i + 1 + 1 + close + 2);
                }
            }
        } else if after_esc.starts_with('x') && after_esc.as_bytes().get(3) == Some(&b'\'') {
            return Some(i + 6);
        } else if let Some(ch) = after_esc.chars().next() {
            let close_idx = 1 + 1 + ch.len_utf8();
            if src.as_bytes().get(i + close_idx) == Some(&b'\'') {
                return Some(i + close_idx + 1);
            }
        }
        return None;
    }
    let ch = rest.chars().next()?;
    if ch == '\n' || ch == '\r' || ch == '\'' {
        return None;
    }
    let after = i + 1 + ch.len_utf8();
    (src.as_bytes().get(after) == Some(&b'\'')).then_some(after + 1)
}

fn classify_rust_item(header: &str) -> (Option<&'static str>, String) {
    let tokens = top_level_words(header);
    let mut idx = 0;
    while idx < tokens.len() {
        let tok = tokens[idx].as_str();
        match tok {
            "pub" | "async" | "unsafe" | "default" | "extern" => idx += 1,
            "const" if matches!(tokens.get(idx + 1).map(String::as_str), Some("fn" | "unsafe")) => {
                idx += 1;
            }
            "use" => return (Some("use"), String::new()),
            "impl" => return (Some("impl"), String::new()),
            "fn" => return (Some("fn"), tokens.get(idx + 1).cloned().unwrap_or_default()),
            "struct" => {
                return (
                    Some("struct"),
                    tokens.get(idx + 1).cloned().unwrap_or_default(),
                )
            }
            "enum" => return (Some("enum"), tokens.get(idx + 1).cloned().unwrap_or_default()),
            "union" => {
                return (
                    Some("union"),
                    tokens.get(idx + 1).cloned().unwrap_or_default(),
                )
            }
            "trait" => return (Some("trait"), tokens.get(idx + 1).cloned().unwrap_or_default()),
            "mod" => return (Some("mod"), tokens.get(idx + 1).cloned().unwrap_or_default()),
            "type" => return (Some("type"), tokens.get(idx + 1).cloned().unwrap_or_default()),
            "const" => return (Some("const"), tokens.get(idx + 1).cloned().unwrap_or_default()),
            "static" => {
                return (
                    Some("static"),
                    tokens.get(idx + 1).cloned().unwrap_or_default(),
                )
            }
            _ => break,
        }
    }
    (None, String::new())
}

fn top_level_words(header: &str) -> Vec<String> {
    let bytes = header.as_bytes();
    let mut words = Vec::new();
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut angle = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        if let Some(next) = skip_comment_or_literal(header, i, true) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            b'<' if paren == 0 && bracket == 0 => angle += 1,
            b'>' if paren == 0 && bracket == 0 => angle = angle.saturating_sub(1),
            b if paren == 0
                && bracket == 0
                && angle == 0
                && (b.is_ascii_alphabetic() || b == b'_') =>
            {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                words.push(header[start..i].to_string());
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    words
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn push_line(out: &mut String, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line);
}

fn push_block(out: &mut String, block: &str) {
    let block = block.trim();
    if block.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(block);
}

fn fold_generic(source: &str, focus: &HashSet<String>) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_error_line(line) {
            out.push(line.trim_end().to_string());
            i += 1;
            continue;
        }
        if let Some(name) = signature_name(line) {
            let indent = indent_of(line);
            if focus.contains(&name) {
                out.push(line.trim_end().to_string());
                i += 1;
                while i < lines.len() && keeps_body_line(lines[i], indent) {
                    out.push(lines[i].trim_end().to_string());
                    i += 1;
                }
            } else {
                out.push(one_line(line));
                i += 1;
                while i < lines.len() && keeps_body_line(lines[i], indent) {
                    i += 1;
                }
            }
            continue;
        }
        i += 1;
    }
    if out.is_empty() {
        return head_tail(source);
    }
    out.join("\n")
}

fn keeps_body_line(line: &str, header_indent: usize) -> bool {
    line.trim().is_empty() || indent_of(line) > header_indent
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

fn is_error_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error") || lower.contains("panic")
}

fn signature_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
        return None;
    }
    const MARKS: &[&str] = &[
        "fn ",
        "def ",
        "class ",
        "struct ",
        "enum ",
        "interface ",
        "trait ",
        "type ",
        "func ",
        "function ",
        "impl ",
    ];
    let rest = MARKS
        .iter()
        .find_map(|mark| trimmed.find(mark).map(|idx| &trimmed[idx + mark.len()..]))?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.len() < 2 {
        None
    } else {
        Some(name)
    }
}

fn head_tail(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    if lines.len() <= 18 {
        return source.to_string();
    }
    let mut out = lines[..8].join("\n");
    out.push_str("\n...");
    out.push('\n');
    out.push_str(&lines[lines.len() - 10..].join("\n"));
    out
}

pub fn collect_idents(text: &str, into: &mut HashSet<String>) {
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            push_ident(&cur, into);
            cur.clear();
        }
    }
    push_ident(&cur, into);
}

fn push_ident(cur: &str, into: &mut HashSet<String>) {
    if cur.len() < 3 || cur.chars().all(|c| c.is_ascii_digit()) {
        return;
    }
    const SKIP: &[&str] = &[
        "the", "and", "for", "fn", "let", "pub", "use", "mod", "error", "Error", "failed", "panic",
        "true", "false", "self", "Self", "mut", "impl",
    ];
    if SKIP.contains(&cur) {
        return;
    }
    into.insert(cur.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_outline_keeps_named_function_and_folds_the_rest() {
        let src = r#"
use std::io::Read;

fn keep_me() {
    let marker = 1;
    println!("{marker}");
}

fn drop_me() {
    let secret = "SHOULD_FOLD";
    println!("{secret}");
}
"#;
        let focus = HashSet::from(["keep_me".to_string()]);
        let out = fold_source("src/lib.rs", src, &focus).unwrap();
        assert!(out.contains("use std::io::Read"), "{out}");
        assert!(out.contains("fn keep_me"), "{out}");
        assert!(out.contains("marker"), "{out}");
        assert!(out.contains("fn drop_me"), "{out}");
        assert!(!out.contains("SHOULD_FOLD"), "{out}");
    }

    #[test]
    fn rust_outline_expands_focused_method_inside_impl() {
        let src = r#"
struct Foo<'a>(&'a str);

impl<'a> Foo<'a> {
    pub fn keep_method(&self) -> char {
        let brace = '{';
        let escaped = '\'';
        brace
    }

    fn fold_method(&self) {
        let hidden = "METHOD_SECRET";
        println!("{hidden}");
    }
}
"#;
        let focus = HashSet::from(["keep_method".to_string()]);
        let out = fold_source("src/foo.rs", src, &focus).unwrap();
        assert!(out.contains("impl<'a> Foo<'a>"), "{out}");
        assert!(out.contains("let brace = '{';"), "{out}");
        assert!(out.contains("fn fold_method"), "{out}");
        assert!(!out.contains("METHOD_SECRET"), "{out}");
    }

    #[test]
    fn generic_outline_keeps_the_named_block() {
        let src = "def keep_me():\n    return 1\n\ndef drop_me():\n    secret = 'SHOULD_FOLD'\n    return secret\n";
        let focus = HashSet::from(["keep_me".to_string()]);
        let out = fold_source("app.py", src, &focus).unwrap();
        assert!(out.contains("return 1"), "{out}");
        assert!(out.contains("def drop_me"), "{out}");
        assert!(!out.contains("SHOULD_FOLD"), "{out}");
    }
}
