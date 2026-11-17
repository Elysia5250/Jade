//! On-demand `SKILL.md` Agent Skill loader tool.

use crate::skills::load_skill;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

pub(crate) fn skill_tool(data_dir: &Path, cwd: &Path, args: &Value) -> Result<String> {
    let name = args["name"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("missing `name`")?;
    let loaded = load_skill(data_dir, cwd, name)?;
    let mut out = format!(
        "# Skill: {}\nscope: {}\ndir: {}\n",
        loaded.summary.name,
        loaded.summary.scope.label(),
        loaded.summary.dir.display()
    );
    if let Some(compat) = &loaded.summary.compatibility {
        out.push_str(&format!("compatibility: {compat}\n"));
    }
    if !loaded.summary.allowed_tools.is_empty() {
        out.push_str(&format!(
            "allowed_tools: {}\n",
            loaded.summary.allowed_tools.join(", ")
        ));
    }
    if !loaded.resources.is_empty() {
        out.push_str("resources:\n");
        for rel in &loaded.resources {
            out.push_str(&format!("  - {rel}\n"));
        }
    }
    out.push('\n');
    out.push_str(&loaded.body);
    Ok(out)
}
