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
                "description": "Load the full instructions and resources for a skill listed in `<available_skills>`.",
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
                "description": "Report a short progress note to the user. Non-blocking.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" },
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
                "description": "Ask the user to choose between options. Blocks the turn until answered; on timeout the recommended option is used (default 300s).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": { "type": "string" },
                        "options": {
                            "type": "array",
                            "minItems": 2,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": { "type": "string" },
                                    "description": { "type": "string" },
                                    "recommended": { "type": "boolean" }
                                },
                                "required": ["label"]
                            }
                        },
                        "timeout_s": { "type": "integer", "default": 300 }
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
    ]
}

pub(crate) fn openai_tools_json_for(
    permissions: Option<&crate::permissions::PermissionEngine>,
    extra: &[Value],
) -> Vec<Value> {
    let mut all = openai_tools_json();
    all.extend(extra.iter().cloned());
    let Some(perm) = permissions else {
        return all;
    };
    all.into_iter()
        .filter(|t| {
            t["function"]["name"]
                .as_str()
                .map(|n| perm.is_tool_enabled(n))
                .unwrap_or(true)
        })
        .collect()
}

pub(crate) fn responses_tools_json_for(
    permissions: Option<&crate::permissions::PermissionEngine>,
    extra: &[Value],
) -> Vec<Value> {
    openai_tools_json_for(permissions, extra)
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

pub(crate) fn anthropic_tools_json_for(
    permissions: Option<&crate::permissions::PermissionEngine>,
    extra: &[Value],
) -> Vec<Value> {
    openai_tools_json_for(permissions, extra)
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

#[allow(dead_code)]
pub(crate) fn responses_tools_json() -> Vec<Value> {
    responses_tools_json_for(None, &[])
}

#[allow(dead_code)]
pub(crate) fn anthropic_tools_json() -> Vec<Value> {
    anthropic_tools_json_for(None, &[])
}
