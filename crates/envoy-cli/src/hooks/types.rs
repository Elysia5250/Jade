//! Core types for Deterministic Lifecycle Hooks and explicit `HookContext` state transitions.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Maximum times `Stop` / `SubagentStop` can block and re-enter the agent loop.
pub const MAX_STOP_HOOK_REENTRIES: u8 = 2;

/// Default per-hook execution timeout (ms).
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 5_000;

/// Maximum configurable per-hook execution timeout (ms).
pub const MAX_HOOK_TIMEOUT_MS: u64 = 30_000;

/// Per-hook timeout ceiling for `SessionEnd` during daemon shutdown (fits inside 3s SIGTERM window).
pub const SHUTDOWN_HOOK_TIMEOUT_MS: u64 = 1_500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreCompact,
    PreModelCall,
    PostModelCall,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    SubagentStart,
    SubagentStop,
    Stop,
    PostRewind,
    SessionEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageCapability {
    /// `Stop`, `SubagentStop`: can block & re-enter the loop up to `MAX_STOP_HOOK_REENTRIES`.
    ReentrantBlockable,
    /// `UserPromptSubmit`, `PreModelCall`, `PostModelCall`, `PreToolUse`, `SubagentStart`.
    BlockableWithAsk,
    /// `PreCompact`: can block auto-compaction, no `Ask`.
    BlockableNoAsk,
    /// `SessionStart`, `PostToolUse`, `PostToolUseFailure`, `PostRewind`, `SessionEnd`.
    ObserveOnly,
}

impl HookEvent {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "SessionStart" | "session_start" => Some(Self::SessionStart),
            "UserPromptSubmit" | "user_prompt_submit" => Some(Self::UserPromptSubmit),
            "PreCompact" | "pre_compact" => Some(Self::PreCompact),
            "PreModelCall" | "pre_model_call" => Some(Self::PreModelCall),
            "PostModelCall" | "post_model_call" => Some(Self::PostModelCall),
            "PreToolUse" | "pre_tool_use" => Some(Self::PreToolUse),
            "PostToolUse" | "post_tool_use" => Some(Self::PostToolUse),
            "PostToolUseFailure" | "post_tool_use_failure" => Some(Self::PostToolUseFailure),
            "SubagentStart" | "subagent_start" => Some(Self::SubagentStart),
            "SubagentStop" | "subagent_stop" => Some(Self::SubagentStop),
            "Stop" | "stop" => Some(Self::Stop),
            "PostRewind" | "post_rewind" => Some(Self::PostRewind),
            "SessionEnd" | "session_end" => Some(Self::SessionEnd),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreCompact => "PreCompact",
            Self::PreModelCall => "PreModelCall",
            Self::PostModelCall => "PostModelCall",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::Stop => "Stop",
            Self::PostRewind => "PostRewind",
            Self::SessionEnd => "SessionEnd",
        }
    }

    pub fn capability(self) -> StageCapability {
        match self {
            Self::Stop | Self::SubagentStop => StageCapability::ReentrantBlockable,
            Self::UserPromptSubmit
            | Self::PreModelCall
            | Self::PostModelCall
            | Self::PreToolUse
            | Self::SubagentStart => StageCapability::BlockableWithAsk,
            Self::PreCompact => StageCapability::BlockableNoAsk,
            Self::SessionStart
            | Self::PostToolUse
            | Self::PostToolUseFailure
            | Self::PostRewind
            | Self::SessionEnd => StageCapability::ObserveOnly,
        }
    }
}

/// Strictly ordered rule scope: `Managed < User < Project`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookScope {
    Managed = 0,
    User = 1,
    Project = 2,
}

impl HookScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum HookDecision {
    #[default]
    Continue,
    Ask {
        reason: String,
        by: String,
        scope: HookScope,
    },
    Block {
        reason: String,
        by: String,
        scope: HookScope,
    },
    Deny {
        reason: String,
        by: String,
        scope: HookScope,
    },
}

impl HookDecision {
    pub fn is_blocking(&self) -> bool {
        matches!(self, Self::Block { .. } | Self::Deny { .. })
    }

    pub fn is_ask(&self) -> bool {
        matches!(self, Self::Ask { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Continue => None,
            Self::Ask { reason, .. } | Self::Block { reason, .. } | Self::Deny { reason, .. } => {
                Some(reason.as_str())
            }
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Ask { .. } => "ask",
            Self::Block { .. } => "block",
            Self::Deny { .. } => "deny",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookStepStatus {
    Succeeded,
    Asked,
    Blocked,
    Denied,
    Errored,
    TimedOut,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookStepTrace {
    pub id: String,
    pub scope: HookScope,
    pub priority: i32,
    pub decl_index: usize,
    pub status: HookStepStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub reason: String,
    pub mutated_keys: Vec<String>,
}

/// Explicit state container passed through every hook in strict execution order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookContext {
    pub session_id: String,
    pub turn_seq: u64,
    pub step: usize,
    pub depth: u8,
    pub event: HookEvent,
    pub cwd: PathBuf,
    pub target: String,
    pub permission_mode: String,
    pub can_block: bool,
    pub stop_hook_active: bool,
    pub stop_hook_count: u8,

    // Stage payloads
    pub prompt: Option<String>,
    pub model_id: Option<String>,
    pub estimated_tokens: Option<u64>,
    pub context_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub assistant_text: Option<String>,
    pub planned_tools: Vec<String>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_output: Option<String>,
    pub tool_ok: Option<bool>,
    pub subagent_name: Option<String>,
    pub subagent_ok: Option<bool>,
    pub compact_auto: Option<bool>,
    pub rewind_checkpoint_id: Option<u64>,
    pub shutdown_reason: Option<String>,

    // Explicit transitions & trace
    pub decision: HookDecision,
    pub additional_context: Vec<String>,
    pub state: BTreeMap<String, Value>,
    pub trace: Vec<HookStepTrace>,
}

impl HookContext {
    pub fn new(
        session_id: impl Into<String>,
        cwd: PathBuf,
        target: impl Into<String>,
        permission_mode: impl Into<String>,
        event: HookEvent,
        state: BTreeMap<String, Value>,
    ) -> Self {
        let can_block = !matches!(event.capability(), StageCapability::ObserveOnly);
        Self {
            session_id: session_id.into(),
            turn_seq: 0,
            step: 0,
            depth: 0,
            event,
            cwd,
            target: target.into(),
            permission_mode: permission_mode.into(),
            can_block,
            stop_hook_active: false,
            stop_hook_count: 0,
            prompt: None,
            model_id: None,
            estimated_tokens: None,
            context_tokens: None,
            input_tokens: None,
            output_tokens: None,
            assistant_text: None,
            planned_tools: Vec::new(),
            tool_name: None,
            tool_call_id: None,
            tool_input: None,
            tool_output: None,
            tool_ok: None,
            subagent_name: None,
            subagent_ok: None,
            compact_auto: None,
            rewind_checkpoint_id: None,
            shutdown_reason: None,
            decision: HookDecision::Continue,
            additional_context: Vec::new(),
            state,
            trace: Vec::new(),
        }
    }
}

/// Declarative stdout JSON output parsed from a hook command.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookOutput {
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default, alias = "updatedInput")]
    pub updated_input: Option<Value>,
    #[serde(default, alias = "updatedPrompt")]
    pub updated_prompt: Option<String>,
    #[serde(default, alias = "additionalContext")]
    pub additional_context: Option<String>,
    #[serde(default, alias = "statePatch")]
    pub state_patch: Option<BTreeMap<String, Value>>,
    #[serde(default, alias = "unsetStateKeys")]
    pub unset_state_keys: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct HookRule {
    pub id: String,
    pub scope: HookScope,
    pub priority: i32,
    pub decl_index: usize,
    pub event: HookEvent,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout_ms: u64,
    pub fail_closed: bool,
    pub always_run: bool,
    /// User and managed hooks always run directly. Project hooks run under the
    /// workspace sandbox unless the entry set `sandbox` to `off`.
    pub sandbox_off: bool,
}

impl HookRule {
    pub fn sort_key(&self) -> (HookScope, i32, usize) {
        (self.scope, self.priority, self.decl_index)
    }
}
