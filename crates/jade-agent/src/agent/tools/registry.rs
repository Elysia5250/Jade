//! Narrow `Tool` trait and `ToolRegistry` for built-in and embedder-supplied tools.

use crate::agent::events::AgentEvent;
use crate::permissions::PermissionEngine;
use anyhow::Result;
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Declarative specification for a single tool exposed to the model.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// True if invoking this tool can mutate workspace, job, or external state.
    pub mutates: bool,
}

impl ToolSpec {
    pub fn to_openai_json(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

/// Execution context passed to custom [`Tool`] implementations.
pub struct ToolExecContext<'a> {
    pub call_id: &'a str,
    pub cwd: &'a Path,
    pub target: &'a str,
    pub data_dir: &'a Path,
    pub session_id: &'a str,
    pub permissions: &'a Arc<PermissionEngine>,
    pub tx: &'a mpsc::UnboundedSender<AgentEvent>,
}

/// Output returned by a custom [`Tool`] implementation.
#[derive(Debug, Clone, Default)]
pub struct ToolExecResult {
    pub text: String,
    pub exit_code: Option<i32>,
}

impl ToolExecResult {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            exit_code: None,
        }
    }
}

/// Narrow extension trait for registering in-process custom tools with `jade-agent`.
///
/// Every call still passes through `HookEngine` (`PreToolUse`/`PostToolUse`),
/// `PermissionEngine`, output previewing, and context truncation.
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    fn summarize(&self, _args: &Value) -> (String, String) {
        (self.spec().name, String::new())
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a ToolExecContext<'a>,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolExecResult>>;
}

/// Registry holding built-in tool specifications and optional custom [`Tool`] adapters.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    builtins: Vec<ToolSpec>,
    custom: BTreeMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("builtins", &self.builtins.len())
            .field("custom", &self.custom.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    pub fn with_builtins() -> Self {
        Self {
            builtins: super::schema::builtin_tool_specs(),
            custom: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        let name = tool.spec().name;
        self.custom.insert(name, tool);
        self
    }

    pub fn get_custom(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.custom.get(name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut out: Vec<ToolSpec> = self
            .builtins
            .iter()
            .filter(|s| !self.custom.contains_key(&s.name))
            .cloned()
            .collect();
        for tool in self.custom.values() {
            out.push(tool.spec());
        }
        out
    }

    pub fn custom_tool_schemas(&self) -> Vec<Value> {
        self.custom
            .values()
            .map(|t| t.spec().to_openai_json())
            .collect()
    }

    pub fn is_mutating(&self, name: &str) -> bool {
        if let Some(custom) = self.custom.get(name) {
            return custom.spec().mutates;
        }
        if let Some(builtin) = self.builtins.iter().find(|s| s.name == name) {
            return builtin.mutates;
        }
        // Unknown or external MCP tools default to mutating unless explicitly declared read-only.
        name != "mcp_search"
    }
}
