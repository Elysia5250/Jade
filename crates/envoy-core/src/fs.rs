//! Local filesystem primitives: range read, one-level listing, atomic write.

use anyhow::{Context, Result};
use envoy_proto::{Entry, EntryKind, READ_LIMIT};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Directory listing ceiling (a runaway `/` should not fill a frame).
const MAX_ENTRIES: usize = 1000;

/// Read at most `limit` bytes at `offset`; returns (whole-file size, mode,
/// bytes read).
pub async fn read_range(path: &str, offset: u64, limit: u64) -> Result<(u64, u32, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let path = resolve(path);
    let mut f = tokio::fs::File::open(&path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let meta = f.metadata().await?;
    let limit = if limit == 0 {
        READ_LIMIT
    } else {
        limit.min(READ_LIMIT)
    };
    f.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut data = Vec::new();
    f.take(limit).read_to_end(&mut data).await?;
    Ok((meta.len(), mode_of(&meta), data))
}

/// One directory level, sorted by name, capped at 1000 entries.
pub async fn list_dir(path: &str) -> Result<Vec<Entry>> {
    let path = resolve(path);
    let mut rd = tokio::fs::read_dir(&path)
        .await
        .with_context(|| format!("list {}", path.display()))?;
    let mut entries = Vec::new();
    while let Some(e) = rd.next_entry().await? {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        let name = e.file_name().to_string_lossy().to_string();
        let Ok(meta) = e.metadata().await else {
            continue;
        };
        let ft = meta.file_type();
        let kind = if ft.is_symlink() {
            EntryKind::Symlink
        } else if ft.is_dir() {
            EntryKind::Dir
        } else if ft.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        entries.push(Entry {
            name,
            kind,
            size: meta.len(),
            mode: mode_of(&meta),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// Write a file through a sibling `.part` and rename it into place.
pub async fn write_file(path: &str, data: &[u8], mode: Option<u32>) -> Result<u64> {
    let path = resolve(path);
    write_atomic(&path, data, mode).await?;
    Ok(data.len() as u64)
}

/// Write through a sibling `.part` and rename into place.
pub async fn write_atomic(path: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
    }
    let part = part_path(path);
    tokio::fs::write(&part, data)
        .await
        .with_context(|| format!("write {}", part.display()))?;
    if let Some(m) = mode {
        set_mode(&part, m);
    }
    tokio::fs::rename(&part, path)
        .await
        .with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// Resolve `path` the way a shell would: relative names land in `$HOME`.
pub fn resolve(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        PathBuf::from(home_dir()).join(p)
    }
}

/// Home directory used for relative paths and as the default cwd.
pub fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
}

/// Sibling temp file for a write. The counter makes concurrent writers to the
/// same destination distinct, even inside one process.
pub fn part_path(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.part-{}-{seq}", std::process::id()))
}

/// Parse an octal permission string: `644`, `0755`, `0o755`.
pub fn parse_mode(text: &str) -> Result<u32> {
    let digits = text.trim().strip_prefix("0o").unwrap_or(text.trim());
    u32::from_str_radix(digits, 8)
        .with_context(|| format!("mode `{text}` is not octal (e.g. 644 or 0755)"))
}

/// Permission bits of a file, `0o644` where the platform has no mode.
pub fn mode_of(meta: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o644
    }
}

/// Apply permission bits; failures are best effort on filesystems without modes.
pub fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
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
        let p = std::env::temp_dir().join(format!("envoy-fs-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn resolve_sends_relative_paths_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(resolve("x/y.txt"), PathBuf::from(home).join("x/y.txt"));
        assert_eq!(resolve("/tmp/a"), PathBuf::from("/tmp/a"));
    }

    #[test]
    fn part_paths_are_unique_siblings() {
        let p = Path::new("/tmp/dir/data.bin");
        let a = part_path(p);
        let b = part_path(p);
        assert_eq!(a.parent().unwrap(), Path::new("/tmp/dir"));
        assert_ne!(a, b, "concurrent writers must not share a .part file");
        let name = a.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with(".data.bin.part-"), "{name}");
    }

    #[tokio::test]
    async fn read_range_honours_offset_and_limit() {
        let dir = tmpdir("read");
        let path = dir.join("data.txt");
        std::fs::write(&path, b"0123456789").unwrap();
        let (size, _, data) = read_range(path.to_str().unwrap(), 2, 4).await.unwrap();
        assert_eq!(size, 10);
        assert_eq!(data, b"2345");
        let (_, _, all) = read_range(path.to_str().unwrap(), 0, 0).await.unwrap();
        assert_eq!(all, b"0123456789");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_file_commits_and_preserves_mode() {
        let dir = tmpdir("write");
        let path = dir.join("bin/tool.sh");
        let n = write_file(path.to_str().unwrap(), b"#!/bin/sh\n", Some(0o755))
            .await
            .unwrap();
        assert_eq!(n, 10);
        assert_eq!(std::fs::read(&path).unwrap(), b"#!/bin/sh\n");
        let mode = mode_of(&std::fs::metadata(&path).unwrap());
        assert_eq!(mode & 0o111, 0o111, "executable bits lost: {mode:o}");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".part-"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_dir_sorts_and_tags_kinds() {
        let dir = tmpdir("list");
        std::fs::write(dir.join("b.txt"), b"x").unwrap();
        std::fs::write(dir.join("a.txt"), b"yy").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let entries = list_dir(dir.to_str().unwrap()).await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "sub"]);
        assert_eq!(entries[0].size, 2);
        assert!(matches!(entries[2].kind, EntryKind::Dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
