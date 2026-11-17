//! Structured JSONL permission audit log with secret redaction.

use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub sandbox: &'a str,
}

pub fn audit_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent").join("permission_audit.jsonl")
}

pub fn record_decision(data_dir: &Path, mut entry: PermissionAuditEntry<'_>) {
    entry.detail = redact_secrets(&entry.detail);
    let path = audit_log_path(data_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{line}");
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn redact_secrets(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(512));
    for (idx, word) in raw.split_whitespace().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        if out.len() > 400 {
            out.push('…');
            break;
        }
        if is_secret_token(word) {
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
        || s.starts_with("ghp_")
        || s.starts_with("gho_")
        || s.starts_with("Bearer ")
        || (s.len() > 28 && s.starts_with("AKIA"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_tokens_and_env_assignments() {
        let out = redact_secrets("curl -H sk-secret123456 API_TOKEN=abc123 https://example.com");
        assert!(!out.contains("sk-secret123456"));
        assert!(!out.contains("abc123"));
        assert!(out.contains("[REDACTED]"));
        assert!(out.contains("https://example.com"));
    }
}
