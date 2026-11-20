//! Recursive workspace `glob` and `grep` primitives.
//!
//! Backed by `regex`, `globset`, and `ignore::WalkBuilder`. Skips
//! VCS/build/protected directories (`.git`, `target`, `node_modules`, `.ssh`,
//! `.aws`, `.envoy`, `.conduit`, `.gnupg`, `.kube`, `.codex`, `.agents`), avoids
//! symlink escapes, honors repo `.gitignore`, ignores binary files and `.env*`
//! secret files, and bounds traversal and output volume.

use anyhow::{bail, Context, Result};
use envoy_proto::GrepHit;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use std::ops::ControlFlow;
use std::path::{Component, Path, PathBuf};

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".ssh",
    ".aws",
    ".envoy",
    ".conduit",
    ".gnupg",
    ".kube",
    ".codex",
    ".agents",
];
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LINE_CHARS: usize = 512;
const DEFAULT_MAX_RESULTS: usize = 200;
const MAX_WALK_ENTRIES: usize = 50_000;
const MAX_BRACE_GROUPS: usize = 8;
const MAX_BRACE_COMBINATIONS: usize = 64;
const REGEX_SIZE_LIMIT: usize = 2 * 1024 * 1024;

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
    tokio::task::spawn_blocking(move || grep_files_sync(&root, &pattern, include.as_deref(), cap))
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

fn is_protected_search_root(canon: &Path) -> bool {
    let raw = canon.to_string_lossy();
    if matches!(raw.as_ref(), "/etc/shadow" | "/etc/gshadow" | "/etc/sudoers") {
        return true;
    }
    for comp in canon.components() {
        let Component::Normal(os) = comp else {
            continue;
        };
        let name = os.to_string_lossy();
        if matches!(
            name.as_ref(),
            ".git"
                | ".ssh"
                | ".aws"
                | ".envoy"
                | ".conduit"
                | ".gnupg"
                | ".kube"
                | ".codex"
                | ".agents"
        ) {
            return true;
        }
    }
    crate::sandbox::user_secret_paths()
        .iter()
        .any(|secret| canon == secret || canon.starts_with(secret))
}

fn resolve_root(root: &str) -> Result<PathBuf> {
    let raw = if root.trim().is_empty() {
        "."
    } else {
        root.trim()
    };
    let resolved = crate::fs::resolve(raw);
    let canon = std::fs::canonicalize(&resolved)
        .with_context(|| format!("search root {}", resolved.display()))?;
    if is_protected_search_root(&canon) {
        bail!("refusing to search protected path {}", canon.display());
    }
    if !canon.is_dir() {
        bail!("{} is not a directory", canon.display());
    }
    Ok(canon)
}

/// Reject nested or combinatorial `{a,b}` expansions before `globset` expands them.
fn validate_brace_budget(pattern: &str) -> Result<()> {
    let mut escaped = false;
    let mut in_class = false;
    let mut in_brace = false;
    let mut groups = 0usize;
    let mut combos = 1usize;
    let mut alts = 1usize;

    for ch in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '[' && !in_class {
            in_class = true;
            continue;
        }
        if ch == ']' && in_class {
            in_class = false;
            continue;
        }
        if in_class {
            continue;
        }
        if ch == '{' {
            if in_brace {
                bail!("nested brace expansion is not supported in glob pattern");
            }
            groups += 1;
            if groups > MAX_BRACE_GROUPS {
                bail!("too many brace groups in glob pattern (max {MAX_BRACE_GROUPS})");
            }
            in_brace = true;
            alts = 1;
        } else if ch == ',' && in_brace {
            alts += 1;
        } else if ch == '}' && in_brace {
            in_brace = false;
            combos = combos
                .checked_mul(alts)
                .filter(|&n| n <= MAX_BRACE_COMBINATIONS)
                .context("glob brace expansion exceeds combination budget")?;
        }
    }
    Ok(())
}

fn compile_glob(pattern: &str) -> Result<GlobSet> {
    validate_brace_budget(pattern)?;
    let pat = pattern.trim_start_matches("./");
    let mut builder = GlobSetBuilder::new();
    let primary = GlobBuilder::new(pat)
        .literal_separator(true)
        .build()
        .with_context(|| format!("invalid glob pattern `{pattern}`"))?;
    builder.add(primary);
    // Single-segment patterns like `*.rs` match any nested basename too.
    if !pat.contains('/') {
        let nested = GlobBuilder::new(&format!("**/{pat}"))
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid glob pattern `{pattern}`"))?;
        builder.add(nested);
    }
    builder
        .build()
        .with_context(|| format!("building glob set `{pattern}`"))
}

fn walk_files_bounded(
    root: &Path,
    max_entries: usize,
    mut visit: impl FnMut(String, PathBuf) -> ControlFlow<()>,
) {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .ignore(false)
        .parents(true)
        .require_git(true)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let Some(ft) = entry.file_type() else {
                return false;
            };
            // Never traverse symlinked directories or follow escapes.
            if ft.is_symlink() {
                return false;
            }
            let name = entry.file_name().to_string_lossy();
            if ft.is_dir() {
                return !SKIP_DIRS.contains(&name.as_ref());
            }
            !crate::sandbox::is_env_filename(&name)
        });

    let mut visited = 0usize;
    for item in builder.build() {
        let Ok(entry) = item else {
            visited += 1;
            if visited >= max_entries {
                break;
            }
            continue;
        };
        if entry.depth() == 0 {
            continue;
        }
        visited += 1;
        if visited > max_entries {
            break;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if let Ok(rel) = path.strip_prefix(root) {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if visit(rel_str, path).is_break() {
                break;
            }
        }
    }
}

fn glob_files_sync(root: &str, pattern: &str, cap: usize) -> Result<Vec<String>> {
    let root_path = resolve_root(root)?;
    let pat = if pattern.trim().is_empty() {
        "**/*"
    } else {
        pattern.trim()
    };
    let matcher = compile_glob(pat)?;
    let mut out = Vec::new();
    walk_files_bounded(&root_path, MAX_WALK_ENTRIES, |rel, _| {
        if matcher.is_match(&rel) {
            out.push(rel);
            if out.len() >= cap {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    });
    Ok(out)
}

fn grep_files_sync(
    root: &str,
    pattern: &str,
    include: Option<&str>,
    cap: usize,
) -> Result<Vec<GrepHit>> {
    if pattern.is_empty() {
        bail!("grep `pattern` must not be empty");
    }
    let root_path = resolve_root(root)?;
    let matcher = RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .context("invalid grep regex pattern")?;
    let include_matcher = include
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(compile_glob)
        .transpose()?;

    let mut hits = Vec::new();
    walk_files_bounded(&root_path, MAX_WALK_ENTRIES, |rel, full| {
        if let Some(inc) = &include_matcher {
            if !inc.is_match(&rel) {
                return ControlFlow::Continue(());
            }
        }
        grep_file(&full, &rel, &matcher, &mut hits, cap)
    });
    Ok(hits)
}

fn grep_file(
    full: &Path,
    rel: &str,
    matcher: &regex::Regex,
    hits: &mut Vec<GrepHit>,
    cap: usize,
) -> ControlFlow<()> {
    let Ok(meta) = std::fs::metadata(full) else {
        return ControlFlow::Continue(());
    };
    if meta.len() > MAX_FILE_BYTES {
        return ControlFlow::Continue(());
    }
    let Ok(bytes) = std::fs::read(full) else {
        return ControlFlow::Continue(());
    };
    if bytes.iter().take(4096).any(|&b| b == 0) {
        return ControlFlow::Continue(());
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return ControlFlow::Continue(());
    };
    for (idx, line) in text.lines().enumerate() {
        if matcher.is_match(line) {
            let snippet: String = line.chars().take(MAX_LINE_CHARS).collect();
            hits.push(GrepHit {
                path: rel.to_string(),
                line: (idx + 1) as u32,
                text: snippet,
            });
            if hits.len() >= cap {
                return ControlFlow::Break(());
            }
        }
    }
    ControlFlow::Continue(())
}

/// Match a relative POSIX path against a glob pattern (`*`, `?`, `**`, `{a,b}`).
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let Ok(matcher) = compile_glob(pattern) else {
        return false;
    };
    matcher.is_match(path.trim_start_matches("./"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-search-{tag}-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
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
        std::fs::write(dir.join(".ENV.local"), "SECRET=2\n").unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.join("target/out.rs"), "ignored\n").unwrap();

        let rs = glob_files(dir.to_str().unwrap(), "**/*.rs", 50)
            .await
            .unwrap();
        assert_eq!(rs, vec!["src/main.rs", "src/sub/util.rs"]);

        let brace = glob_files(dir.to_str().unwrap(), "*.{md,rs}", 50)
            .await
            .unwrap();
        assert!(brace.contains(&"README.md".to_string()));
        assert!(!brace
            .iter()
            .any(|p| p.to_ascii_lowercase().contains(".env")));

        let blocked = glob_files(dir.join(".git").to_str().unwrap(), "*", 10).await;
        assert!(blocked.is_err());
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

    #[tokio::test]
    async fn glob_brace_bomb_rejects_fast_without_oom() {
        let dir = tmpdir("brace-bomb");
        std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
        let bomb = "{a,b}".repeat(30);
        assert!(!glob_match(&bomb, "aaa"));
        let err = glob_files(dir.to_str().unwrap(), &bomb, 10)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("brace"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_supports_parenthesized_alternation_and_groups() {
        let dir = tmpdir("groups");
        std::fs::write(dir.join("sample.txt"), "foobaz\nbarbaz\nfooqux\n").unwrap();
        let hits = grep_files(dir.to_str().unwrap(), r"^(foo|bar)baz$", None, 20)
            .await
            .unwrap();
        let texts: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, vec!["foobaz", "barbaz"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn walk_files_bounded_stops_early_and_skips_symlinks() {
        let dir = tmpdir("bounded");
        for i in 0..45 {
            std::fs::write(dir.join(format!("file-{i:02}.txt")), "x\n").unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("file-00.txt"), dir.join("sym-file.txt")).unwrap();
            let outside = tmpdir("outside");
            std::fs::write(outside.join("secret.txt"), "nope\n").unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("sym-dir")).unwrap();
            let all = glob_files(dir.to_str().unwrap(), "**/*", 100).await.unwrap();
            assert!(!all.iter().any(|p| p.contains("sym-")), "{all:?}");
            let _ = std::fs::remove_dir_all(&outside);
        }

        let mut seen = 0usize;
        walk_files_bounded(&dir, 10, |_, _| {
            seen += 1;
            ControlFlow::Continue(())
        });
        assert!(seen <= 10, "expected <= 10 visited entries, got {seen}");

        let capped = glob_files(dir.to_str().unwrap(), "*.txt", 5).await.unwrap();
        assert_eq!(capped.len(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
