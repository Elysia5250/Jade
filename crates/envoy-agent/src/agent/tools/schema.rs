//! Tool schemas for the three wire protocols.
use serde_json::{json, Value};

pub(crate) fn openai_tools_json() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "exec",
                "description": "Run a shell command on the active target machine and return its output and exit code.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to execute via sh -c." },
                        "cwd": { "type": "string", "description": "Working directory (defaults to session cwd)." },
                        "timeout_s": { "type": "integer", "description": "Timeout in seconds (default 120)." }
                    },
                    "required": ["command"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read bytes or a line range from a file on the active target machine.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "offset": { "type": "integer" },
                        "limit": { "type": "integer" },
                        "start_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based starting line." },
                        "line_count": { "type": "integer", "minimum": 1, "description": "Optional max lines to return." }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write",
                "description": "Write a whole file atomically on the active target machine.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "edit",
                "description": "Replace an exact unique snippet in a text file atomically.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old": { "type": "string" },
                        "new": { "type": "string" },
                        "replace_all": { "type": "boolean" }
                    },
                    "required": ["path", "old", "new"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "ls",
                "description": "List one directory level on the active target machine.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "glob",
                "description": "Find files matching a glob pattern (e.g. `**/*.rs`) on the active target machine.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob pattern such as `src/**/*.rs`." },
                        "path": { "type": "string", "description": "Root directory (default `.`)." },
                        "max_results": { "type": "integer", "minimum": 1, "default": 200 }
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search file contents by regex pattern on the active target machine.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regular expression pattern." },
                        "path": { "type": "string", "description": "Root directory or file (default `.`)." },
                        "include": { "type": "string", "description": "Optional filename glob filter (e.g. `*.rs`)." },
                        "max_results": { "type": "integer", "minimum": 1, "default": 100 }
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "webfetch",
                "description": "Fetch a public HTTP/HTTPS page with SSRF protection.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" },
                        "format": { "type": "string", "enum": ["text", "html"], "default": "text" },
                        "timeout_s": { "type": "integer", "minimum": 1, "maximum": 60, "default": 20 }
                    },
                    "required": ["url"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "todo",
                "description": "Read or update the session task checklist.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["read", "write"] },
                        "items": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": { "type": "string" },
                                    "content": { "type": "string" },
                                    "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                                    "priority": { "type": "string", "enum": ["high", "medium", "low"] }
                                },
                                "required": ["content", "status"]
                            }
                        }
                    },
                    "required": ["action"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "skill",
                "description": "Load the full instructions and resources for a skill listed in `<available_skills>`. Only call this for a skill present in `<available_skills>`; if `<available_skills>` is absent, no skills are installed.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Skill name from `<available_skills>`." }
                    },
                    "required": ["name"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "say",
                "description": "One plain text progress line for the user. No period, no path, no Markdown, display width at most 48. Chinese: `正在<动作>，接着<下一拍>` or `收到，先<动作>`. English: `Doing <action>, then <next>` or `On it, first <action>`. Other languages use that same present-tense shape. Always emit one alongside your very first tool call after a user message or `ask` answer, and also when changing phase, starting long work, or updating the plan. Emit in the same response as the next real tool; never spend a turn on `say` alone, do not call it for every file read, and do not restate the request. Non-blocking.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "One plain text line in the user's language. Chinese: `正在对列表缩进，接着改代码块对齐`. English: `Doing the list indent, then the code-block alignment`." },
                        "kind": { "type": "string", "enum": ["progress", "note", "warning"], "default": "progress" }
                    },
                    "required": ["text"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "ask",
                "description": "Ask the user to choose, in the user's language. The question is at most 22 Chinese characters or 44 Latin letters. Give 2-4 options. Blocks the turn until answered. `recommended` is a highlight only. If nobody answers, the call fails (default 120s).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": { "type": "string", "description": "One question in the user's language. At most 22 Chinese characters or 44 Latin letters." },
                        "options": {
                            "type": "array",
                            "minItems": 2,
                            "maxItems": 4,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": { "type": "string", "description": "At most 8 Chinese characters or 16 Latin letters." },
                                    "description": { "type": "string", "description": "At most 18 Chinese characters or 36 Latin letters. Omit when the label is enough." },
                                    "recommended": { "type": "boolean" }
                                },
                                "required": ["label"]
                            }
                        },
                        "timeout_s": { "type": "integer", "default": 120 }
                    },
                    "required": ["question", "options"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "job",
                "description": "Inspect or control detached jobs (`list`, `wait`, `status`, `logs`, `cancel`).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["list", "wait", "status", "logs", "cancel"] },
                        "id": { "type": "integer" },
                        "offset": { "type": "integer" }
                    },
                    "required": ["action"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "delegate",
                "description": "Delegate one or more focused subtasks to isolated subagents (`explore`, `plan`, `review`, `coder`, or presets in `<available_agents>`). Pass `agent` + `prompt` for one subtask, or `tasks: [...]` to run multiple independent subtasks (read-only batches run in parallel). In a local git checkout, two or more coding tasks each get a worktree, run in parallel, and are merged back into the current branch.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "agent": { "type": "string", "description": "Subagent name (`explore`, `plan`, `review`, `coder`, or custom preset)." },
                        "prompt": { "type": "string", "description": "Self-contained instructions and context for the subagent." },
                        "description": { "type": "string", "description": "Short 3-6 word summary shown on the tool card." },
                        "model": { "type": "string", "description": "Optional override: `inherit`, `fast`, `flagship`, or `<provider>/<model>`." },
                        "tasks": {
                            "type": "array",
                            "description": "Batch of independent subtasks. Read-only tasks execute concurrently.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "agent": { "type": "string" },
                                    "prompt": { "type": "string" },
                                    "description": { "type": "string" },
                                    "model": { "type": "string" }
                                },
                                "required": ["agent", "prompt"]
                            }
                        }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "monitor",
                "description": "Watch a background shell command or existing detached job for matching output lines and deliver in-session alerts (`start`, `list`, `stop`).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["start", "list", "stop"] },
                        "command": { "type": "string", "description": "Shell command to spawn and monitor (when action=start)." },
                        "cwd": { "type": "string", "description": "Optional working directory for `command`." },
                        "job_id": { "type": "integer", "description": "Existing job ID to monitor instead of spawning a command." },
                        "pattern": { "type": "string", "description": "Case-insensitive substring or `|`-separated alternatives (e.g. `error|panic`), or `*` for any line." },
                        "label": { "type": "string", "description": "Short display label for the monitor." },
                        "debounce_ms": { "type": "integer", "default": 500, "description": "Batch matched lines over this window before notifying (100-30000ms)." },
                        "max_triggers": { "type": "integer", "description": "Optional max notifications before auto-stopping (0 or omit for unlimited)." },
                        "timeout_s": { "type": "integer", "default": 3600, "description": "Job timeout in seconds when starting `command`." },
                        "id": { "type": "integer", "description": "Monitor ID (required for action=stop)." }
                    },
                    "required": ["action"]
                }
            }
        }),
    ]
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ToolFilter {
    pub(crate) depth: u8,
    pub(crate) allowed: Option<Vec<String>>,
}

impl ToolFilter {
    /// Summary and other no-tool requests. An empty allow-list matches nothing.
    pub(crate) fn none() -> Self {
        Self {
            depth: 0,
            allowed: Some(Vec::new()),
        }
    }

    pub(crate) fn allows(&self, name: &str) -> bool {
        if self.depth > 0 && matches!(name, "delegate" | "monitor") {
            return false;
        }
        if let Some(allowed) = &self.allowed {
            return allowed.iter().any(|a| a == name);
        }
        true
    }

    /// Tighten the allow-list to `names`. An empty list adds no restriction.
    /// `skill` stays available when this agent could already call it.
    pub(crate) fn restrict_to(&mut self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let mut next = names.to_vec();
        if self.allows("skill") && !next.iter().any(|name| name == "skill") {
            next.push("skill".to_string());
        }
        if let Some(existing) = &self.allowed {
            next.retain(|name| existing.iter().any(|kept| kept == name));
        }
        self.allowed = Some(next);
    }
}

pub(crate) fn openai_tools_json_filtered(
    permissions: Option<&crate::permissions::PermissionEngine>,
    filter: &ToolFilter,
    extra: &[Value],
) -> Vec<Value> {
    let mut all = openai_tools_json();
    all.extend(extra.iter().cloned());
    all.into_iter()
        .filter(|t| {
            let Some(name) = t["function"]["name"].as_str() else {
                return true;
            };
            if !filter.allows(name) {
                return false;
            }
            permissions.map(|p| p.is_tool_enabled(name)).unwrap_or(true)
        })
        .collect()
}

pub(crate) fn responses_tools_json_filtered(
    permissions: Option<&crate::permissions::PermissionEngine>,
    filter: &ToolFilter,
    extra: &[Value],
) -> Vec<Value> {
    openai_tools_json_filtered(permissions, filter, extra)
        .into_iter()
        .map(|t| {
            let f = &t["function"];
            json!({
                "type": "function",
                "name": f["name"],
                "description": f["description"],
                "parameters": f["parameters"],
            })
        })
        .collect()
}

pub(crate) fn anthropic_tools_json_filtered(
    permissions: Option<&crate::permissions::PermissionEngine>,
    filter: &ToolFilter,
    extra: &[Value],
) -> Vec<Value> {
    openai_tools_json_filtered(permissions, filter, extra)
        .into_iter()
        .map(|t| {
            let f = &t["function"];
            json!({
                "name": f["name"],
                "description": f["description"],
                "input_schema": f["parameters"],
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_filter_hides_delegate_for_subagents_and_enforces_allowlist() {
        let root_tools = openai_tools_json_filtered(None, &ToolFilter::default(), &[]);
        assert!(root_tools
            .iter()
            .any(|t| t["function"]["name"] == "delegate"));

        let child_filter = ToolFilter {
            depth: 1,
            allowed: Some(vec!["read".into(), "grep".into(), "delegate".into()]),
        };
        let child_tools = openai_tools_json_filtered(None, &child_filter, &[]);
        let names: Vec<&str> = child_tools
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert_eq!(names, vec!["read", "grep"]);
    }

    #[test]
    fn restrict_to_keeps_skill_and_intersects_the_existing_list() {
        let mut open = ToolFilter::default();
        open.restrict_to(&[]);
        assert!(open.allowed.is_none());
        open.restrict_to(&["read".into(), "grep".into()]);
        assert!(open.allows("read"));
        assert!(open.allows("skill"));
        assert!(!open.allows("exec"));
        open.restrict_to(&["exec".into(), "read".into()]);
        assert!(open.allows("read"));
        assert!(open.allows("skill"));
        assert!(!open.allows("exec"));

        let mut narrow = ToolFilter {
            depth: 0,
            allowed: Some(vec!["read".into()]),
        };
        narrow.restrict_to(&["grep".into()]);
        assert!(!narrow.allows("read"));
        assert!(!narrow.allows("grep"));
        assert!(!narrow.allows("skill"));
    }
}
