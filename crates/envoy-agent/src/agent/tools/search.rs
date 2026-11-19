//! Remote/local fast search tools (`glob` and `grep`) backed by `envoy_service::Remote`.

use super::resolve_path;
use anyhow::{Context, Result};
use envoy_service::{AgentDialer, Remote};
use serde_json::Value;
use std::path::Path;

pub(crate) async fn glob_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<String> {
    let pattern = args["pattern"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("missing `pattern`")?;
    let raw_root = args["path"].as_str().unwrap_or(".");
    let root = resolve_path(cwd, raw_root);
    let max_results = args["max_results"].as_u64().unwrap_or(200).clamp(1, 2000) as u32;

    let paths = remote.glob(target, &root, pattern, max_results).await?;
    if paths.is_empty() {
        return Ok("(no matching files)".into());
    }
    let mut out = paths.join("\n");
    if paths.len() >= max_results as usize {
        out.push_str(&format!("\n\n[capped at {max_results} matches]"));
    }
    Ok(out)
}

pub(crate) async fn grep_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<String> {
    let pattern = args["pattern"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing `pattern`")?;
    let raw_root = args["path"].as_str().unwrap_or(".");
    let root = resolve_path(cwd, raw_root);
    let include = args["include"].as_str().filter(|s| !s.trim().is_empty());
    let max_results = args["max_results"].as_u64().unwrap_or(100).clamp(1, 1000) as u32;

    let hits = remote
        .grep(target, &root, pattern, include, max_results)
        .await?;
    if hits.is_empty() {
        return Ok("(no matches)".into());
    }
    let mut out = String::new();
    for h in &hits {
        out.push_str(&format!("{}:{}: {}\n", h.path, h.line, h.text));
    }
    if hits.len() >= max_results as usize {
        out.push_str(&format!("\n[capped at {max_results} matches]"));
    }
    Ok(out)
}
