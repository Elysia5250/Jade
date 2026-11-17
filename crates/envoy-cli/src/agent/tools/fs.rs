//! Filesystem tools (`read` with optional line range, `write`, `edit` with near-match hints, `ls`).
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
    let start_line = args["start_line"].as_u64();
    let line_count = args["line_count"].as_u64();

    let (size, _mode, data) = remote.read(target, &path, offset, limit).await?;
    let body = String::from_utf8_lossy(&data).to_string();

    if (start_line.is_some() || line_count.is_some()) && offset == 0 && limit == 0 {
        let lines: Vec<&str> = body.lines().collect();
        let total_lines = lines.len();
        let start = start_line.unwrap_or(1).max(1) as usize;
        if start > total_lines {
            return Ok(format!("[start_line {start} past EOF ({total_lines} lines)]"));
        }
        let count = line_count.unwrap_or(200).clamp(1, 2000) as usize;
        let end = (start - 1).saturating_add(count).min(total_lines);
        let mut out = String::new();
        for (idx, line) in lines[start - 1..end].iter().enumerate() {
            out.push_str(&format!("{:>5}| {line}\n", start + idx));
        }
        if end < total_lines || size > READ_LIMIT {
            out.push_str(&format!(
                "\n[lines {start}..={end} of {total_lines} (file {size} bytes)]"
            ));
        }
        return Ok(out);
    }

    if limit == 0 && size > READ_LIMIT {
        Ok(format!("{body}\n\n[{} bytes total; truncated]", size))
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
    let path = resolve_path(cwd, raw_path);
    let content = args["content"].as_str().context("missing `content`")?;
    let (existed_before, before, restorable_before) =
        read_snapshot_text(remote, target, &path).await;
    let bytes = remote
        .write(target, &path, content.as_bytes(), None)
        .await?;
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
    let path = resolve_path(cwd, raw_path);
    let old = args["old"].as_str().context("missing `old`")?;
    let new = args["new"].as_str().context("missing `new`")?;
    let replace_all = args["replace_all"].as_bool().unwrap_or(false);
    let (existed_before, before, restorable_before) =
        read_snapshot_text(remote, target, &path).await;
    match remote.edit(target, &path, old, new, replace_all).await {
        Ok(n) => {
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
