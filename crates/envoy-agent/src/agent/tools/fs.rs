//! Filesystem tools (`read` with optional line range, `write`, `edit` with near-match hints, `ls`).
use crate::tools_core;
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
    let start_line = args["start_line"].as_u64();
    let line_count = args["line_count"].as_u64();
    let raw = tools_core::read_raw(remote, target, Some(cwd), args).await?;
    let body = String::from_utf8_lossy(&raw.data).to_string();

    if (start_line.is_some() || line_count.is_some()) && raw.offset == 0 && raw.limit == 0 {
        let lines: Vec<&str> = body.lines().collect();
        let total_lines = lines.len();
        let start = start_line.unwrap_or(1).max(1) as usize;
        if start > total_lines {
            return Ok(format!(
                "[start_line {start} past EOF ({total_lines} lines)]"
            ));
        }
        let count = line_count.unwrap_or(200).clamp(1, 2000) as usize;
        let end = (start - 1).saturating_add(count).min(total_lines);
        let mut out = String::new();
        for (idx, line) in lines[start - 1..end].iter().enumerate() {
            out.push_str(&format!("{:>5}| {line}\n", start + idx));
        }
        if end < total_lines || raw.size > READ_LIMIT {
            out.push_str(&format!(
                "\n[lines {start}..={end} of {total_lines} (file {} bytes)]",
                raw.size
            ));
        }
        return Ok(out);
    }

    if raw.truncated {
        Ok(format!("{body}\n\n[{} bytes total; truncated]", raw.size))
    } else {
        Ok(body)
    }
}

pub(crate) struct FileEditSnapshot {
    pub path: String,
    pub before: String,
    pub after: String,
    pub existed_before: bool,
    pub restorable_before: bool,
}

const SNAPSHOT_CAP: u64 = 256 * 1024;

async fn read_snapshot_text(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    path: &str,
) -> (bool, String, bool) {
    match remote.read(target, path, 0, SNAPSHOT_CAP).await {
        Ok((size, _mode, data)) => {
            let restorable = size <= SNAPSHOT_CAP && std::str::from_utf8(&data).is_ok();
            (
                true,
                String::from_utf8_lossy(&data).into_owned(),
                restorable,
            )
        }
        Err(err) => {
            let local = target.is_empty() || target == "local";
            let missing = if local {
                Path::new(path).symlink_metadata().is_err()
            } else {
                let msg = format!("{err:#}").to_ascii_lowercase();
                msg.contains("no such file") || msg.contains("not found")
            };
            if missing {
                (false, String::new(), true)
            } else {
                (true, String::new(), false)
            }
        }
    }
}

pub(crate) async fn write_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<(String, Option<FileEditSnapshot>)> {
    let raw_path = args["path"].as_str().context("missing `path`")?;
    let snap_path = tools_core::resolve_path(cwd, raw_path);
    let content = args["content"].as_str().context("missing `content`")?;
    let (existed_before, before, restorable_before) =
        read_snapshot_text(remote, target, &snap_path).await;
    let (path, bytes) = tools_core::write_raw(remote, target, Some(cwd), args, false).await?;
    let after = if content.len() as u64 <= SNAPSHOT_CAP {
        content.to_string()
    } else {
        content.chars().take(SNAPSHOT_CAP as usize).collect()
    };
    Ok((
        format!("wrote {bytes} bytes to {path}"),
        Some(FileEditSnapshot {
            path,
            before,
            after,
            existed_before,
            restorable_before,
        }),
    ))
}

pub(crate) async fn edit_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<(String, Option<FileEditSnapshot>)> {
    let raw_path = args["path"].as_str().context("missing `path`")?;
    let snap_path = tools_core::resolve_path(cwd, raw_path);
    let (existed_before, before, restorable_before) =
        read_snapshot_text(remote, target, &snap_path).await;
    let (path, n) = tools_core::edit_raw(remote, target, Some(cwd), args).await?;
    let (_existed_after, after, _) = read_snapshot_text(remote, target, &path).await;
    Ok((
        format!("edited {path}: {n} replacement(s)"),
        Some(FileEditSnapshot {
            path,
            before,
            after,
            existed_before: true,
            restorable_before: existed_before && restorable_before,
        }),
    ))
}

pub(crate) async fn ls_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<String> {
    let entries = tools_core::ls_raw(remote, target, Some(cwd), args).await?;
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
