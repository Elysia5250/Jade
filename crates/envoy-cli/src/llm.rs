//! Streaming LLM client and tool-use agent loop over Envoy's `Remote` RPCs.
//!
//! Supports three wire protocols so every `/login` option works out of the box:
//! - OpenAI Chat Completions SSE (OpenAI API Key, DeepSeek, Gemini, OpenRouter,
//!   GitHub Copilot, OpenCode, Ollama/vLLM/Custom OpenAI-compatible)
//! - OpenAI Codex Responses SSE (ChatGPT Plus/Pro OAuth)
//! - Anthropic Messages SSE (Anthropic API Key, Claude Pro/Max OAuth, Custom
//!   Anthropic-compatible)

use crate::auth::{
    copilot_session_token, ensure_fresh_provider, Credential, ModelRef, ProviderProtocol,
    StoredProvider,
};
use crate::daemon;
use crate::session::{ChatMessage, ChatRole, ToolCall};
use anyhow::{anyhow, bail, Context, Result};
use envoy_proto::{LogRecord, SubmitRequest, TaskSpec, DEFAULT_GROUP, READ_LIMIT};
use envoy_service::{dialer_for, AgentDialer, Remote};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const TOOL_OUTPUT_CAP: usize = 64 * 1024;
const MAX_TOOL_STEPS: usize = 25;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentEvent {
    /// The daemon started a fresh session (it was restarted): rebuild the view.
    SessionReset,
    /// A user message entered the session (prompt, steer, or queued follow-up).
    UserMessage {
        text: String,
        mode: String,
    },
    AssistantStart {
        model: String,
    },
    ReasoningDelta(String),
    TextDelta(String),
    ToolStart {
        call_id: String,
        name: String,
        title: String,
        subtitle: String,
    },
    ToolJob {
        call_id: String,
        job_id: u64,
    },
    ToolOutput {
        call_id: String,
        text: String,
        err: bool,
    },
    ToolDone {
        call_id: String,
        ok: bool,
        duration_ms: u64,
        exit_code: Option<i32>,
    },
    /// A user-facing progress note from the `say` tool.
    Say {
        text: String,
        kind: String,
    },
    /// The `ask` tool wants a decision; the UI answers through [`AskBroker`].
    Ask {
        id: u64,
        question: String,
        options: Vec<AskOption>,
        recommended: usize,
        timeout_ms: u64,
    },
    /// The ask timed out (or was aborted) and the UI should close the prompt.
    AskClosed {
        id: u64,
    },
    TurnDone {
        messages: Vec<ChatMessage>,
        input_tokens: u64,
        output_tokens: u64,
    },
    TurnError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
    pub recommended: bool,
}

/// Routes `ask` questions to the UI and waits for the chosen option.
#[derive(Default)]
pub struct AskBroker {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<String>>>,
}

impl std::fmt::Debug for AskBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AskBroker")
    }
}

impl AskBroker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn register(&self, id: u64, tx: oneshot::Sender<String>) {
        self.pending.lock().unwrap().insert(id, tx);
    }

    fn forget(&self, id: u64) {
        self.pending.lock().unwrap().remove(&id);
    }

    /// Answer a pending question. Returns false if it already timed out.
    pub fn answer(&self, id: u64, label: String) -> bool {
        match self.pending.lock().unwrap().remove(&id) {
            Some(tx) => tx.send(label).is_ok(),
            None => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub data_dir: PathBuf,
    pub conduit: Option<PathBuf>,
    pub target: String,
    pub cwd: PathBuf,
    pub provider: StoredProvider,
    pub model: ModelRef,
    pub history: Vec<ChatMessage>,
    /// Questions the model asks the user.
    pub asks: Arc<AskBroker>,
    /// Messages the user steered in while the turn is running; drained between
    /// tool batches and delivered verbatim as `role=user`.
    pub steer: Arc<Mutex<Vec<String>>>,
}

pub async fn run_agent_turn(cfg: TurnConfig, tx: mpsc::UnboundedSender<AgentEvent>) {
    let model_label = cfg.model.display();
    let _ = tx.send(AgentEvent::AssistantStart {
        model: model_label.clone(),
    });
    match execute_turn(cfg, &tx).await {
        Ok((messages, input_tokens, output_tokens)) => {
            let _ = tx.send(AgentEvent::TurnDone {
                messages,
                input_tokens,
                output_tokens,
            });
        }
        Err(e) => {
            let _ = tx.send(AgentEvent::TurnError(format!("{e:#}")));
        }
    }
}

async fn execute_turn(
    cfg: TurnConfig,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<(Vec<ChatMessage>, u64, u64)> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()?;

    let mut provider = ensure_fresh_provider(&http, &cfg.data_dir, cfg.provider.clone()).await?;
    let mut messages = cfg.history.clone();
    let mut total_in = 0u64;
    let mut total_out = 0u64;

    if cfg.target == "local" || cfg.target.is_empty() {
        daemon::ensure(&cfg.data_dir).await?;
    }
    let remote: Remote<Box<dyn AgentDialer>> = Remote::new(dialer_for(
        &cfg.data_dir,
        &cfg.target,
        cfg.conduit.clone(),
    ));

    let system = build_system_prompt(&cfg.target, &cfg.cwd);

    for step in 0..MAX_TOOL_STEPS {
        if step > 0 {
            let _ = tx.send(AgentEvent::AssistantStart {
                model: cfg.model.display(),
            });
        }
        provider = ensure_fresh_provider(&http, &cfg.data_dir, provider).await?;

        let reply = match provider.protocol {
            ProviderProtocol::OpenAiChat => {
                stream_openai_chat(&http, &provider, &cfg.model.model_id, &system, &messages, tx)
                    .await?
            }
            ProviderProtocol::OpenAiCodex => {
                stream_openai_codex(&http, &provider, &cfg.model.model_id, &system, &messages, tx)
                    .await?
            }
            ProviderProtocol::Anthropic => {
                stream_anthropic(&http, &provider, &cfg.model.model_id, &system, &messages, tx)
                    .await?
            }
        };

        total_in += reply.input_tokens;
        total_out += reply.output_tokens;

        let tool_calls = reply.tool_calls.clone();
        messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: reply.text,
            reasoning: if reply.reasoning.is_empty() {
                None
            } else {
                Some(reply.reasoning)
            },
            tool_calls: tool_calls.clone(),
            tool_call_id: None,
            tool_name: None,
        });

        if tool_calls.is_empty() {
            return Ok((messages, total_in, total_out));
        }

        for tc in tool_calls {
            let result =
                run_tool_call(&remote, &cfg.target, &cfg.cwd, &tc, tx, &cfg.asks).await;
            messages.push(ChatMessage {
                role: ChatRole::Tool,
                content: result,
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: Some(tc.id),
                tool_name: Some(tc.name),
            });
        }

        // Steering: deliver queued user text verbatim before the next model
        // call. No summarising, no reordering.
        let steered: Vec<String> = {
            let mut pending = cfg.steer.lock().unwrap();
            pending.drain(..).collect()
        };
        for text in steered {
            messages.push(ChatMessage {
                role: ChatRole::User,
                content: text,
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
                tool_name: None,
            });
        }
    }

    bail!("reached maximum tool steps ({MAX_TOOL_STEPS}) without a final response")
}

fn build_system_prompt(target: &str, cwd: &Path) -> String {
    let mut prompt = format!(
        "You are Envoy, a terminal coding and systems agent.\n\
         Active target machine: {target}\n\
         Working directory: {}\n\
         Operating system: {} {}\n\n\
         ## Rules\n\
         - Reply in the same language as the user's most recent message. If the user writes Chinese, answer in Chinese; never switch to English just because the code or tool output is English.\n\
         - Be precise and concise. Do the work; never substitute a summary of what you would do for actually doing it.\n\
         - If a request is ambiguous, ask one short question before acting. If a command is destructive (delete, overwrite, force-push, chmod -R, kill), confirm first.\n\
         - Prefer `read`/`edit`/`ls` over shell commands when inspecting or editing files. Relative paths resolve against the working directory.\n\
         - Use `exec` with a small wait for long commands, then `job` to follow or cancel. Report exact commands, job ids and exit codes; never invent tool output.\n\
         - Answer in Markdown. Use tables for comparisons, but keep them narrow enough for a terminal.\n\
         - Use `say` for short progress notes the user should see while you work (one sentence). Do not use it to restate the request.\n\
         - Use `ask` when a decision is genuinely ambiguous or a command is destructive. Give 2-5 options, mark exactly one `recommended`, and keep the safe option recommended for destructive actions. If nobody answers within the timeout, the recommended option is taken.\n\n\
         ## Machine tools (run on the active target machine)\n\
         - `exec`: run a shell command (detached daemon job, streams output)\n\
         - `read`: read a file\n\
         - `write`: write a whole file atomically\n\
         - `edit`: replace an exact snippet in a file\n\
         - `ls`: list a directory\n\
         - `job`: inspect, follow logs of (action \"wait\"), or cancel detached jobs\n\
         ## User tools (not machine operations)\n\
         - `say`: report progress to the user\n\
         - `ask`: ask the user to choose; blocks until answered or timeout",
        cwd.display(),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    if let Some(extra) = project_instructions(cwd) {
        prompt.push_str("\n\n## Project instructions\n");
        prompt.push_str(&extra);
    }
    prompt
}

/// `AGENTS.md` (or `ENVOY.md`) from the working directory, capped so a huge
/// file cannot crowd out the conversation.
fn project_instructions(cwd: &Path) -> Option<String> {
    const CAP: usize = 16_000;
    for name in ["AGENTS.md", "ENVOY.md"] {
        let Ok(text) = std::fs::read_to_string(cwd.join(name)) else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut out: String = trimmed.chars().take(CAP).collect();
        if trimmed.chars().count() > CAP {
            out.push_str("\n…[truncated]");
        }
        return Some(out);
    }
    None
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-prompt-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn ask_broker_routes_an_answer_once() {
        let asks = AskBroker::new();
        let (tx, rx) = oneshot::channel();
        asks.register(7, tx);
        assert!(asks.answer(7, "yes".into()));
        assert_eq!(rx.await.unwrap(), "yes");
        assert!(!asks.answer(7, "again".into()));
    }

    #[tokio::test]
    async fn ask_tool_returns_the_chosen_option() {
        let asks = AskBroker::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let answerer = asks.clone();
        tokio::spawn(async move {
            if let Some(AgentEvent::Ask { id, .. }) = rx.recv().await {
                answerer.answer(id, "B".into());
            }
        });
        let out = ask_tool(
            &asks,
            &tx,
            &json!({
                "question": "which?",
                "options": [{"label": "A"}, {"label": "B", "recommended": true}],
                "timeout_s": 5
            }),
        )
        .await
        .unwrap();
        assert!(out.contains("\"answer\":\"B\""), "{out}");
        assert!(out.contains("\"via\":\"user\""), "{out}");
    }

    #[test]
    fn prompt_pins_the_language_and_environment() {
        let dir = tmpdir("lang");
        let prompt = build_system_prompt("laptop", &dir);
        assert!(prompt.contains("same language as the user's most recent message"));
        assert!(prompt.contains("Chinese"));
        assert!(prompt.contains("laptop"));
        assert!(prompt.contains(&dir.display().to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_toolset_includes_say_and_ask() {
        let names: Vec<String> = openai_tools_json()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
            .collect();
        assert!(names.contains(&"say".to_string()), "{names:?}");
        assert!(names.contains(&"ask".to_string()), "{names:?}");
        assert!(names.contains(&"exec".to_string()), "{names:?}");
        let prompt = build_system_prompt("local", std::path::Path::new("/tmp"));
        assert!(prompt.contains("`say`"));
        assert!(prompt.contains("`ask`"));
        assert!(prompt.contains("recommended"));
    }

    #[test]
    fn agents_md_is_included_and_capped() {
        let dir = tmpdir("agents");
        std::fs::write(dir.join("AGENTS.md"), "# House rules\nUse tabs.").unwrap();
        let prompt = build_system_prompt("local", &dir);
        assert!(prompt.contains("Use tabs."));
        assert!(prompt.contains("Project instructions"));

        std::fs::write(dir.join("AGENTS.md"), "x".repeat(20_000)).unwrap();
        let prompt = build_system_prompt("local", &dir);
        assert!(prompt.contains("truncated"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn openai_tools_json() -> Vec<Value> {
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

fn responses_tools_json() -> Vec<Value> {
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

fn anthropic_tools_json() -> Vec<Value> {
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

// ------------------------------------------------------------------ Tool Execution

fn resolve_path(cwd: &Path, raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "." {
        return cwd.display().to_string();
    }
    if trimmed.starts_with('/') || trimmed.starts_with("~/") || trimmed == "~" {
        return trimmed.to_string();
    }
    cwd.join(trimmed).display().to_string()
}

fn summarize_tool(name: &str, args: &Value) -> (String, String) {
    match name {
        "exec" => {
            let cmd = args["command"].as_str().unwrap_or("");
            (format!("$ {cmd}"), String::new())
        }
        "read" => (
            format!("read {}", args["path"].as_str().unwrap_or("")),
            String::new(),
        ),
        "write" => (
            format!("write {}", args["path"].as_str().unwrap_or("")),
            format!("{}B", args["content"].as_str().map(|s| s.len()).unwrap_or(0)),
        ),
        "edit" => (
            format!("edit {}", args["path"].as_str().unwrap_or("")),
            String::new(),
        ),
        "ls" => (
            format!("ls {}", args["path"].as_str().unwrap_or(".")),
            String::new(),
        ),
        "job" => (
            format!(
                "job {} {}",
                args["action"].as_str().unwrap_or(""),
                args["id"].as_u64().map(|id| format!("#{id}")).unwrap_or_default()
            ),
            String::new(),
        ),
        other => (other.to_string(), String::new()),
    }
}

async fn run_tool_call(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    tc: &ToolCall,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    asks: &Arc<AskBroker>,
) -> String {
    let args: Value = serde_json::from_str(&tc.arguments).unwrap_or_else(|_| json!({}));
    let (title, subtitle) = summarize_tool(&tc.name, &args);
    let _ = tx.send(AgentEvent::ToolStart {
        call_id: tc.id.clone(),
        name: tc.name.clone(),
        title,
        subtitle,
    });

    let start = Instant::now();
    let res = match tc.name.as_str() {
        "exec" => exec_tool(remote, target, cwd, &tc.id, &args, tx).await,
        "read" => read_tool(remote, target, cwd, &args).await.map(|s| (s, None)),
        "write" => write_tool(remote, target, cwd, &args).await.map(|s| (s, None)),
        "edit" => edit_tool(remote, target, cwd, &args).await.map(|s| (s, None)),
        "ls" => ls_tool(remote, target, cwd, &args).await.map(|s| (s, None)),
        "job" => job_tool(remote, target, &args).await.map(|s| (s, None)),
        "say" => {
            let text = args["text"].as_str().unwrap_or("").trim().to_string();
            let kind = args["kind"].as_str().unwrap_or("progress").to_string();
            if !text.is_empty() {
                let _ = tx.send(AgentEvent::Say { text, kind });
            }
            Ok(("ok".to_string(), None))
        }
        "ask" => ask_tool(asks, tx, &args).await.map(|s| (s, None)),
        other => Err(anyhow!("unknown tool `{other}`")),
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    match res {
        Ok((text, exit_code)) => {
            let ok = exit_code.map(|c| c == 0).unwrap_or(true);
            if tc.name != "exec" && !text.is_empty() {
                let preview = truncate_tool_preview(&text, 4096);
                let _ = tx.send(AgentEvent::ToolOutput {
                    call_id: tc.id.clone(),
                    text: preview,
                    err: false,
                });
            }
            let _ = tx.send(AgentEvent::ToolDone {
                call_id: tc.id.clone(),
                ok,
                duration_ms,
                exit_code,
            });
            truncate_tool_output(&text)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let _ = tx.send(AgentEvent::ToolOutput {
                call_id: tc.id.clone(),
                text: msg.clone(),
                err: true,
            });
            let _ = tx.send(AgentEvent::ToolDone {
                call_id: tc.id.clone(),
                ok: false,
                duration_ms,
                exit_code: Some(1),
            });
            format!("error: {msg}")
        }
    }
}

async fn exec_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    call_id: &str,
    args: &Value,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<(String, Option<i32>)> {
    let command = args["command"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .context("missing `command`")?
        .to_string();
    let job_cwd = args["cwd"]
        .as_str()
        .map(|s| resolve_path(cwd, s))
        .unwrap_or_else(|| cwd.display().to_string());
    let timeout_s = args["timeout_s"].as_u64().unwrap_or(120).min(3600);

    let req = SubmitRequest {
        spec: TaskSpec::Shell { command },
        cwd: Some(job_cwd),
        env: Vec::new(),
        label: None,
        group: DEFAULT_GROUP.to_string(),
        timeout_s,
    };
    let job = remote.submit(target, req).await?;
    let _ = tx.send(AgentEvent::ToolJob {
        call_id: call_id.to_string(),
        job_id: job.id,
    });

    let mut out_all = Vec::new();
    let mut err_all = Vec::new();
    let mut cursor = 0u64;
    let deadline = Instant::now() + Duration::from_secs(timeout_s.max(5));

    loop {
        let mut batch_out = String::new();
        let mut batch_err = String::new();
        let (next, finished) = remote
            .logs(target, job.id, cursor, false, |records: &[LogRecord]| {
                for r in records {
                    let s = String::from_utf8_lossy(&r.data);
                    if r.fd == 1 {
                        err_all.extend_from_slice(&r.data);
                        batch_err.push_str(&s);
                    } else {
                        out_all.extend_from_slice(&r.data);
                        batch_out.push_str(&s);
                    }
                }
            })
            .await?;
        cursor = next;
        if !batch_out.is_empty() {
            let _ = tx.send(AgentEvent::ToolOutput {
                call_id: call_id.to_string(),
                text: batch_out,
                err: false,
            });
        }
        if !batch_err.is_empty() {
            let _ = tx.send(AgentEvent::ToolOutput {
                call_id: call_id.to_string(),
                text: batch_err,
                err: true,
            });
        }
        if let Some(code) = finished {
            let mut text = String::from_utf8_lossy(&out_all).to_string();
            if !err_all.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str("[stderr]\n");
                text.push_str(&String::from_utf8_lossy(&err_all));
            }
            text.push_str(&format!("\n[job #{} exit={code}]", job.id));
            return Ok((text, Some(code)));
        }
        if Instant::now() >= deadline {
            let text = format!(
                "{}\n[job #{} still running after {}s; use `job` tool with action=logs id={}]",
                String::from_utf8_lossy(&out_all),
                job.id,
                timeout_s,
                job.id
            );
            return Ok((text, None));
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

async fn read_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<String> {
    let raw_path = args["path"].as_str().context("missing `path`")?;
    let path = resolve_path(cwd, raw_path);
    let offset = args["offset"].as_u64().unwrap_or(0);
    let limit = args["limit"].as_u64().unwrap_or(0);
    let (size, _mode, data) = remote.read(target, &path, offset, limit).await?;
    let body = String::from_utf8_lossy(&data).to_string();
    if limit == 0 && size > READ_LIMIT {
        Ok(format!("{body}\n\n[{} bytes total; truncated]", size))
    } else {
        Ok(body)
    }
}

async fn write_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<String> {
    let raw_path = args["path"].as_str().context("missing `path`")?;
    let path = resolve_path(cwd, raw_path);
    let content = args["content"].as_str().context("missing `content`")?;
    let bytes = remote
        .write(target, &path, content.as_bytes(), None)
        .await?;
    Ok(format!("wrote {bytes} bytes to {path}"))
}

async fn edit_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    cwd: &Path,
    args: &Value,
) -> Result<String> {
    let raw_path = args["path"].as_str().context("missing `path`")?;
    let path = resolve_path(cwd, raw_path);
    let old = args["old"].as_str().context("missing `old`")?;
    let new = args["new"].as_str().context("missing `new`")?;
    let replace_all = args["replace_all"].as_bool().unwrap_or(false);
    let n = remote.edit(target, &path, old, new, replace_all).await?;
    Ok(format!("edited {path}: {n} replacement(s)"))
}

async fn ls_tool(
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

async fn ask_tool(
    asks: &Arc<AskBroker>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    args: &Value,
) -> Result<String> {
    let question = args["question"]
        .as_str()
        .filter(|q| !q.trim().is_empty())
        .context("missing `question`")?;
    let list = args["options"]
        .as_array()
        .context("`ask` needs at least one option")?;
    let mut options = Vec::new();
    for item in list {
        if let Some(label) = item["label"].as_str() {
            options.push(AskOption {
                label: label.to_string(),
                description: item["description"].as_str().map(str::to_string),
                recommended: item["recommended"].as_bool().unwrap_or(false),
            });
        }
    }
    if options.is_empty() {
        bail!("`ask` needs at least one option");
    }
    let recommended = options
        .iter()
        .position(|o| o.recommended)
        .unwrap_or(0);
    let timeout_s = args["timeout_s"].as_u64().unwrap_or(300).clamp(5, 3600);
    let id = asks.next_id();
    let (send, recv) = oneshot::channel();
    asks.register(id, send);
    let _ = tx.send(AgentEvent::Ask {
        id,
        question: question.to_string(),
        options: options.clone(),
        recommended,
        timeout_ms: timeout_s * 1000,
    });

    let answer = tokio::time::timeout(Duration::from_secs(timeout_s), recv).await;
    asks.forget(id);
    let (label, via) = match answer {
        Ok(Ok(label)) => (label, "user"),
        // Timeout or the UI went away: take the recommended option.
        _ => {
            let _ = tx.send(AgentEvent::AskClosed { id });
            (options[recommended].label.clone(), "timeout")
        }
    };
    Ok(json!({ "answer": label, "via": via }).to_string())
}

async fn job_tool(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    args: &Value,
) -> Result<String> {
    let action = args["action"].as_str().context("missing `action`")?;
    match action {
        "list" => {
            let jobs = remote.jobs(target).await?;
            if jobs.is_empty() {
                return Ok("no jobs".into());
            }
            let mut out = String::new();
            for j in jobs {
                out.push_str(&format!(
                    "#{} {} {}\n",
                    j.id,
                    j.state.label(),
                    j.command()
                ));
            }
            Ok(out)
        }
        "status" => {
            let id = args["id"].as_u64().context("missing `id`")?;
            let j = remote.status(target, id).await?;
            Ok(format!(
                "#{} {} cwd={} cmd={}",
                j.id,
                j.state.label(),
                j.cwd,
                j.command()
            ))
        }
        "logs" => {
            let id = args["id"].as_u64().context("missing `id`")?;
            let offset = args["offset"].as_u64().unwrap_or(0);
            let mut text = String::new();
            let (next, finished) = remote
                .logs(target, id, offset, false, |recs| {
                    for r in recs {
                        text.push_str(&String::from_utf8_lossy(&r.data));
                    }
                })
                .await?;
            text.push_str(&format!("\n[next={next} finished={finished:?}]"));
            Ok(text)
        }
        "wait" => {
            let id = args["id"].as_u64().context("missing `id`")?;
            let timeout_s = args["timeout_s"].as_u64().unwrap_or(300).clamp(5, 3600);
            let deadline = Instant::now() + Duration::from_secs(timeout_s);
            let mut offset = 0u64;
            let mut text = String::new();
            loop {
                let (next, finished) = remote
                    .logs(target, id, offset, false, |recs| {
                        for r in recs {
                            text.push_str(&String::from_utf8_lossy(&r.data));
                        }
                    })
                    .await?;
                offset = next;
                if let Some(code) = finished {
                    return Ok(format!("{text}\n[job #{id} exit {code}]"));
                }
                if Instant::now() >= deadline {
                    return Ok(format!(
                        "{text}\n[job #{id} still running after {timeout_s}s; use job wait again]"
                    ));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        "cancel" => {
            let id = args["id"].as_u64().context("missing `id`")?;
            remote.cancel(target, id).await?;
            Ok(format!("cancel requested for #{id}"))
        }
        other => bail!("unknown job action `{other}`"),
    }
}

fn truncate_tool_output(s: &str) -> String {
    if s.len() <= TOOL_OUTPUT_CAP {
        return s.to_string();
    }
    let mut end = TOOL_OUTPUT_CAP;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}\n[truncated]", &s[..end])
}

fn truncate_tool_preview(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}\n…", &s[..end])
}

// ------------------------------------------------------------------ Streaming LLM Protocols

struct StreamReply {
    text: String,
    reasoning: String,
    tool_calls: Vec<ToolCall>,
    input_tokens: u64,
    output_tokens: u64,
}

async fn stream_openai_chat(
    http: &reqwest::Client,
    provider: &StoredProvider,
    model: &str,
    system: &str,
    history: &[ChatMessage],
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    let (bearer, base_url) = match &provider.credential {
        Credential::ApiKey { key } => (key.clone(), provider.base_url.clone()),
        Credential::OAuth {
            access_token,
            method_id,
            ..
        } => {
            if method_id == "github-copilot" {
                copilot_session_token(http, access_token).await?
            } else {
                (access_token.clone(), provider.base_url.clone())
            }
        }
    };

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut msgs = vec![json!({ "role": "system", "content": system })];
    for m in history {
        match m.role {
            ChatRole::System => {}
            ChatRole::User => msgs.push(json!({ "role": "user", "content": m.content })),
            ChatRole::Assistant => {
                if m.tool_calls.is_empty() {
                    msgs.push(json!({ "role": "assistant", "content": m.content }));
                } else {
                    let calls: Vec<Value> = m
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.arguments,
                                }
                            })
                        })
                        .collect();
                    msgs.push(json!({
                        "role": "assistant",
                        "content": if m.content.is_empty() { Value::Null } else { json!(m.content) },
                        "tool_calls": calls,
                    }));
                }
            }
            ChatRole::Tool => {
                msgs.push(json!({
                    "role": "tool",
                    "tool_call_id": m.tool_call_id.as_deref().unwrap_or(""),
                    "content": m.content,
                }));
            }
        }
    }

    let body = json!({
        "model": model,
        "messages": msgs,
        "tools": openai_tools_json(),
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    let mut req = http
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream");
    if !bearer.is_empty() {
        req = req.bearer_auth(&bearer);
    }
    if provider.id == "github-copilot" {
        req = req
            .header("Editor-Version", "vscode/1.99.0")
            .header("Copilot-Integration-Id", "vscode-chat");
    }

    let resp = req.json(&body).send().await.context("calling LLM API")?;
    let status = resp.status();
    if !status.is_success() {
        let err_body = resp.text().await.unwrap_or_default();
        bail!("LLM API error ({status}): {err_body}");
    }

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut partial_calls: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;

    let mut sse = SseParser::default();
    let mut bytes = resp.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk?;
        for ev in sse.feed(&chunk) {
            let data = ev.data.trim();
            if data == "[DONE]" || data.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                input_tokens = u["prompt_tokens"].as_u64().unwrap_or(input_tokens);
                output_tokens = u["completion_tokens"].as_u64().unwrap_or(output_tokens);
            }
            let Some(choice) = v["choices"].as_array().and_then(|c| c.first()) else {
                continue;
            };
            let delta = &choice["delta"];
            if let Some(r) = delta["reasoning_content"]
                .as_str()
                .or_else(|| delta["reasoning"].as_str())
            {
                if !r.is_empty() {
                    reasoning.push_str(r);
                    let _ = tx.send(AgentEvent::ReasoningDelta(r.to_string()));
                }
            }
            if let Some(c) = delta["content"].as_str() {
                if !c.is_empty() {
                    text.push_str(c);
                    let _ = tx.send(AgentEvent::TextDelta(c.to_string()));
                }
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    let idx = call["index"].as_u64().unwrap_or(0) as usize;
                    let entry = partial_calls.entry(idx).or_default();
                    if let Some(id) = call["id"].as_str() {
                        entry.0 = id.to_string();
                    }
                    if let Some(name) = call["function"]["name"].as_str() {
                        entry.1.push_str(name);
                    }
                    if let Some(args) = call["function"]["arguments"].as_str() {
                        entry.2.push_str(args);
                    }
                }
            }
        }
    }

    let tool_calls = partial_calls
        .into_iter()
        .enumerate()
        .map(|(i, (_idx, (id, name, arguments)))| ToolCall {
            id: if id.is_empty() {
                format!("call_{i}")
            } else {
                id
            },
            name,
            arguments,
        })
        .filter(|tc| !tc.name.is_empty())
        .collect();

    Ok(StreamReply {
        text,
        reasoning,
        tool_calls,
        input_tokens,
        output_tokens,
    })
}

async fn stream_openai_codex(
    http: &reqwest::Client,
    provider: &StoredProvider,
    model: &str,
    system: &str,
    history: &[ChatMessage],
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    let (token, account_id) = match &provider.credential {
        Credential::OAuth {
            access_token,
            account_id,
            ..
        } => (access_token.clone(), account_id.clone()),
        Credential::ApiKey { key } => (key.clone(), None),
    };

    let mut input = Vec::new();
    for m in history {
        match m.role {
            ChatRole::System => {}
            ChatRole::User => {
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": m.content }],
                }));
            }
            ChatRole::Assistant => {
                if !m.content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": m.content }],
                    }));
                }
                for tc in &m.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": tc.name,
                        "arguments": tc.arguments,
                    }));
                }
            }
            ChatRole::Tool => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": m.tool_call_id.as_deref().unwrap_or(""),
                    "output": m.content,
                }));
            }
        }
    }

    let body = json!({
        "model": model,
        "instructions": system,
        "input": input,
        "tools": responses_tools_json(),
        "stream": true,
        "store": false,
    });

    let mut req = http
        .post(&provider.base_url)
        .bearer_auth(&token)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .header("originator", "envoy");
    if let Some(acct) = account_id {
        req = req.header("ChatGPT-Account-ID", acct);
    }

    let resp = req
        .json(&body)
        .send()
        .await
        .context("calling OpenAI Codex endpoint")?;
    let status = resp.status();
    if !status.is_success() {
        let err_body = resp.text().await.unwrap_or_default();
        bail!("Codex API error ({status}): {err_body}");
    }

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: BTreeMap<String, (String, String, String)> = BTreeMap::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;

    let mut sse = SseParser::default();
    let mut bytes = resp.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk?;
        for ev in sse.feed(&chunk) {
            let data = ev.data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            let kind = v["type"].as_str().unwrap_or(ev.event.as_str());
            match kind {
                "response.output_text.delta" => {
                    if let Some(d) = v["delta"].as_str() {
                        text.push_str(d);
                        let _ = tx.send(AgentEvent::TextDelta(d.to_string()));
                    }
                }
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    if let Some(d) = v["delta"].as_str() {
                        reasoning.push_str(d);
                        let _ = tx.send(AgentEvent::ReasoningDelta(d.to_string()));
                    }
                }
                "response.output_item.added" => {
                    let item = &v["item"];
                    if item["type"].as_str() == Some("function_call") {
                        let item_id = item["id"].as_str().unwrap_or("").to_string();
                        let call_id = item["call_id"].as_str().unwrap_or(&item_id).to_string();
                        let name = item["name"].as_str().unwrap_or("").to_string();
                        let args = item["arguments"].as_str().unwrap_or("").to_string();
                        calls.insert(item_id, (call_id, name, args));
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(item_id) = v["item_id"].as_str() {
                        if let Some(d) = v["delta"].as_str() {
                            if let Some(entry) = calls.get_mut(item_id) {
                                entry.2.push_str(d);
                            }
                        }
                    }
                }
                "response.output_item.done" => {
                    let item = &v["item"];
                    if item["type"].as_str() == Some("function_call") {
                        let item_id = item["id"].as_str().unwrap_or("").to_string();
                        let call_id = item["call_id"].as_str().unwrap_or(&item_id).to_string();
                        let name = item["name"].as_str().unwrap_or("").to_string();
                        let args = item["arguments"].as_str().unwrap_or("").to_string();
                        calls.insert(item_id, (call_id, name, args));
                    }
                }
                "response.completed" => {
                    let usage = &v["response"]["usage"];
                    input_tokens = usage["input_tokens"].as_u64().unwrap_or(input_tokens);
                    output_tokens = usage["output_tokens"].as_u64().unwrap_or(output_tokens);
                }
                "response.failed" | "error" => {
                    let msg = v["response"]["error"]["message"]
                        .as_str()
                        .or_else(|| v["error"]["message"].as_str())
                        .or_else(|| v["message"].as_str())
                        .unwrap_or("Codex stream error");
                    bail!("{msg}");
                }
                _ => {}
            }
        }
    }

    let tool_calls = calls
        .into_values()
        .map(|(id, name, arguments)| ToolCall {
            id,
            name,
            arguments,
        })
        .filter(|tc| !tc.name.is_empty())
        .collect();

    Ok(StreamReply {
        text,
        reasoning,
        tool_calls,
        input_tokens,
        output_tokens,
    })
}

async fn stream_anthropic(
    http: &reqwest::Client,
    provider: &StoredProvider,
    model: &str,
    system: &str,
    history: &[ChatMessage],
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<StreamReply> {
    let url = format!("{}/messages", provider.base_url.trim_end_matches('/'));
    let mut msgs: Vec<Value> = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();

    for m in history {
        if m.role != ChatRole::Tool && !pending_tool_results.is_empty() {
            msgs.push(json!({
                "role": "user",
                "content": std::mem::take(&mut pending_tool_results),
            }));
        }
        match m.role {
            ChatRole::System => {}
            ChatRole::User => {
                msgs.push(json!({ "role": "user", "content": m.content }));
            }
            ChatRole::Assistant => {
                let mut blocks = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": m.content }));
                }
                for tc in &m.tool_calls {
                    let input: Value = serde_json::from_str(&tc.arguments).unwrap_or_else(|_| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": input,
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
                msgs.push(json!({ "role": "assistant", "content": blocks }));
            }
            ChatRole::Tool => {
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.as_deref().unwrap_or(""),
                    "content": m.content,
                }));
            }
        }
    }
    if !pending_tool_results.is_empty() {
        msgs.push(json!({ "role": "user", "content": pending_tool_results }));
    }

    let body = json!({
        "model": model,
        "max_tokens": 8192,
        "system": system,
        "messages": msgs,
        "tools": anthropic_tools_json(),
        "stream": true,
    });

    let mut req = http
        .post(&url)
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream");

    match &provider.credential {
        Credential::ApiKey { key } => {
            req = req.header("x-api-key", key);
        }
        Credential::OAuth { access_token, .. } => {
            req = req
                .bearer_auth(access_token)
                .header(
                    "anthropic-beta",
                    "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14",
                );
        }
    }

    let resp = req
        .json(&body)
        .send()
        .await
        .context("calling Anthropic API")?;
    let status = resp.status();
    if !status.is_success() {
        let err_body = resp.text().await.unwrap_or_default();
        bail!("Anthropic API error ({status}): {err_body}");
    }

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut blocks: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;

    let mut sse = SseParser::default();
    let mut bytes = resp.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk?;
        for ev in sse.feed(&chunk) {
            let data = ev.data.trim();
            if data.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            match v["type"].as_str().unwrap_or("") {
                "message_start" => {
                    input_tokens = v["message"]["usage"]["input_tokens"]
                        .as_u64()
                        .unwrap_or(input_tokens);
                }
                "content_block_start" => {
                    let idx = v["index"].as_u64().unwrap_or(0) as usize;
                    let cb = &v["content_block"];
                    if cb["type"].as_str() == Some("tool_use") {
                        let id = cb["id"].as_str().unwrap_or("").to_string();
                        let name = cb["name"].as_str().unwrap_or("").to_string();
                        blocks.insert(idx, (id, name, String::new()));
                    }
                }
                "content_block_delta" => {
                    let idx = v["index"].as_u64().unwrap_or(0) as usize;
                    let d = &v["delta"];
                    match d["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            if let Some(t) = d["text"].as_str() {
                                text.push_str(t);
                                let _ = tx.send(AgentEvent::TextDelta(t.to_string()));
                            }
                        }
                        "thinking_delta" => {
                            if let Some(t) = d["thinking"].as_str() {
                                reasoning.push_str(t);
                                let _ = tx.send(AgentEvent::ReasoningDelta(t.to_string()));
                            }
                        }
                        "input_json_delta" => {
                            if let Some(j) = d["partial_json"].as_str() {
                                if let Some(entry) = blocks.get_mut(&idx) {
                                    entry.2.push_str(j);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    output_tokens = v["usage"]["output_tokens"]
                        .as_u64()
                        .unwrap_or(output_tokens);
                }
                "error" => {
                    let msg = v["error"]["message"]
                        .as_str()
                        .unwrap_or("Anthropic stream error");
                    bail!("{msg}");
                }
                _ => {}
            }
        }
    }

    let tool_calls = blocks
        .into_values()
        .map(|(id, name, args)| ToolCall {
            id,
            name,
            arguments: if args.is_empty() { "{}".into() } else { args },
        })
        .filter(|tc| !tc.name.is_empty())
        .collect();

    Ok(StreamReply {
        text,
        reasoning,
        tool_calls,
        input_tokens,
        output_tokens,
    })
}

#[derive(Default)]
struct SseEvent {
    event: String,
    data: String,
}

#[derive(Default)]
struct SseParser {
    buf: String,
    cur_event: String,
    cur_data: Vec<String>,
}

impl SseParser {
    fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find('\n') {
            let line = self.buf[..pos].trim_end_matches('\r').to_string();
            self.buf.drain(..=pos);
            if line.is_empty() {
                if !self.cur_data.is_empty() {
                    out.push(SseEvent {
                        event: std::mem::take(&mut self.cur_event),
                        data: std::mem::take(&mut self.cur_data).join("\n"),
                    });
                } else {
                    self.cur_event.clear();
                }
            } else if let Some(rest) = line.strip_prefix("event:") {
                self.cur_event = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                self.cur_data.push(rest.trim_start().to_string());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_handles_chunked_events() {
        let mut p = SseParser::default();
        assert!(p.feed(b"event: response.output_text.delta\n").is_empty());
        assert!(p.feed(b"data: {\"delta\":\"hel").is_empty());
        let evs = p.feed(b"lo\"}\n\ndata: [DONE]\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].event, "response.output_text.delta");
        assert_eq!(evs[0].data, "{\"delta\":\"hello\"}");
        assert_eq!(evs[1].data, "[DONE]");
    }

    #[test]
    fn resolve_path_uses_cwd_for_relative() {
        let p = resolve_path(Path::new("/tmp/proj"), "src/main.rs");
        assert_eq!(p, "/tmp/proj/src/main.rs");
        assert_eq!(resolve_path(Path::new("/tmp/proj"), "/etc/hosts"), "/etc/hosts");
    }
}
