//! Shared YAML-subset frontmatter parser for skills, subagents, and memory notes.

/// Split a UTF-8 markdown document starting with `---\n` or `---\r\n` into
/// `(Some(frontmatter), body)`. Handles closing `---` even at EOF without a
/// trailing newline.
pub(crate) fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let normalized = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let Some(rest) = normalized
        .strip_prefix("---\n")
        .or_else(|| normalized.strip_prefix("---\r\n"))
    else {
        return (None, normalized);
    };

    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']).trim();
        if trimmed == "---" {
            let fm = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (Some(fm), body);
        }
        offset += line.len();
    }
    (None, normalized)
}

/// Strip an unquoted inline YAML `#` comment preceded by whitespace (or at byte 0).
pub(crate) fn strip_inline_comment(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for (i, &b) in bytes.iter().enumerate() {
        if in_double {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_double = false;
            }
            continue;
        }
        if in_single {
            if b == b'\'' {
                in_single = false;
            }
            continue;
        }
        match b {
            b'"' => in_double = true,
            b'\'' => in_single = true,
            b'#' if i == 0 || bytes[i - 1].is_ascii_whitespace() => {
                return raw[..i].trim_end();
            }
            _ => {}
        }
    }
    raw
}

/// Strip inline comments, trim whitespace, and unwrap matching `'...'` / `"..."` quotes.
pub(crate) fn unquote(raw: &str) -> String {
    let s = strip_inline_comment(raw).trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Parse an inline YAML list such as `[Read, Grep]` or `Read, Edit` after stripping
/// inline comments.
pub(crate) fn parse_inline_list(raw: &str, split_whitespace: bool) -> Vec<String> {
    let cleaned = strip_inline_comment(raw).trim();
    if cleaned.is_empty() {
        return Vec::new();
    }
    if cleaned.starts_with('[') && cleaned.ends_with(']') {
        return cleaned[1..cleaned.len() - 1]
            .split(',')
            .map(unquote)
            .filter(|s| !s.is_empty())
            .collect();
    }
    if split_whitespace {
        cleaned
            .split([',', ' '])
            .map(unquote)
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        cleaned
            .split(',')
            .map(unquote)
            .filter(|s| !s.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_frontmatter_handles_crlf_and_eof_without_trailing_newline() {
        let crlf = "\u{feff}---\r\nname: demo\r\n---\r\nbody";
        let (fm, body) = split_frontmatter(crlf);
        assert_eq!(fm, Some("name: demo\r\n"));
        assert_eq!(body, "body");

        let eof = "---\ntitle: note\n---";
        let (fm, body) = split_frontmatter(eof);
        assert_eq!(fm, Some("title: note\n"));
        assert_eq!(body, "");
    }

    #[test]
    fn strip_inline_comment_and_unquote_preserve_hash_in_quotes_and_urls() {
        assert_eq!(strip_inline_comment("sonnet # fast model"), "sonnet");
        assert_eq!(
            strip_inline_comment("https://a.test#sec # docs anchor"),
            "https://a.test#sec"
        );
        assert_eq!(unquote(" \"a # b\" # comment"), "a # b");
        assert_eq!(unquote(" 'single # ok' # tail"), "single # ok");
    }

    #[test]
    fn parse_inline_list_handles_brackets_and_comments() {
        assert_eq!(
            parse_inline_list("[Read, \"Edit\"] # read only", false),
            vec!["Read", "Edit"]
        );
        assert_eq!(
            parse_inline_list("Read Grep # whitespace split", true),
            vec!["Read", "Grep"]
        );
    }
}
