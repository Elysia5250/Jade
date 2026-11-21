//! Append-only JSONL audit log for agent invocations.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Rotate an append-only log once it reaches this size. One previous generation
/// is kept at `{path}.1`.
pub const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

fn rotate_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Rename `path` to `{path}.1` when it is a file of at least `limit` bytes.
/// Serialized in-process and uses atomic `rename` (replacing `.1` in place) so
/// concurrent writers never delete a freshly rotated generation.
pub fn rotate_if_oversized(path: &Path, limit: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if !meta.is_file() || meta.len() < limit {
        return;
    }
    let _guard = rotate_lock().lock().unwrap_or_else(|e| e.into_inner());
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if !meta.is_file() || meta.len() < limit {
        return;
    }
    let rotated = std::path::PathBuf::from(format!("{}.1", path.display()));
    let _ = std::fs::rename(path, rotated);
}

/// Atomically rotate `path` (if `>= limit`) and append `line` (with trailing
/// newline) under the same process-wide lock so concurrent writers cannot
/// append into an already-rotated `.1` inode.
pub fn append_line_with_rotate(path: &Path, limit: u64, line: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let _guard = rotate_lock().lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.is_file() && meta.len() >= limit {
            let rotated = std::path::PathBuf::from(format!("{}.1", path.display()));
            let _ = std::fs::rename(path, rotated);
        }
    }
    let mut f = crate::fs::open_append_private(path)?;
    writeln!(f, "{line}")
}

/// Append-only audit trail: one JSON line per invocation, whoever the caller.
pub fn audit(root: &Path, peer: &str, op: &str, detail: &str, ok: bool, ms: u64) {
    let dir = root.join("agent");
    if crate::fs::ensure_private_dir(&dir).is_err() {
        return;
    }
    let log_path = dir.join("audit.log");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = serde_json::json!({
        "ts": ts,
        "peer": peer,
        "op": op,
        "detail": detail,
        "ok": ok,
        "ms": ms,
    });
    let _ = append_line_with_rotate(&log_path, LOG_ROTATE_BYTES, &line.to_string());
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_chars_not_bytes() {
        assert_eq!(truncate("abc", 2), "ab…");
        assert_eq!(truncate("中文", 5), "中文");
    }

    #[test]
    fn rotates_oversized_log_to_dot_one() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jade-rotate-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.log");
        std::fs::write(&path, b"old-log").unwrap();
        rotate_if_oversized(&path, 4);
        let rotated = std::path::PathBuf::from(format!("{}.1", path.display()));
        assert!(!path.exists());
        assert_eq!(std::fs::read(&rotated).unwrap(), b"old-log");
        std::fs::write(&path, b"newer-log").unwrap();
        rotate_if_oversized(&path, 1);
        assert_eq!(std::fs::read(&rotated).unwrap(), b"newer-log");
        std::fs::write(&path, b"ok").unwrap();
        rotate_if_oversized(&path, 100);
        assert_eq!(std::fs::read(&path).unwrap(), b"ok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_rotate_preserves_rotated_generation() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jade-rotate-race-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.log");
        std::fs::write(&path, b"payload-1234567890").unwrap();

        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| rotate_if_oversized(&path, 4));
            }
        });
        let rotated = dir.join("audit.log.1");
        assert!(rotated.exists(), "rotated generation must not be deleted");
        assert_eq!(std::fs::read(&rotated).unwrap(), b"payload-1234567890");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_line_with_rotate_serializes_concurrent_writers() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jade-audit-append-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.log");

        std::thread::scope(|s| {
            for t in 0..4 {
                let p = &path;
                s.spawn(move || {
                    for i in 0..25 {
                        append_line_with_rotate(p, 512, &format!(r#"{{"t":{t},"i":{i}}}"#))
                            .unwrap();
                    }
                });
            }
        });
        // Both current and rotated files must contain only valid complete JSON lines.
        let mut total_lines = 0usize;
        for f in [&path, &dir.join("audit.log.1")] {
            if let Ok(raw) = std::fs::read_to_string(f) {
                for line in raw.lines() {
                    let _: serde_json::Value = serde_json::from_str(line).unwrap();
                    total_lines += 1;
                }
            }
        }
        assert!(total_lines > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
