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

/// Largest regular file `edit_file` will read into memory.
const MAX_EDIT_BYTES: u64 = 16 * 1024 * 1024;

/// Replace an exact snippet in a UTF-8 text file, atomically.
///
/// Refuses a non-unique match unless `replace_all` is set, so a stale snippet
/// cannot silently rewrite several places. Preserves the file's mode.
/// Refuses symlinks, directories, devices, and files over [`MAX_EDIT_BYTES`].
pub async fn edit_file(path: &str, old: &str, new: &str, replace_all: bool) -> Result<usize> {
    use tokio::io::AsyncReadExt as _;
    if old.is_empty() {
        anyhow::bail!("`old` must not be empty");
    }
    let path = resolve(path);
    let mut opts = tokio::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file = opts.open(&path).await.map_err(|err| {
        #[cfg(unix)]
        if err.raw_os_error() == Some(libc::ELOOP) {
            return anyhow::anyhow!(
                "{} is not a regular file (refusing symlink, device, or directory)",
                path.display()
            );
        }
        anyhow::Error::new(err).context(format!("open {}", path.display()))
    })?;
    let meta = file
        .metadata()
        .await
        .with_context(|| format!("stat {}", path.display()))?;
    if !meta.file_type().is_file() {
        anyhow::bail!(
            "{} is not a regular file (refusing symlink, device, or directory)",
            path.display()
        );
    }
    if meta.len() > MAX_EDIT_BYTES {
        anyhow::bail!(
            "{} is {} bytes; edit refuses files over {MAX_EDIT_BYTES}",
            path.display(),
            meta.len()
        );
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut bytes)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    drop(file);
    let text = String::from_utf8(bytes).map_err(|_| {
        anyhow::anyhow!(
            "{} is not UTF-8 text; use `write` for binary files",
            path.display()
        )
    })?;

    let matches = text.matches(old).count();
    if matches == 0 {
        anyhow::bail!("`old` not found in {}", path.display());
    }
    if matches > 1 && !replace_all {
        anyhow::bail!(
            "`old` matches {matches} times in {}; include more context or set replace_all",
            path.display()
        );
    }
    let updated = if replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    write_atomic(&path, updated.as_bytes(), Some(mode_of(&meta))).await?;
    Ok(if replace_all { matches } else { 1 })
}

/// Write through a directory-fd pinned sibling `.part` and `renameat` into place.
pub async fn write_atomic(path: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
    let path = path.to_path_buf();
    let data = data.to_vec();
    tokio::task::spawn_blocking(move || {
        crate::fs_atomic::write_atomic_pinned(&path, &data, mode, false, false)
    })
    .await
    .map_err(|e| anyhow::anyhow!("atomic write task failed: {e}"))?
}

/// Create a directory tree restricted to owner-only (`0700` on Unix).
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .with_context(|| format!("creating private directory {}", path.display()))?;
        set_mode_strict(path, 0o700)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
            .with_context(|| format!("creating directory {}", path.display()))?;
    }
    Ok(())
}

/// Atomically write `data` to `path` via a directory-fd pinned sibling temp file
/// with explicit `fchmod` mode and `fsync`.
pub fn write_atomic_sync(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    crate::fs_atomic::write_atomic_pinned(path, data, Some(mode), true, true)
}

/// Open an append-only file restricted to owner-only (`0600` on Unix).
pub fn open_append_private(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    set_mode(path, 0o600);
    Ok(file)
}

/// Apply permission bits strictly, returning an error if `chmod` fails.
pub fn set_mode_strict(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting mode {:o} on {}", mode, path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// Disable ptrace/core-dump inspection of the current daemon process where supported.
pub fn harden_process_dumpable() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
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

/// Host name from `gethostname(2)`, with `HOSTNAME` / `COMPUTERNAME` as fallback.
///
/// Does not read `/etc/hostname`, which is absent on macOS and can disagree
/// with the kernel's nodename.
pub fn system_hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is a writable buffer and `len` is its size.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            if let Ok(name) = std::str::from_utf8(&buf[..end]) {
                let name = name.trim();
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default()
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
    async fn edit_replaces_one_snippet_and_preserves_mode() {
        let dir = tmpdir("edit");
        let path = dir.join("code.rs");
        std::fs::write(&path, b"fn a() {}\nfn b() {}\n").unwrap();
        set_mode(&path, 0o640);

        let n = edit_file(path.to_str().unwrap(), "fn a()", "fn alpha()", false)
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn alpha() {}\nfn b() {}\n"
        );
        assert_eq!(mode_of(&std::fs::metadata(&path).unwrap()) & 0o777, 0o640);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_refuses_ambiguity_and_missing_snippets() {
        let dir = tmpdir("edit-err");
        let path = dir.join("dup.txt");
        std::fs::write(&path, b"x\nx\n").unwrap();
        let err = edit_file(path.to_str().unwrap(), "x", "y", false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches 2 times"), "{err}");
        assert_eq!(
            edit_file(path.to_str().unwrap(), "x", "y", true)
                .await
                .unwrap(),
            2
        );
        let err = edit_file(path.to_str().unwrap(), "nope", "y", false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_rejects_symlink_fifo_and_oversized_file() {
        let dir = tmpdir("edit-special");
        let real = dir.join("real.txt");
        std::fs::write(&real, b"hello\n").unwrap();
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = edit_file(link.to_str().unwrap(), "hello", "bye", false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a regular file"), "{err}");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "hello\n");

        let fifo = dir.join("pipe");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let err = edit_file(fifo.to_str().unwrap(), "a", "b", false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a regular file"), "{err}");

        let huge = dir.join("huge.txt");
        let file = std::fs::File::create(&huge).unwrap();
        file.set_len(MAX_EDIT_BYTES + 1).unwrap();
        let err = edit_file(huge.to_str().unwrap(), "a", "b", false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("refuses files over"), "{err}");
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
