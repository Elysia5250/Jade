//! Session-scoped structured task checklist (`todo`) tool.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
    #[serde(default)]
    pub priority: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pending" | "todo" => Ok(Self::Pending),
            "in_progress" | "in-progress" | "doing" => Ok(Self::InProgress),
            "completed" | "done" => Ok(Self::Completed),
            other => bail!("invalid todo status `{other}` (use pending, in_progress, or completed)"),
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[~]",
            Self::Completed => "[x]",
        }
    }
}

fn todo_path(data_dir: &Path, session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    data_dir.join("todos").join(format!("{safe}.json"))
}

pub(crate) fn load_todos(data_dir: &Path, session_id: &str) -> Vec<TodoItem> {
    let path = todo_path(data_dir, session_id);
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub(crate) fn save_todos(data_dir: &Path, session_id: &str, items: &[TodoItem]) -> Result<()> {
    let path = todo_path(data_dir, session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(items)?)?;
    Ok(())
}

pub(crate) fn format_todos(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "(no todos)".to_string();
    }
    let mut out = String::new();
    for item in items {
        let prio = item
            .priority
            .as_deref()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        out.push_str(&format!(
            "{} {}{}: {}\n",
            item.status.marker(),
            item.id,
            prio,
            item.content
        ));
    }
    out.trim_end().to_string()
}

pub(crate) fn format_active_todos_xml(data_dir: &Path, session_id: &str) -> Option<String> {
    let items = load_todos(data_dir, session_id);
    if items.is_empty() {
        return None;
    }
    Some(format!("<active_todos>\n{}\n</active_todos>", format_todos(&items)))
}

pub(crate) fn todo_tool(data_dir: &Path, session_id: &str, args: &Value) -> Result<String> {
    let action = args["action"]
        .as_str()
        .unwrap_or_else(|| if args.get("items").is_some() { "write" } else { "read" });
    match action {
        "read" => {
            let items = load_todos(data_dir, session_id);
            Ok(format_todos(&items))
        }
        "write" => {
            let raw_items = args["items"]
                .as_array()
                .context("missing `items` array for todo write")?;
            let mut items = Vec::with_capacity(raw_items.len());
            for (idx, raw) in raw_items.iter().enumerate() {
                let content = raw["content"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("todo item missing `content`")?
                    .to_string();
                let id = raw["id"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{}", idx + 1));
                let status = raw["status"]
                    .as_str()
                    .map(TodoStatus::parse)
                    .transpose()?
                    .unwrap_or(TodoStatus::Pending);
                let priority = raw["priority"].as_str().map(|s| s.trim().to_string());
                items.push(TodoItem {
                    id,
                    content,
                    status,
                    priority,
                });
            }
            save_todos(data_dir, session_id, &items)?;
            Ok(format_todos(&items))
        }
        other => bail!("unknown todo action `{other}` (use `read` or `write`)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn todo_write_read_and_xml_roundtrip() {
        let dir = std::env::temp_dir().join(format!("envoy-todo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let out = todo_tool(
            &dir,
            "s-1",
            &json!({
                "action": "write",
                "items": [
                    { "id": "1", "content": "implement sandbox", "status": "completed", "priority": "high" },
                    { "content": "verify mcp", "status": "in_progress" }
                ]
            }),
        )
        .unwrap();
        assert!(out.contains("[x] 1 (high): implement sandbox"));
        assert!(out.contains("[~] 2: verify mcp"));

        let xml = format_active_todos_xml(&dir, "s-1").unwrap();
        assert!(xml.contains("<active_todos>"));
        assert!(xml.contains("verify mcp"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
