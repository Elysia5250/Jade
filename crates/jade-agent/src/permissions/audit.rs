//! Structured JSONL permission audit log with secret redaction.

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize)]
pub struct PermissionAuditEntry<'a> {
    pub ts_ms: u64,
    pub session_id: &'a str,
    pub target: &'a str,
    pub mode: &'a str,
    pub tool: &'a str,
    pub detail: String,
    pub decision: &'a str,
    pub resolution: &'a str,
    pub reason: &'a str,
    pub sandbox: String,
}

pub fn audit_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent").join("permission_audit.jsonl")
}

pub fn record_decision(data_dir: &Path, mut entry: PermissionAuditEntry<'_>) -> anyhow::Result<()> {
    entry.detail = redact_secrets(&entry.detail);
    let path = audit_log_path(data_dir);
    if let Some(parent) = path.parent() {
        jade_core::fs::ensure_private_dir(parent)?;
    }
    let line = serde_json::to_string(&entry)?;
    jade_core::audit::append_line_with_rotate(&path, jade_core::audit::LOG_ROTATE_BYTES, &line)?;
    Ok(())
}

pub fn now_ms() -> u64 {
    jade_core::now_ms()
}

pub fn redact_secrets(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(512));
    let mut prev_was_bearer = false;
    for (idx, word) in raw.split_whitespace().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        if out.len() > 400 {
            out.push('…');
            break;
        }
        if prev_was_bearer {
            prev_was_bearer = false;
            let trailing_quote = word.chars().last().filter(|c| *c == '\'' || *c == '"');
            out.push_str("[REDACTED]");
            if let Some(q) = trailing_quote {
                out.push(q);
            }
            continue;
        }
        let bare = word.trim_matches(|c| c == '"' || c == '\'');
        if bare.eq_ignore_ascii_case("bearer") {
            prev_was_bearer = true;
            out.push_str(word);
        } else if is_secret_token(word) {
            out.push_str("[REDACTED]");
        } else if let Some((k, v)) = word.split_once('=') {
            let kl = k.to_ascii_lowercase();
            if kl.contains("key")
                || kl.contains("token")
                || kl.contains("secret")
                || kl.contains("pass")
                || is_secret_token(v)
            {
                out.push_str(k);
                out.push_str("=[REDACTED]");
            } else {
                out.push_str(word);
            }
        } else {
            out.push_str(word);
        }
    }
    out
}

fn is_secret_token(s: &str) -> bool {
    let s = s.trim_matches(|c| c == '"' || c == '\'');
    s.starts_with("sk-")
        || s.starts_with("sk_live_")
        || s.starts_with("ghp_")
        || s.starts_with("gho_")
        || s.starts_with("ghs_")
        || s.starts_with("xoxb-")
        || s.starts_with("xoxp-")
        || s.starts_with("AIza")
        || (s.len() > 28 && s.starts_with("AKIA"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_tokens_bearer_and_env_assignments() {
        let out = redact_secrets(
            "curl -H 'Authorization: Bearer eyJhbGciOiSecret' -H sk-secret123456 API_TOKEN=abc123 https://example.com",
        );
        assert!(!out.contains("eyJhbGciOiSecret"), "{out}");
        assert!(!out.contains("sk-secret123456"), "{out}");
        assert!(!out.contains("abc123"), "{out}");
        assert!(out.contains("Bearer [REDACTED]'"), "{out}");
        assert!(out.contains("https://example.com"), "{out}");
    }
}
