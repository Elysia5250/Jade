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
                "description": "Read bytes from a file on the active target machine.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "offset": { "type": "integer" },
                        "limit": { "type": "integer" }
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
                        "action": { "type": "string", "enum": ["list", "status", "logs", "cancel"] },
                        "id": { "type": "integer" },
                        "offset": { "type": "integer" }
                    },
                    "required": ["action"]
                }
            }
        }),
    ]
}

pub(crate) fn responses_tools_json() -> Vec<Value> {
    openai_tools_json()
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

pub(crate) fn anthropic_tools_json() -> Vec<Value> {
    openai_tools_json()
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
