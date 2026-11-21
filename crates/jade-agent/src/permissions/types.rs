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
