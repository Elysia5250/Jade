//! Unified core implementation of the 8 coding-agent / MCP tools:
//! `exec`, `read`, `write`, `edit`, `ls`, `glob`, `grep`, and `job`.

use anyhow::{bail, Context, Result};
use envoy_core::parse_mode;
use envoy_proto::{
    Entry, GrepHit, Job, SandboxPolicy, SubmitRequest, TaskSpec, DEFAULT_GROUP, READ_LIMIT,
};
use envoy_service::{AgentDialer, Remote};
use serde_json::Value;
use std::path::Path;

/// Bytes of command output retained at each end of an `exec` result (256 KiB total).
pub(crate) const EXEC_OUTPUT_SIDE: usize = 128 * 1024;
pub(crate) const DEFAULT_GLOB_MAX: u64 = 200;
pub(crate) const MAX_GLOB_LIMIT: u64 = 2000;
pub(crate) const DEFAULT_GREP_MAX: u64 = 100;
pub(crate) const MAX_GREP_LIMIT: u64 = 1000;

/// Resolve relative paths against `cwd` unless already absolute or `~`-rooted.
pub(crate) fn resolve_path(cwd: &Path, raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "." {
        return cwd.display().to_string();
    }
    if trimmed.starts_with('/') || trimmed.starts_with("~/") || trimmed == "~" {
        return trimmed.to_string();
    }
    cwd.join(trimmed).display().to_string()
}

pub(crate) fn resolve_opt(cwd: Option<&Path>, raw: &str) -> String {
    match cwd {
        Some(base) => resolve_path(base, raw),
        None => raw.to_string(),
    }
}

/// Extract validated `(key, value)` environment overrides from `args["env"]`.
pub(crate) fn env_arg(args: &Value) -> Result<Vec<(String, String)>> {
    let Some(map) = args["env"].as_object() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(map.len());
    for (k, v) in map {
        if let Some(val) = v.as_str() {
            if envoy_core::sandbox::is_dangerous_env_var(k) || val.contains('\0') {
                bail!("disallowed environment variable `{k}`");
            }
            out.push((k.clone(), val.to_string()));
        }
    }
    Ok(out)
}

pub(crate) fn validate_env_arg(args: &Value) -> Result<()> {
    let _ = env_arg(args)?;
    Ok(())
}

/// Build a validated `SubmitRequest` for `exec`.
pub(crate) fn build_exec_request(
    args: &Value,
    default_cwd: &Path,
    sandbox: SandboxPolicy,
    default_timeout_s: u64,
    allow_env: bool,
) -> Result<SubmitRequest> {
    let command = args["command"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .context("missing `command`")?
        .to_string();
    let cwd = args["cwd"]
        .as_str()
        .map(|s| resolve_path(default_cwd, s))
        .unwrap_or_else(|| default_cwd.display().to_string());
    let timeout_s = args["timeout_s"]
        .as_u64()
        .unwrap_or(default_timeout_s)
        .min(3600);
    let env = if allow_env {
        env_arg(args)?
    } else {
        Vec::new()
    };
    Ok(SubmitRequest {
        spec: TaskSpec::Shell { command },
        cwd: Some(cwd),
        env,
        label: if allow_env {
            args["label"].as_str().map(str::to_string)
        } else {
            None
        },
        group: if allow_env {
            args["group"]
                .as_str()
                .filter(|g| !g.is_empty())
                .unwrap_or(DEFAULT_GROUP)
                .to_string()
        } else {
            DEFAULT_GROUP.to_string()
        },
        timeout_s,
        sandbox,
    })
}

/// Head-and-tail buffer capping in-memory command output at `2 * EXEC_OUTPUT_SIDE`.
pub(crate) struct BoundedOutput {
    head: Vec<u8>,
    tail: Vec<u8>,
    dropped: usize,
}

impl BoundedOutput {
    pub(crate) fn new() -> Self {
        Self {
            head: Vec::new(),
            tail: Vec::new(),
            dropped: 0,
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if self.head.len() >= EXEC_OUTPUT_SIDE {
            self.push_tail(data);
            return;
        }
        let room = EXEC_OUTPUT_SIDE - self.head.len();
        if data.len() <= room {
            self.head.extend_from_slice(data);
            return;
        }
        self.head.extend_from_slice(&data[..room]);
        self.push_tail(&data[room..]);
    }

    fn push_tail(&mut self, data: &[u8]) {
        self.tail.extend_from_slice(data);
        if self.tail.len() <= EXEC_OUTPUT_SIDE {
            return;
        }
        let overflow = self.tail.len() - EXEC_OUTPUT_SIDE;
        self.dropped += overflow;
        self.tail.drain(..overflow);
    }

    pub(crate) fn render(&self) -> String {
        if self.dropped == 0 {
            if self.tail.is_empty() {
                return String::from_utf8_lossy(&self.head).into_owned();
            }
            let mut joined = Vec::with_capacity(self.head.len() + self.tail.len());
            joined.extend_from_slice(&self.head);
            joined.extend_from_slice(&self.tail);
            return String::from_utf8_lossy(&joined).into_owned();
        }
        format!(
            "{}\n[... {} bytes omitted ...]\n{}",
            String::from_utf8_lossy(&self.head),
            self.dropped,
            String::from_utf8_lossy(&self.tail)
        )
    }
}

pub(crate) struct ReadRaw {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) data: Vec<u8>,
    pub(crate) offset: u64,
    pub(crate) limit: u64,
    pub(crate) truncated: bool,
}

pub(crate) async fn read_raw(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: Option<&Path>,
    args: &Value,
) -> Result<ReadRaw> {
    let raw_path = args["path"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing `path`")?;
    let path = resolve_opt(cwd, raw_path);
    let offset = args["offset"].as_u64().unwrap_or(0);
    let limit = args["limit"].as_u64().unwrap_or(0);
    let (size, _mode, data) = remote.read(target, &path, offset, limit).await?;
    let truncated = limit == 0 && size > READ_LIMIT;
    Ok(ReadRaw {
        path,
        size,
        data,
        offset,
        limit,
        truncated,
    })
}

pub(crate) async fn write_raw(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: Option<&Path>,
    args: &Value,
    allow_mode: bool,
) -> Result<(String, u64)> {
    let raw_path = args["path"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing `path`")?;
    let path = resolve_opt(cwd, raw_path);
    let content = args["content"].as_str().context("missing `content`")?;
    let mode = if allow_mode {
        match args["mode"].as_str() {
            None => None,
            Some(m) => Some(parse_mode(m)?),
        }
    } else {
        None
    };
    let bytes = remote
        .write(target, &path, content.as_bytes(), mode)
        .await?;
    Ok((path, bytes))
}

pub(crate) async fn edit_raw(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: Option<&Path>,
    args: &Value,
) -> Result<(String, usize)> {
    let raw_path = args["path"].as_str().context("missing `path`")?;
    let path = resolve_opt(cwd, raw_path);
    let old = args["old"].as_str().context("missing `old`")?;
    let new = args["new"].as_str().context("missing `new`")?;
    let replace_all = args["replace_all"].as_bool().unwrap_or(false);
    match remote.edit(target, &path, old, new, replace_all).await {
        Ok(n) => Ok((path, n)),
        Err(err) => {
            if let Some(hint) = near_match_hint(remote, target, &path, old).await {
                Err(anyhow::anyhow!("{err:#}\n{hint}"))
            } else {
                Err(err)
            }
        }
    }
}

async fn near_match_hint(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    path: &str,
    old: &str,
) -> Option<String> {
    let anchor = old.lines().map(str::trim).find(|l| l.len() >= 4)?;
    let (_size, _mode, data) = remote.read(target, path, 0, 256 * 1024).await.ok()?;
    let text = std::str::from_utf8(&data).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let hit = lines.iter().position(|l| l.contains(anchor))?;
    let lo = hit.saturating_sub(2);
    let hi = (hit + 5).min(lines.len());
    let mut out = format!("hint: closest match near line {}:\n", hit + 1);
    for (idx, line) in lines[lo..hi].iter().enumerate() {
        out.push_str(&format!("{:>5}| {line}\n", lo + idx + 1));
    }
    Some(out)
}

pub(crate) async fn ls_raw(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: Option<&Path>,
    args: &Value,
) -> Result<Vec<Entry>> {
    let raw_path = args["path"].as_str().unwrap_or(".");
    let path = resolve_opt(cwd, raw_path);
    remote.list(target, &path).await
}

pub(crate) async fn glob_raw(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: Option<&Path>,
    args: &Value,
) -> Result<(Vec<String>, u32)> {
    let pattern = args["pattern"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("missing `pattern`")?;
    let raw_root = args["path"].as_str().unwrap_or(".");
    let root = resolve_opt(cwd, raw_root);
    let max_results = args["max_results"]
        .as_u64()
        .unwrap_or(DEFAULT_GLOB_MAX)
        .clamp(1, MAX_GLOB_LIMIT) as u32;
    let paths = remote.glob(target, &root, pattern, max_results).await?;
    Ok((paths, max_results))
}

pub(crate) async fn grep_raw(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: Option<&Path>,
    args: &Value,
) -> Result<(Vec<GrepHit>, u32)> {
    let pattern = args["pattern"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing `pattern`")?;
    let raw_root = args["path"].as_str().unwrap_or(".");
    let root = resolve_opt(cwd, raw_root);
    let include = args["include"].as_str().filter(|s| !s.trim().is_empty());
    let max_results = args["max_results"]
        .as_u64()
        .unwrap_or(DEFAULT_GREP_MAX)
        .clamp(1, MAX_GREP_LIMIT) as u32;
    let hits = remote
        .grep(target, &root, pattern, include, max_results)
        .await?;
    Ok((hits, max_results))
}

pub(crate) fn job_in_scope(target: &str, cwd: &Path, job_cwd: &str) -> bool {
    let is_local = target.is_empty() || target == "local";
    !is_local
        || (!job_cwd.is_empty()
            && crate::permissions::paths::is_path_under_workspace(cwd, Path::new(job_cwd)))
}

pub(crate) fn check_job_scope(target: &str, cwd: &Path, id: u64, job_cwd: &str) -> Result<()> {
    if !job_in_scope(target, cwd, job_cwd) {
        bail!("job #{id} belongs to another workspace ({job_cwd})");
    }
    Ok(())
}

pub(crate) async fn job_list_scoped(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
) -> Result<Vec<Job>> {
    Ok(remote
        .jobs(target)
        .await?
        .into_iter()
        .filter(|j| job_in_scope(target, cwd, &j.cwd))
        .collect())
}

pub(crate) async fn job_status_scoped(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    id: u64,
) -> Result<Job> {
    let job = remote.status(target, id).await?;
    check_job_scope(target, cwd, id, &job.cwd)?;
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shared_env_arg_and_path_resolution_enforce_consistent_rules() {
        assert_eq!(
            resolve_path(Path::new("/work/app"), "src/main.rs"),
            "/work/app/src/main.rs"
        );
        assert_eq!(
            resolve_path(Path::new("/work/app"), "/etc/hosts"),
            "/etc/hosts"
        );
        assert!(validate_env_arg(&json!({ "env": { "LD_PRELOAD": "/tmp/x" } })).is_err());
        assert!(validate_env_arg(&json!({ "env": { "OK": "a\0b" } })).is_err());
        assert!(validate_env_arg(&json!({ "env": { "RUST_LOG": "info" } })).is_ok());

        let req = build_exec_request(
            &json!({ "command": "echo hi", "cwd": "sub", "timeout_s": 99_999 }),
            Path::new("/work/app"),
            SandboxPolicy::Off,
            120,
            true,
        )
        .unwrap();
        assert_eq!(req.cwd.as_deref(), Some("/work/app/sub"));
        assert_eq!(req.timeout_s, 3600);
    }

    #[test]
    fn shared_bounded_output_and_job_scope_are_consistent() {
        let mut out = BoundedOutput::new();
        out.push(&vec![b'a'; EXEC_OUTPUT_SIDE]);
        out.push(&vec![b'b'; EXEC_OUTPUT_SIDE + 32]);
        let rendered = out.render();
        assert!(rendered.contains("[... 32 bytes omitted ...]"));
        assert!(rendered.starts_with("aaaa"));
        assert!(rendered.ends_with("bbbb"));

        let ws = std::env::temp_dir();
        assert!(!job_in_scope("local", &ws, "/nonexistent-other-ws"));
        assert!(job_in_scope("remote-peer", &ws, "/anywhere"));
    }
}
