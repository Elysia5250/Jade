//! Append-only JSONL audit log for agent invocations.

use std::path::Path;

/// Append-only audit trail: one JSON line per invocation, whoever the caller.
pub fn audit(root: &Path, peer: &str, op: &str, detail: &str, ok: bool, ms: u64) {
    let dir = root.join("agent");
    if crate::fs::ensure_private_dir(&dir).is_err() {
        return;
    }
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
    if let Ok(mut f) = crate::fs::open_append_private(&dir.join("audit.log")) {
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
}
