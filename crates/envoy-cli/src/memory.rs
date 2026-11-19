//! Cross-workspace notes in `<data_dir>/memory/<id>.md`.
//!
//! The welcome screen lists and toggles these. Enabled bodies are injected
//! into the depth-0 environment note, capped near 4000 characters.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MEMORY_CAP: usize = 4000;

#[derive(Debug, Clone)]
pub struct MemoryNote {
    pub id: String,
    pub title: String,
    pub enabled: bool,
    pub body: String,
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

pub fn create(data_dir: &Path, title: &str) -> Result<MemoryNote, String> {
    let title = title.trim().replace(['\r', '\n'], " ");
    if title.is_empty() {
        return Err("title is empty".into());
    }
    let dir = memory_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|err| format!("cannot create: {err}"))?;
    let id = unique_id(&dir, &title);
    let path = dir.join(format!("{id}.md"));
    write_note(&path, &title, true, "")?;
    Ok(MemoryNote {
        id,
        title,
        enabled: true,
        body: String::new(),
        path,
    })
}

pub fn set_enabled(note: &MemoryNote, enabled: bool) -> Result<(), String> {
    write_note(&note.path, &note.title, enabled, &note.body)
}

pub fn delete(note: &MemoryNote) -> Result<(), String> {
    std::fs::remove_file(&note.path).map_err(|err| format!("cannot delete: {err}"))
}

/// Enabled notes for the depth-0 environment block. `None` when nothing is on.
pub fn prompt_block(data_dir: &Path) -> Option<String> {
    let mut body = String::new();
    for note in list(data_dir).into_iter().filter(|note| note.enabled) {
        if body.len() >= MEMORY_CAP {
            break;
        }
        let chunk = if note.body.is_empty() {
            format!("### {}\n", note.title)
        } else {
            format!("### {}\n{}\n", note.title, note.body)
        };
        let room = MEMORY_CAP - body.len();
        if chunk.len() <= room {
            body.push_str(&chunk);
        } else {
            let mut end = room.min(chunk.len());
            while end > 0 && !chunk.is_char_boundary(end) {
                end -= 1;
            }
            body.push_str(&chunk[..end]);
            break;
        }
    }
    if body.is_empty() {
        None
    } else {
        Some(format!("## Long-term memory\n{body}"))
    }
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
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if slug.is_empty() {
        format!("note-{millis}")
    } else {
        format!("{slug}-{millis}")
    }
}

fn write_note(path: &Path, title: &str, enabled: bool, body: &str) -> Result<(), String> {
    let text = format!(
        "---\ntitle: {title}\nenabled: {}\n---\n{body}",
        if enabled { "true" } else { "false" }
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("cannot create: {err}"))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| format!("cannot create: {err}"))?;
    file.write_all(text.as_bytes())
        .map_err(|err| format!("cannot create: {err}"))?;
    Ok(())
}

fn parse_note(path: &Path, raw: &str) -> MemoryNote {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("note")
        .to_string();
    let (title, enabled, body) = split_note(raw, &id);
    MemoryNote {
        id,
        title,
        enabled,
        body,
        path: path.to_path_buf(),
    }
}

fn split_note(raw: &str, fallback_title: &str) -> (String, bool, String) {
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"));
    let Some(rest) = rest else {
        return (fallback_title.to_string(), true, raw.trim().to_string());
    };
    let (fm, body) = if let Some(i) = rest.find("\n---\n") {
        (&rest[..i], &rest[i + 5..])
    } else if let Some(i) = rest.find("\n---\r\n") {
        (&rest[..i], &rest[i + 6..])
    } else if let Some(i) = rest.find("\r\n---\r\n") {
        (&rest[..i], &rest[i + 7..])
    } else {
        return (fallback_title.to_string(), true, raw.trim().to_string());
    };
    let mut title = fallback_title.to_string();
    let mut enabled = true;
    for line in fm.lines() {
        if let Some(value) = line.strip_prefix("title:") {
            let value = value.trim();
            if !value.is_empty() {
                title = value.to_string();
            }
        } else if let Some(value) = line.strip_prefix("enabled:") {
            enabled = !value.trim().eq_ignore_ascii_case("false");
        }
    }
    (title, enabled, body.trim().to_string())
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
        let note = create(&dir, "记事").unwrap();
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
        assert!(create(&dir, "  ").unwrap_err().contains("empty"));
        let note = create(&dir, "Ship Notes!").unwrap();
        assert!(note.id.starts_with("shipnotes-"), "{}", note.id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_and_delete_round_trip() {
        let dir = tmpdir();
        let note = create(&dir, "alpha").unwrap();
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
        let big = create(&dir, "big").unwrap();
        let huge = "字".repeat(3000);
        std::fs::write(
            &big.path,
            format!("---\ntitle: big\nenabled: true\n---\n{huge}"),
        )
        .unwrap();
        let hidden = create(&dir, "hidden").unwrap();
        set_enabled(&hidden, false).unwrap();
        let block = prompt_block(&dir).unwrap();
        assert!(block.starts_with("## Long-term memory\n"));
        assert!(!block.contains("hidden"));
        assert!(block.contains("big"));
        let notes = block.find("### big").unwrap();
        assert!(block[notes..].len() <= MEMORY_CAP + 16);
        assert!(std::str::from_utf8(block.as_bytes()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_is_injected_only_at_depth_zero() {
        let dir = tmpdir();
        create(&dir, "envoy-depth-marker").unwrap();
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
}
