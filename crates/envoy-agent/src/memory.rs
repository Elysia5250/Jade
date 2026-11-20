//! Workspace notes in `<data_dir>/memory/<id>.md`.
//!
//! The welcome screen lists and toggles notes for the current workspace.
//! Enabled bodies are injected into the depth-0 environment note, capped at
//! 4000 characters. Notes without a `workspace` field stay on disk and are
//! not injected.

use std::path::{Path, PathBuf};

pub const MEMORY_CAP: usize = 4000;

#[derive(Debug, Clone)]
pub struct MemoryNote {
    pub id: String,
    pub title: String,
    pub enabled: bool,
    pub body: String,
    pub workspace: Option<String>,
    pub path: PathBuf,
}

fn memory_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("memory")
}

pub fn list(data_dir: &Path) -> Vec<MemoryNote> {
    let Ok(entries) = std::fs::read_dir(memory_dir(data_dir)) else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        notes.push(parse_note(&path, &raw));
    }
    notes.sort_by(|a, b| a.id.cmp(&b.id));
    notes
}

pub fn create(data_dir: &Path, cwd: &Path, title: &str) -> Result<MemoryNote, String> {
    let title = title.trim().replace(['\r', '\n'], " ");
    if title.is_empty() {
        return Err("title is empty".into());
    }
    let dir = memory_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|err| format!("cannot create: {err}"))?;
    let id = unique_id(&dir, &title);
    let path = dir.join(format!("{id}.md"));
    let workspace = workspace_key(cwd);
    write_note(&path, &title, true, Some(&workspace), "")?;
    Ok(MemoryNote {
        id,
        title,
        enabled: true,
        body: String::new(),
        workspace: Some(workspace),
        path,
    })
}

pub fn list_for(data_dir: &Path, cwd: &Path) -> Vec<MemoryNote> {
    let want = workspace_key(cwd);
    list(data_dir)
        .into_iter()
        .filter(|note| note.workspace.as_deref() == Some(want.as_str()))
        .collect()
}

pub fn set_enabled(note: &MemoryNote, enabled: bool) -> Result<(), String> {
    write_note(
        &note.path,
        &note.title,
        enabled,
        note.workspace.as_deref(),
        &note.body,
    )
}

pub fn delete(note: &MemoryNote) -> Result<(), String> {
    std::fs::remove_file(&note.path).map_err(|err| format!("cannot delete: {err}"))
}

/// Enabled notes for this workspace, for the depth-0 environment block.
/// `None` when nothing is on. The 4000-character cap applies to the notes
/// body; a truncation marker may extend past that cap.
pub fn prompt_block(data_dir: &Path, cwd: &Path) -> Option<String> {
    let mut body = String::new();
    let mut truncated = false;
    for note in list_for(data_dir, cwd)
        .into_iter()
        .filter(|note| note.enabled)
    {
        let used = body.chars().count();
        if used >= MEMORY_CAP {
            truncated = true;
            break;
        }
        let chunk = if note.body.is_empty() {
            format!("### {}\n", note.title)
        } else {
            format!("### {}\n{}\n", note.title, note.body)
        };
        let room = MEMORY_CAP - used;
        if chunk.chars().count() <= room {
            body.push_str(&chunk);
        } else {
            body.extend(chunk.chars().take(room));
            truncated = true;
            break;
        }
    }
    if truncated {
        body.push_str("\n…(truncated)");
    }
    if body.is_empty() {
        None
    } else {
        Some(format!("## Long-term memory\n{body}"))
    }
}

fn workspace_key(cwd: &Path) -> String {
    std::fs::canonicalize(cwd)
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string()
        .replace(['\r', '\n'], "")
}

fn unique_id(dir: &Path, title: &str) -> String {
    let base = note_id(title);
    let mut id = base.clone();
    let mut n = 0u32;
    while dir.join(format!("{id}.md")).exists() {
        n += 1;
        id = format!("{base}-{n}");
    }
    id
}

fn note_id(title: &str) -> String {
    let slug: String = title
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .take(40)
        .collect();
    let millis = envoy_core::now_ms();
    if slug.is_empty() {
        format!("note-{millis}")
    } else {
        format!("{slug}-{millis}")
    }
}

fn write_note(
    path: &Path,
    title: &str,
    enabled: bool,
    workspace: Option<&str>,
    body: &str,
) -> Result<(), String> {
    let workspace_line = match workspace {
        Some(workspace) if !workspace.is_empty() => format!("workspace: {workspace}\n"),
        _ => String::new(),
    };
    let text = format!(
        "---\ntitle: {title}\nenabled: {}\n{workspace_line}---\n{body}",
        if enabled { "true" } else { "false" }
    );
    envoy_core::fs::write_atomic_sync(path, text.as_bytes(), 0o600)
        .map_err(|err| format!("cannot create: {err:#}"))
}

fn parse_note(path: &Path, raw: &str) -> MemoryNote {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("note")
        .to_string();
    let (title, enabled, workspace, body) = split_note(raw, &id);
    MemoryNote {
        id,
        title,
        enabled,
        body,
        workspace,
        path: path.to_path_buf(),
    }
}

fn split_note(raw: &str, fallback_title: &str) -> (String, bool, Option<String>, String) {
    let (Some(fm), body) = crate::frontmatter::split_frontmatter(raw) else {
        return (
            fallback_title.to_string(),
            true,
            None,
            raw.trim().to_string(),
        );
    };
    let mut title = fallback_title.to_string();
    let mut enabled = true;
    let mut workspace = None;
    for line in fm.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, val)) = trimmed.split_once(':') else {
            continue;
        };
        match key.trim() {
            "title" => {
                let value = crate::frontmatter::unquote(val);
                if !value.is_empty() {
                    title = value;
                }
            }
            "enabled" => {
                enabled = !crate::frontmatter::unquote(val).eq_ignore_ascii_case("false");
            }
            "workspace" => {
                let value = crate::frontmatter::unquote(val);
                if !value.is_empty() {
                    workspace = Some(value);
                }
            }
            _ => {}
        }
    }
    (title, enabled, workspace, body.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "envoy-mem-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn chinese_title_keeps_its_text_and_a_unique_id() {
        let dir = tmpdir();
        let note = create(&dir, &dir, "记事").unwrap();
        assert!(note.id.starts_with("note-"), "{}", note.id);
        assert_ne!(note.id, "custom");
        let raw = std::fs::read_to_string(&note.path).unwrap();
        assert!(raw.contains("title: 记事"));
        assert!(raw.contains("enabled: true"));
        let listed = list(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "记事");
        assert!(listed[0].enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ascii_title_slugs_and_empty_title_is_refused() {
        let dir = tmpdir();
        assert!(create(&dir, &dir, "  ").unwrap_err().contains("empty"));
        let note = create(&dir, &dir, "Ship Notes!").unwrap();
        assert!(note.id.starts_with("shipnotes-"), "{}", note.id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_and_delete_round_trip() {
        let dir = tmpdir();
        let note = create(&dir, &dir, "alpha").unwrap();
        set_enabled(&note, false).unwrap();
        let listed = list(&dir);
        assert!(!listed[0].enabled);
        delete(&listed[0]).unwrap();
        assert!(list(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_block_skips_disabled_notes_and_stops_on_a_char_boundary() {
        let dir = tmpdir();
        let big = create(&dir, &dir, "big").unwrap();
        let huge = "字".repeat(3000);
        let ws = workspace_key(&dir);
        std::fs::write(
            &big.path,
            format!("---\ntitle: big\nenabled: true\nworkspace: {ws}\n---\n{huge}"),
        )
        .unwrap();
        let hidden = create(&dir, &dir, "hidden").unwrap();
        set_enabled(&hidden, false).unwrap();
        let block = prompt_block(&dir, &dir).unwrap();
        assert!(block.starts_with("## Long-term memory\n"));
        assert!(!block.contains("hidden"));
        assert!(block.contains("big"));
        let notes = block.find("### big").unwrap();
        assert!(block[notes..].chars().count() <= MEMORY_CAP);
        assert!(!block.contains("(truncated)"));
        assert!(std::str::from_utf8(block.as_bytes()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_is_injected_only_at_depth_zero() {
        let dir = tmpdir();
        create(&dir, &dir, "envoy-depth-marker").unwrap();
        let top = crate::agent::prompt::environment_note_for_depth(&dir, &dir, "sess", 0);
        let top = top.expect("depth 0 note");
        assert!(top.contains("## Long-term memory"));
        assert!(top.contains("envoy-depth-marker"));
        let skills_at = top.find("Available skills");
        let memory_at = top.find("Long-term memory").unwrap();
        if let Some(skills_at) = skills_at {
            assert!(skills_at < memory_at);
        }
        let agents_at = top.find("Available subagents");
        if let Some(agents_at) = agents_at {
            assert!(memory_at < agents_at);
        }
        let deep = crate::agent::prompt::environment_note_for_depth(&dir, &dir, "sess", 1);
        if let Some(text) = deep {
            assert!(!text.contains("Long-term memory"));
            assert!(!text.contains("envoy-depth-marker"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_note_parses_eof_delimiter_and_inline_comment() {
        let (title, enabled, workspace, body) =
            split_note("---\ntitle: \"Alpha # 1\" # keep\nenabled: false # off\n---", "fb");
        assert_eq!(title, "Alpha # 1");
        assert!(!enabled);
        assert!(workspace.is_none());
        assert_eq!(body, "");
    }

    #[test]
    fn prompt_block_ignores_notes_from_another_workspace() {
        let dir = tmpdir();
        let other = tmpdir();
        create(&dir, &other, "elsewhere").unwrap();
        create(&dir, &dir, "here").unwrap();
        let block = prompt_block(&dir, &dir).unwrap();
        assert!(block.contains("here"));
        assert!(!block.contains("elsewhere"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn chinese_memory_truncates_on_a_char_boundary_with_a_marker() {
        let dir = tmpdir();
        let note = create(&dir, &dir, "long").unwrap();
        let huge = "字".repeat(5000);
        let ws = workspace_key(&dir);
        std::fs::write(
            &note.path,
            format!("---\ntitle: long\nenabled: true\nworkspace: {ws}\n---\n{huge}"),
        )
        .unwrap();
        let block = prompt_block(&dir, &dir).unwrap();
        assert!(block.contains("\n…(truncated)"));
        assert!(std::str::from_utf8(block.as_bytes()).is_ok());
        let notes = block.find("### long").unwrap();
        let body = block[notes..].strip_suffix("\n…(truncated)").unwrap();
        assert_eq!(body.chars().count(), MEMORY_CAP);
        assert!(body.chars().all(|ch| ch != '\u{FFFD}'));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
