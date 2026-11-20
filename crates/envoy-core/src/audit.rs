//! Append-only JSONL audit log for agent invocations.

use std::path::Path;

/// Rotate an append-only log once it reaches this size. One previous generation
/// is kept at `{path}.1`.
pub const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Rename `path` to `{path}.1` when it is a file of at least `limit` bytes.
/// A previous `.1` is replaced. Missing files are left alone.
pub fn rotate_if_oversized(path: &Path, limit: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if !meta.is_file() || meta.len() < limit {
        return;
    }
    let rotated = std::path::PathBuf::from(format!("{}.1", path.display()));
    let _ = std::fs::remove_file(&rotated);
    let _ = std::fs::rename(path, rotated);
}

/// Append-only audit trail: one JSON line per invocation, whoever the caller.
pub fn audit(root: &Path, peer: &str, op: &str, detail: &str, ok: bool, ms: u64) {
    let dir = root.join("agent");
    if crate::fs::ensure_private_dir(&dir).is_err() {
        return;
    }
    let log_path = dir.join("audit.log");
    rotate_if_oversized(&log_path, LOG_ROTATE_BYTES);
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
    use std::io::Write as _;
    if let Ok(mut f) = crate::fs::open_append_private(&log_path) {
        let _ = writeln!(f, "{line}");
    }
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
        let dir = std::env::temp_dir().join(format!("envoy-rotate-{nanos}"));
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
}
