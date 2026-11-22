//! Core types for permission modes, rules, policy files, and workspace trust.

use jade_proto::SandboxPolicy;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    #[default]
    Default,
    AcceptEdits,
    Plan,
    Auto,
    /// Never prompt. Explicit deny rules still win; nothing else does.
    ///
    /// Appended last on purpose: postcard encodes the variant index, so new
    /// modes must never be inserted in the middle.
    Turbo,
}

impl PermissionMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Default => Self::AcceptEdits,
            Self::AcceptEdits => Self::Plan,
            Self::Plan => Self::Auto,
            Self::Auto => Self::Turbo,
            Self::Turbo => Self::Default,
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "default" | "ask" => Some(Self::Default),
            "accept-edits" | "acceptedits" | "edits" => Some(Self::AcceptEdits),
            "plan" | "readonly" | "read-only" => Some(Self::Plan),
            "auto" | "allow" => Some(Self::Auto),
            "turbo" | "yolo" => Some(Self::Turbo),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "accept-edits",
            Self::Plan => "plan",
            Self::Auto => "auto",
            Self::Turbo => "turbo",
        }
    }

    pub fn short_badge(self) -> &'static str {
        match self {
            Self::Default => "perm:ask",
            Self::AcceptEdits => "perm:edits",
            Self::Plan => "perm:plan",
            Self::Auto => "perm:auto",
            Self::Turbo => "perm:turbo",
        }
    }

    /// Privilege rank for model-driven mode transitions:
    /// `Plan (0) < Default (1) < AcceptEdits (2) < Auto (3) < Turbo (4)`.
    pub fn privilege_rank(self) -> u8 {
        match self {
            Self::Plan => 0,
            Self::Default => 1,
            Self::AcceptEdits => 2,
            Self::Auto => 3,
            Self::Turbo => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleScope {
    Managed,
    User,
    Project,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub scope: RuleScope,
    pub effect: Effect,
    pub pattern: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyFile {
    #[serde(default)]
    pub default_mode: Option<PermissionMode>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub extra_rw: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrustStore {
    #[serde(default)]
    pub trusted_dirs: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub effect: Effect,
    pub reason: String,
    pub suggested_rule: Option<String>,
    pub needs_net: bool,
    pub extra_rw: Vec<String>,
    pub outside_workspace: bool,
    pub destructive: bool,
}

#[derive(Debug, Clone)]
pub struct ToolAuth {
    pub sandbox: SandboxPolicy,
    /// Canonical path authorized for `write`/`edit` calls, used to detect TOCTOU
    /// symlink swaps before writing.
    pub resolved_path: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxOverride {
    WorkspaceWrite,
    ReadOnly,
    Off,
    DangerFullAccess,
}

impl SandboxOverride {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "workspace-write" | "ws" => Some(Self::WorkspaceWrite),
            "read-only" | "ro" | "readonly" => Some(Self::ReadOnly),
            "off" | "none" => Some(Self::Off),
            "danger-full-access" | "full" | "unrestricted" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }
}

/// Session permission-mode state, tracking pre-plan mode and written plan files.
#[derive(Debug, Clone)]
pub(super) struct ModeState {
    pub(super) mode: PermissionMode,
    pub(super) previous_mode: Option<PermissionMode>,
    pub(super) plan_files: Vec<std::path::PathBuf>,
}

impl ModeState {
    pub(super) fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            previous_mode: None,
            plan_files: Vec::new(),
        }
    }

    pub(super) fn set_mode(&mut self, next: PermissionMode) -> bool {
        if self.mode == next {
            return false;
        }
        if next == PermissionMode::Plan {
            self.previous_mode = Some(self.mode);
            self.plan_files.clear();
        } else {
            self.previous_mode = None;
            self.plan_files.clear();
        }
        self.mode = next;
        true
    }

    pub(super) fn plan_exit_target(&self) -> PermissionMode {
        match self.previous_mode {
            Some(prev) if prev != PermissionMode::Plan => prev,
            _ => PermissionMode::AcceptEdits,
        }
    }

    pub(super) fn record_plan_file(&mut self, canonical: std::path::PathBuf, non_empty: bool) {
        if self.mode != PermissionMode::Plan {
            return;
        }
        self.plan_files.retain(|p| p != &canonical);
        if non_empty {
            self.plan_files.push(canonical);
        }
    }

    pub(super) fn resolve_plan_file(&self, canonical: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
        if self.mode != PermissionMode::Plan {
            return None;
        }
        match canonical {
            Some(target) => self.plan_files.iter().find(|p| p.as_path() == target).cloned(),
            None => self.plan_files.last().cloned(),
        }
    }

    pub(super) fn latest_plan_file(&self) -> Option<std::path::PathBuf> {
        (self.mode == PermissionMode::Plan)
            .then(|| self.plan_files.last().cloned())
            .flatten()
    }
}
