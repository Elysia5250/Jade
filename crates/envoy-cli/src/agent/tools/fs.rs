//! Filesystem tools.
use super::resolve_path;
use anyhow::{Context, Result};
use envoy_proto::READ_LIMIT;
use envoy_service::{AgentDialer, Remote};
use serde_json::Value;
use std::path::Path;

pub(crate) async fn read_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<String> {
    let raw_path = args["path"].as_str().context("missing `path`")?;
    let path = resolve_path(cwd, raw_path);
    let offset = args["offset"].as_u64().unwrap_or(0);
    let limit = args["limit"].as_u64().unwrap_or(0);
    let (size, _mode, data) = remote.read(target, &path, offset, limit).await?;
    let body = String::from_utf8_lossy(&data).to_string();
    if limit == 0 && size > READ_LIMIT {
        Ok(format!("{body}\n\n[{} bytes total; truncated]", size))
    } else {
        Ok(body)
    }
}

pub(crate) async fn write_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<String> {
    let raw_path = args["path"].as_str().context("missing `path`")?;
    let path = resolve_path(cwd, raw_path);
    let content = args["content"].as_str().context("missing `content`")?;
    let bytes = remote
        .write(target, &path, content.as_bytes(), None)
        .await?;
    Ok(format!("wrote {bytes} bytes to {path}"))
}

pub(crate) async fn edit_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<String> {
    let raw_path = args["path"].as_str().context("missing `path`")?;
    let path = resolve_path(cwd, raw_path);
    let old = args["old"].as_str().context("missing `old`")?;
    let new = args["new"].as_str().context("missing `new`")?;
    let replace_all = args["replace_all"].as_bool().unwrap_or(false);
    let n = remote.edit(target, &path, old, new, replace_all).await?;
    Ok(format!("edited {path}: {n} replacement(s)"))
}

pub(crate) async fn ls_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<String> {
    let raw_path = args["path"].as_str().unwrap_or(".");
    let path = resolve_path(cwd, raw_path);
    let entries = remote.list(target, &path).await?;
    let mut out = String::new();
    for e in entries {
        let kind = match e.kind {
            envoy_proto::EntryKind::Dir => "dir ",
            envoy_proto::EntryKind::Symlink => "link",
            _ => "file",
        };
        out.push_str(&format!("{kind} {:>8}  {}\n", e.size, e.name));
    }
    if out.is_empty() {
        out.push_str("(empty directory)");
    }
    Ok(out)
}
