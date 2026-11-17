//! Recursive workspace `glob` and `grep` primitives.
//!
//! Skips VCS/build/protected directories (`.git`, `target`, `node_modules`,
//! `.ssh`, `.aws`, `.envoy`, `.codex`, `.agents`), avoids symlink escapes,
//! ignores binary files and `.env*` secret files, and bounds output volume.

use anyhow::{Context, Result};
use envoy_proto::GrepHit;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".ssh",
    ".aws",
    ".envoy",
    ".codex",
    ".agents",
];
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LINE_CHARS: usize = 512;
const DEFAULT_MAX_RESULTS: usize = 200;

/// Recursively find files under `root` whose relative path matches `pattern`.
pub async fn glob_files(root: &str, pattern: &str, max_results: u32) -> Result<Vec<String>> {
    let root = root.to_string();
    let pattern = pattern.to_string();
    let cap = normalize_cap(max_results);
    tokio::task::spawn_blocking(move || glob_files_sync(&root, &pattern, cap))
        .await
        .context("glob task panicked")?
}

/// Recursively search text files under `root` for lines matching `pattern`.
pub async fn grep_files(
    root: &str,
    pattern: &str,
    include: Option<&str>,
    max_results: u32,
) -> Result<Vec<GrepHit>> {
    let root = root.to_string();
    let pattern = pattern.to_string();
    let include = include.map(str::to_string);
    let cap = normalize_cap(max_results);
    tokio::task::spawn_blocking(move || {
        grep_files_sync(&root, &pattern, include.as_deref(), cap)
    })
    .await
    .context("grep task panicked")?
}

fn normalize_cap(max_results: u32) -> usize {
    if max_results == 0 {
        DEFAULT_MAX_RESULTS
    } else {
        (max_results as usize).min(2000)
    }
}

fn resolve_root(root: &str) -> Result<PathBuf> {
    let raw = if root.trim().is_empty() { "." } else { root.trim() };
    let resolved = crate::fs::resolve(raw);
    let canon = std::fs::canonicalize(&resolved)
        .with_context(|| format!("search root {}", resolved.display()))?;
    if !canon.is_dir() {
        anyhow::bail!("{} is not a directory", canon.display());
    }
    Ok(canon)
}

fn walk_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut dirs = VecDeque::new();
    dirs.push_back(root.to_path_buf());
    let mut files = Vec::new();

    while let Some(dir) = dirs.pop_front() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<_> = rd.flatten().collect();
        children.sort_by_key(|e| e.file_name());
        for entry in children {
            let name_os = entry.file_name();
            let name = name_os.to_string_lossy();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            // Never traverse symlinked directories or follow escapes.
            if ft.is_symlink() {
                continue;
            }
            let path = entry.path();
            if ft.is_dir() {
                if SKIP_DIRS.contains(&name.as_ref()) {
                    continue;
                }
                dirs.push_back(path);
            } else if ft.is_file() {
                if name == ".env" || name.starts_with(".env.") {
                    continue;
                }
                if let Ok(rel) = path.strip_prefix(root) {
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    files.push((rel_str, path));
                }
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn glob_files_sync(root: &str, pattern: &str, cap: usize) -> Result<Vec<String>> {
    let root_path = resolve_root(root)?;
    let pat = if pattern.trim().is_empty() {
        "**/*"
    } else {
        pattern.trim()
    };
    let mut out = Vec::new();
    for (rel, _) in walk_files(&root_path) {
        if glob_match(pat, &rel) {
            out.push(rel);
            if out.len() >= cap {
                break;
            }
        }
    }
    Ok(out)
}

fn grep_files_sync(
    root: &str,
    pattern: &str,
    include: Option<&str>,
    cap: usize,
) -> Result<Vec<GrepHit>> {
    if pattern.is_empty() {
        anyhow::bail!("grep `pattern` must not be empty");
    }
    let root_path = resolve_root(root)?;
    let matcher = RegexLite::compile(pattern);
    let include_pat = include.map(str::trim).filter(|s| !s.is_empty());
    let mut hits = Vec::new();

    for (rel, full) in walk_files(&root_path) {
        if let Some(inc) = include_pat {
            if !glob_match(inc, &rel)
                && !Path::new(&rel)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| glob_match(inc, n))
            {
                continue;
            }
        }
        let Ok(meta) = std::fs::metadata(&full) else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        if bytes.iter().take(4096).any(|&b| b == 0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        for (idx, line) in text.lines().enumerate() {
            if matcher.is_match(line) {
                let snippet: String = line.chars().take(MAX_LINE_CHARS).collect();
                hits.push(GrepHit {
                    path: rel.clone(),
                    line: (idx + 1) as u32,
                    text: snippet,
                });
                if hits.len() >= cap {
                    return Ok(hits);
                }
            }
        }
    }
    Ok(hits)
}

/// Match a relative POSIX path against a glob pattern (`*`, `?`, `**`, `{a,b}`).
pub fn glob_match(pattern: &str, path: &str) -> bool {
    for alt in expand_braces(pattern) {
        let pat = alt.trim_start_matches("./");
        let p = path.trim_start_matches("./");
        let pat_segs: Vec<&str> = pat.split('/').collect();
        let path_segs: Vec<&str> = p.split('/').collect();
        if match_segments(&pat_segs, &path_segs) {
            return true;
        }
        // Convenience: single-segment patterns like `*.rs` match any basename too.
        if pat_segs.len() == 1 && !pat.contains('/') {
            if let Some(base) = path_segs.last() {
                if match_segment(pat, base) {
                    return true;
                }
            }
        }
    }
    false
}

fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(start) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let Some(rel_end) = pattern[start + 1..].find('}') else {
        return vec![pattern.to_string()];
    };
    let end = start + 1 + rel_end;
    let prefix = &pattern[..start];
    let suffix = &pattern[end + 1..];
    let mut out = Vec::new();
    for item in pattern[start + 1..end].split(',') {
        let combined = format!("{prefix}{item}{suffix}");
        out.extend(expand_braces(&combined));
    }
    out
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    if pat.is_empty() {
        return path.is_empty();
    }
    if pat[0] == "**" {
        if match_segments(&pat[1..], path) {
            return true;
        }
        return !path.is_empty() && match_segments(pat, &path[1..]);
    }
    if path.is_empty() {
        return false;
    }
    match_segment(pat[0], path[0]) && match_segments(&pat[1..], &path[1..])
}

fn match_segment(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=p.len() {
        for j in 1..=t.len() {
            dp[i][j] = match p[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i - 1][j - 1],
                c => dp[i - 1][j - 1] && c == t[j - 1],
            };
        }
    }
    dp[p.len()][t.len()]
}

/// Lightweight regex/substring engine supporting `|`, `^`, `$`, `.`, `*`, `+`,
/// `?`, `[abc]`/`[^a-z]`, `\d`, `\w`, `\s`, and escaped literals, with literal
/// fallback on malformed patterns.
struct RegexLite {
    branches: Vec<Vec<Atom>>,
    literal_fallback: Option<String>,
}

#[derive(Clone)]
enum Atom {
    StartAnchor,
    EndAnchor,
    Rep { kind: Matcher, min: usize, max: Option<usize> },
}

#[derive(Clone)]
enum Matcher {
    Any,
    Lit(char),
    Digit,
    Word,
    Space,
    Class { negated: bool, ranges: Vec<(char, char)> },
}

impl Matcher {
    fn matches(&self, c: char) -> bool {
        match self {
            Matcher::Any => true,
            Matcher::Lit(x) => *x == c,
            Matcher::Digit => c.is_ascii_digit(),
            Matcher::Word => c.is_ascii_alphanumeric() || c == '_',
            Matcher::Space => c.is_whitespace(),
            Matcher::Class { negated, ranges } => {
                let hit = ranges.iter().any(|(a, b)| c >= *a && c <= *b);
                if *negated { !hit } else { hit }
            }
        }
    }
}

impl RegexLite {
    fn compile(pattern: &str) -> Self {
        match parse_branches(pattern) {
            Some(branches) => Self {
                branches,
                literal_fallback: None,
            },
            None => Self {
                branches: Vec::new(),
                literal_fallback: Some(pattern.to_string()),
            },
        }
    }

    fn is_match(&self, line: &str) -> bool {
        if let Some(lit) = &self.literal_fallback {
            return line.contains(lit);
        }
        let chars: Vec<char> = line.chars().collect();
        self.branches.iter().any(|seq| {
            let anchored_start = matches!(seq.first(), Some(Atom::StartAnchor));
            if anchored_start {
                match_seq(seq, &chars, 0)
            } else {
                (0..=chars.len()).any(|start| match_seq(seq, &chars, start))
            }
        })
    }
}

fn parse_branches(pattern: &str) -> Option<Vec<Vec<Atom>>> {
    let mut branches = Vec::new();
    let mut cur = String::new();
    let mut in_class = false;
    let mut escaped = false;
    for ch in pattern.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            cur.push(ch);
            escaped = true;
            continue;
        }
        if ch == '[' && !in_class {
            in_class = true;
            cur.push(ch);
            continue;
        }
        if ch == ']' && in_class {
            in_class = false;
            cur.push(ch);
            continue;
        }
        if ch == '|' && !in_class {
            branches.push(parse_seq(&cur)?);
            cur.clear();
            continue;
        }
        cur.push(ch);
    }
    if in_class || escaped {
        return None;
    }
    branches.push(parse_seq(&cur)?);
    Some(branches)
}

fn parse_seq(s: &str) -> Option<Vec<Atom>> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut atoms = Vec::new();
    while i < chars.len() {
        let atom = match chars[i] {
            '^' => {
                i += 1;
                atoms.push(Atom::StartAnchor);
                continue;
            }
            '$' => {
                i += 1;
                atoms.push(Atom::EndAnchor);
                continue;
            }
            '.' => {
                i += 1;
                Matcher::Any
            }
            '\\' => {
                i += 1;
                let esc = *chars.get(i)?;
                i += 1;
                match esc {
                    'd' => Matcher::Digit,
                    'w' => Matcher::Word,
                    's' => Matcher::Space,
                    c => Matcher::Lit(c),
                }
            }
            '[' => {
                i += 1;
                let negated = chars.get(i) == Some(&'^');
                if negated {
                    i += 1;
                }
                let mut ranges = Vec::new();
                while i < chars.len() && chars[i] != ']' {
                    let start = chars[i];
                    if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i + 2] != ']' {
                        let end = chars[i + 2];
                        ranges.push((start, end));
                        i += 3;
                    } else {
                        ranges.push((start, start));
                        i += 1;
                    }
                }
                if i >= chars.len() || chars[i] != ']' {
                    return None;
                }
                i += 1;
                Matcher::Class { negated, ranges }
            }
            '(' | ')' => {
                // Skip grouping parens when not nested into full sub-expressions.
                i += 1;
                continue;
            }
            c => {
                i += 1;
                Matcher::Lit(c)
            }
        };
        let (min, max) = match chars.get(i) {
            Some('*') => {
                i += 1;
                (0, None)
            }
            Some('+') => {
                i += 1;
                (1, None)
            }
            Some('?') => {
                i += 1;
                (0, Some(1))
            }
            _ => (1, Some(1)),
        };
        atoms.push(Atom::Rep { kind: atom, min, max });
    }
    Some(atoms)
}

fn match_seq(seq: &[Atom], text: &[char], pos: usize) -> bool {
    let mut steps = 0usize;
    match_seq_bounded(seq, text, pos, &mut steps)
}

fn match_seq_bounded(seq: &[Atom], text: &[char], pos: usize, steps: &mut usize) -> bool {
    *steps += 1;
    if *steps > 50_000 {
        return false;
    }
    let Some((first, rest)) = seq.split_first() else {
        return true;
    };
    match first {
        Atom::StartAnchor => pos == 0 && match_seq_bounded(rest, text, pos, steps),
        Atom::EndAnchor => pos == text.len() && match_seq_bounded(rest, text, pos, steps),
        Atom::Rep { kind, min, max } => {
            let mut limit = 0usize;
            while pos + limit < text.len()
                && max.is_none_or(|m| limit < m)
                && kind.matches(text[pos + limit])
            {
                limit += 1;
            }
            if limit < *min {
                return false;
            }
            for count in (*min..=limit).rev() {
                if match_seq_bounded(rest, text, pos + count, steps) {
                    return true;
                }
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-search-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn glob_skips_git_target_env_and_matches_nested_patterns() {
        let dir = tmpdir("glob");
        std::fs::create_dir_all(dir.join("src/sub")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/sub/util.rs"), "pub fn help() {}\n").unwrap();
        std::fs::write(dir.join("README.md"), "# doc\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.join("target/out.rs"), "ignored\n").unwrap();

        let rs = glob_files(dir.to_str().unwrap(), "**/*.rs", 50).await.unwrap();
        assert_eq!(rs, vec!["src/main.rs", "src/sub/util.rs"]);

        let brace = glob_files(dir.to_str().unwrap(), "*.{md,rs}", 50).await.unwrap();
        assert!(brace.contains(&"README.md".to_string()));
        assert!(!brace.iter().any(|p| p.contains(".env")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_matches_regex_alternation_and_include_filter() {
        let dir = tmpdir("grep");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub fn alpha() {}\npub fn beta() {}\nlet x = 42;\n",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "pub fn ignored_by_include() {}\n").unwrap();
        std::fs::write(dir.join(".env"), "API_KEY=alpha\n").unwrap();

        let hits = grep_files(
            dir.to_str().unwrap(),
            r"^pub\s+fn\s+\w+|42",
            Some("*.rs"),
            50,
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert_eq!(hits[0].path, "src/lib.rs");
        assert_eq!(hits[0].line, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
