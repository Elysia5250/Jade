//! Remote/local fast search tools (`glob` and `grep`) backed by `crate::tools_core`.

use crate::tools_core;
use anyhow::Result;
use envoy_service::{AgentDialer, Remote};
use serde_json::Value;
use std::path::Path;

pub(crate) async fn glob_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<String> {
    let (paths, max_results) = tools_core::glob_raw(remote, target, Some(cwd), args).await?;
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
    let (hits, max_results) = tools_core::grep_raw(remote, target, Some(cwd), args).await?;
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
