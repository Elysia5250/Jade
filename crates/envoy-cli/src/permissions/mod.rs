//! Permission engine: modes, rules, path/shell guards, approvals, and audit.
//!
//! No `ratatui` or provider types live here. The rule engine decides `allow`,
//! `ask`, or `deny` and selects the [`SandboxPolicy`]; the OS sandbox enforces
//! the real boundary.

pub mod audit;
pub mod paths;
mod prompt;
pub mod rules;
pub mod shell;
pub mod types;

use crate::agent::{AgentEvent, AskBroker};
use anyhow::{bail, Result};
use envoy_proto::SandboxPolicy;
use prompt::{prompt_user_approval, summarize_call, ApprovalChoice};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub use rules::{is_workspace_trusted, set_workspace_trusted};
pub use types::{
    Decision, Effect, PermissionMode, PermissionRule, PolicyFile, RuleScope, SandboxOverride,
    ToolAuth,
};

#[derive(Debug)]
pub struct PermissionEngine {
    data_dir: PathBuf,
    cwd: PathBuf,
    session_id: String,
    target: String,
    mode: Mutex<PermissionMode>,
    session_allows: Mutex<Vec<PermissionRule>>,
    managed_override: Option<PathBuf>,
    skip_permissions: Mutex<bool>,
    sandbox_override: Mutex<Option<SandboxOverride>>,
}

impl PermissionEngine {
    pub fn new(
        data_dir: PathBuf,
        cwd: PathBuf,
        session_id: String,
        target: String,
        mode: PermissionMode,
    ) -> Arc<Self> {
        let trusted = is_workspace_trusted(&data_dir, &cwd);
        let policies = rules::load_policies(&data_dir, &cwd, trusted, None, &[]);
        let effective_mode = if mode == PermissionMode::Default {
            policies.default_mode.unwrap_or(mode)
        } else {
            mode
        };
        Arc::new(Self {
            data_dir,
            cwd,
            session_id,
            target,
            mode: Mutex::new(effective_mode),
            session_allows: Mutex::new(Vec::new()),
            managed_override: None,
            skip_permissions: Mutex::new(false),
            sandbox_override: Mutex::new(None),
        })
    }

    pub fn fork_for_subagent(
        &self,
        mode: PermissionMode,
        inherit_session_allows: bool,
    ) -> Arc<Self> {
        let session_allows = if inherit_session_allows {
            self.session_allows.lock().unwrap().clone()
        } else {
            Vec::new()
        };
        Arc::new(Self {
            data_dir: self.data_dir.clone(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            target: self.target.clone(),
            mode: Mutex::new(mode),
            session_allows: Mutex::new(session_allows),
            managed_override: self.managed_override.clone(),
            skip_permissions: Mutex::new(*self.skip_permissions.lock().unwrap()),
            sandbox_override: Mutex::new(*self.sandbox_override.lock().unwrap()),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_managed_override(mut self, path: PathBuf) -> Self {
        self.managed_override = Some(path);
        self
    }

    pub fn mode(&self) -> PermissionMode {
        *self.mode.lock().unwrap()
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.lock().unwrap() = mode;
    }

    pub fn set_skip_permissions(&self, skip: bool) {
        *self.skip_permissions.lock().unwrap() = skip;
    }

    pub fn skip_permissions(&self) -> bool {
        *self.skip_permissions.lock().unwrap()
    }

    pub fn set_sandbox_override(&self, sandbox: Option<SandboxOverride>) {
        *self.sandbox_override.lock().unwrap() = sandbox;
    }

    pub fn sandbox_override(&self) -> Option<SandboxOverride> {
        *self.sandbox_override.lock().unwrap()
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_trusted(&self) -> bool {
        is_workspace_trusted(&self.data_dir, &self.cwd)
    }

    pub fn loaded_policies(&self) -> rules::LoadedPolicies {
        let session = self.session_allows.lock().unwrap().clone();
        rules::load_policies(
            &self.data_dir,
            &self.cwd,
            self.is_trusted(),
            self.managed_override.as_deref(),
            &session,
        )
    }

    pub fn is_tool_enabled(&self, tool: &str) -> bool {
        let policies = self.loaded_policies();
        if rules::is_tool_denied(&policies, tool) {
            return false;
        }
        if self.mode() == PermissionMode::Plan {
            if tool.starts_with("mcp__") {
                return false;
            }
        }
        true
    }

    pub fn evaluate(&self, tool: &str, args: &Value) -> Decision {
        let policies = self.loaded_policies();
        rules::evaluate_tool_call(&self.data_dir, &self.cwd, self.mode(), &policies, tool, args)
    }

    pub async fn authorize_tool_call(
        &self,
        tool: &str,
        args: &Value,
        asks: &Arc<AskBroker>,
        tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<ToolAuth> {
        let policies = self.loaded_policies();
        let mode = self.mode();
        let decision =
            rules::evaluate_tool_call(&self.data_dir, &self.cwd, mode, &policies, tool, args);
        let detail = summarize_call(tool, args);

        let local_target = self.target == "local" || self.target.is_empty();
        let skip_perms = self.skip_permissions() && mode != PermissionMode::Plan;
        let sandbox_ovr = self.sandbox_override();
        let mut extra_rw = policies.extra_rw.clone();
        extra_rw.extend(decision.extra_rw.iter().cloned());

        match decision.effect {
            Effect::Deny => {
                audit::record_decision(
                    &self.data_dir,
                    audit::PermissionAuditEntry {
                        ts_ms: audit::now_ms(),
                        session_id: &self.session_id,
                        target: &self.target,
                        mode: mode.label(),
                        tool,
                        detail,
                        decision: "deny",
                        resolution: "policy_deny",
                        reason: &decision.reason,
                        sandbox: "none",
                    },
                );
                bail!("permission denied: {}", decision.reason);
            }
            Effect::Allow => {
                let sandbox = sandbox_for(
                    local_target,
                    &self.cwd,
                    &extra_rw,
                    skip_perms && decision.needs_net,
                    skip_perms,
                    sandbox_ovr,
                );
                audit::record_decision(
                    &self.data_dir,
                    audit::PermissionAuditEntry {
                        ts_ms: audit::now_ms(),
                        session_id: &self.session_id,
                        target: &self.target,
                        mode: mode.label(),
                        tool,
                        detail,
                        decision: "allow",
                        resolution: "policy_allow",
                        reason: &decision.reason,
                        sandbox: sandbox_label(&sandbox),
                    },
                );
                Ok(ToolAuth { sandbox })
            }
            Effect::Ask if skip_perms => {
                let sandbox = sandbox_for(
                    local_target,
                    &self.cwd,
                    &extra_rw,
                    true,
                    true,
                    sandbox_ovr,
                );
                audit::record_decision(
                    &self.data_dir,
                    audit::PermissionAuditEntry {
                        ts_ms: audit::now_ms(),
                        session_id: &self.session_id,
                        target: &self.target,
                        mode: mode.label(),
                        tool,
                        detail,
                        decision: "ask",
                        resolution: "dangerously_skip_permissions",
                        reason: &decision.reason,
                        sandbox: sandbox_label(&sandbox),
                    },
                );
                Ok(ToolAuth { sandbox })
            }
            Effect::Ask if asks.is_non_interactive() => {
                audit::record_decision(
                    &self.data_dir,
                    audit::PermissionAuditEntry {
                        ts_ms: audit::now_ms(),
                        session_id: &self.session_id,
                        target: &self.target,
                        mode: mode.label(),
                        tool,
                        detail,
                        decision: "ask",
                        resolution: "headless_ask_denied",
                        reason: &decision.reason,
                        sandbox: "none",
                    },
                );
                bail!(
                    "tool `{tool}` requires interactive approval ({}); re-run with --permission-mode auto or --dangerously-skip-permissions",
                    decision.reason
                );
            }
            Effect::Ask => {
                let prompt_res =
                    prompt_user_approval(tool, &detail, &decision, asks, tx).await;
                let choice = match prompt_res {
                    Ok(c) => c,
                    Err(e) => {
                        audit::record_decision(
                            &self.data_dir,
                            audit::PermissionAuditEntry {
                                ts_ms: audit::now_ms(),
                                session_id: &self.session_id,
                                target: &self.target,
                                mode: mode.label(),
                                tool,
                                detail,
                                decision: "ask",
                                resolution: "timeout_or_error",
                                reason: &decision.reason,
                                sandbox: "none",
                            },
                        );
                        return Err(e);
                    }
                };

                let (allow_net, full_host, resolution) = match choice {
                    ApprovalChoice::OnceOffline => (false, false, "user_once_offline"),
                    ApprovalChoice::OnceWithNet => (true, false, "user_once_net"),
                    ApprovalChoice::OnceOutsideSandbox => (true, true, "user_once_unsandboxed"),
                    ApprovalChoice::Session => {
                        if let Some(pat) = &decision.suggested_rule {
                            self.session_allows.lock().unwrap().push(PermissionRule {
                                scope: RuleScope::Session,
                                effect: Effect::Allow,
                                pattern: pat.clone(),
                            });
                        }
                        (decision.needs_net, false, "user_session")
                    }
                    ApprovalChoice::Project => {
                        if let Some(pat) = &decision.suggested_rule {
                            let _ = rules::save_allow_rule(
                                &self.data_dir,
                                &self.cwd,
                                RuleScope::Project,
                                pat,
                            );
                        }
                        (decision.needs_net, false, "user_project")
                    }
                    ApprovalChoice::Deny => {
                        audit::record_decision(
                            &self.data_dir,
                            audit::PermissionAuditEntry {
                                ts_ms: audit::now_ms(),
                                session_id: &self.session_id,
                                target: &self.target,
                                mode: mode.label(),
                                tool,
                                detail,
                                decision: "ask",
                                resolution: "user_denied",
                                reason: &decision.reason,
                                sandbox: "none",
                            },
                        );
                        bail!("user denied `{tool}` ({})", decision.reason);
                    }
                };

                let sandbox = if full_host {
                    SandboxPolicy::DangerFullAccess
                } else {
                    sandbox_for(
                        local_target,
                        &self.cwd,
                        &extra_rw,
                        allow_net,
                        false,
                        sandbox_ovr,
                    )
                };
                audit::record_decision(
                    &self.data_dir,
                    audit::PermissionAuditEntry {
                        ts_ms: audit::now_ms(),
                        session_id: &self.session_id,
                        target: &self.target,
                        mode: mode.label(),
                        tool,
                        detail,
                        decision: "ask",
                        resolution,
                        reason: &decision.reason,
                        sandbox: sandbox_label(&sandbox),
                    },
                );
                Ok(ToolAuth { sandbox })
            }
        }
    }
}

fn sandbox_for(
    local: bool,
    cwd: &Path,
    extra_rw: &[String],
    allow_net: bool,
    skip_perms: bool,
    override_policy: Option<SandboxOverride>,
) -> SandboxPolicy {
    if !local {
        return SandboxPolicy::Off;
    }
    match override_policy {
        Some(SandboxOverride::Off) => SandboxPolicy::Off,
        Some(SandboxOverride::DangerFullAccess) => SandboxPolicy::DangerFullAccess,
        Some(SandboxOverride::ReadOnly) => SandboxPolicy::ReadOnly {
            cwd: cwd.display().to_string(),
            allow_net,
        },
        Some(SandboxOverride::WorkspaceWrite) => SandboxPolicy::WorkspaceWrite {
            cwd: cwd.display().to_string(),
            extra_rw: extra_rw.to_vec(),
            allow_net,
        },
        None if skip_perms => SandboxPolicy::DangerFullAccess,
        None => SandboxPolicy::WorkspaceWrite {
            cwd: cwd.display().to_string(),
            extra_rw: extra_rw.to_vec(),
            allow_net,
        },
    }
}

fn sandbox_label(s: &SandboxPolicy) -> &'static str {
    match s {
        SandboxPolicy::Off => "off",
        SandboxPolicy::ReadOnly { allow_net: false, .. } => "ro:offline",
        SandboxPolicy::ReadOnly { allow_net: true, .. } => "ro:net",
        SandboxPolicy::WorkspaceWrite { allow_net: false, .. } => "ws:offline",
        SandboxPolicy::WorkspaceWrite { allow_net: true, .. } => "ws:net",
        SandboxPolicy::DangerFullAccess => "danger-full-access",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("envoy-perm-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn explicit_deny_beats_allow_and_managed_beats_local() {
        let root = tmpdir("precedence");
        let data = root.join("data");
        let work = root.join("work");
        let managed = root.join("managed.json");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        set_workspace_trusted(&data, &work, true).unwrap();

        // User allows `Bash(rm *)`, project denies `Bash(rm -rf *)`: Deny wins (no more-specific hack).
        rules::save_allow_rule(&data, &work, RuleScope::User, "Bash(rm *)").unwrap();
        std::fs::create_dir_all(work.join(".envoy")).unwrap();
        std::fs::write(
            rules::project_policy_path(&work),
            r#"{"allow":["Bash(git status)"],"deny":["Bash(rm -rf *)"]}"#,
        )
        .unwrap();

        // Managed denies `Bash(git status)` even though project explicitly allows it.
        std::fs::write(&managed, r#"{"deny":["Bash(git status)"]}"#).unwrap();

        let eng = PermissionEngine::new(
            data.clone(),
            work.clone(),
            "s1".into(),
            "local".into(),
            PermissionMode::Auto,
        );
        let eng = Arc::new(
            Arc::try_unwrap(eng)
                .unwrap()
                .with_managed_override(managed),
        );

        let d1 = eng.evaluate("exec", &json!({"command": "rm -rf build"}));
        assert_eq!(d1.effect, Effect::Deny, "{d1:?}");

        let d2 = eng.evaluate("exec", &json!({"command": "git status"}));
        assert_eq!(d2.effect, Effect::Deny, "{d2:?}");

        // Compound command cannot bypass allow via safe first segment.
        rules::save_allow_rule(&data, &work, RuleScope::User, "Bash(ls*)").unwrap();
        let d3 = eng.evaluate("exec", &json!({"command": "ls && rm -rf build"}));
        assert_eq!(d3.effect, Effect::Deny, "{d3:?}");

        // Unparseable shell falls back to Ask.
        let d4 = eng.evaluate("exec", &json!({"command": "echo 'unterminated"}));
        assert_eq!(d4.effect, Effect::Ask, "{d4:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_mode_is_readonly_except_own_plan_files() {
        let root = tmpdir("plan");
        let data = root.join("data");
        let work = root.join("work");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&work).unwrap();

        let eng = PermissionEngine::new(
            data.clone(),
            work.clone(),
            "s1".into(),
            "local".into(),
            PermissionMode::Plan,
        );

        assert_eq!(
            eng.evaluate("read", &json!({"path": "src/lib.rs"})).effect,
            Effect::Allow
        );
        assert_eq!(
            eng.evaluate("write", &json!({"path": "src/lib.rs", "content": "x"}))
                .effect,
            Effect::Deny
        );
        assert_eq!(
            eng.evaluate("exec", &json!({"command": "ls -la"})).effect,
            Effect::Allow
        );
        assert_eq!(
            eng.evaluate("exec", &json!({"command": "touch x"})).effect,
            Effect::Deny
        );
        // Writing to `.envoy/plans/*.md` is allowed in Plan mode, EXCEPT `.envoy` is protected:
        // Wait — check that plan file write in Plan mode succeeds (plan dir exception)!
        let plan_dec = eng.evaluate(
            "write",
            &json!({"path": ".envoy/plans/phase3.md", "content": "# Plan"}),
        );
        assert_eq!(plan_dec.effect, Effect::Allow, "{plan_dec:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn approval_flow_grants_network_once_and_logs_audit() {
        let root = tmpdir("audit");
        let data = root.join("data");
        let work = root.join("work");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&work).unwrap();

        let eng = PermissionEngine::new(
            data.clone(),
            work.clone(),
            "s-net".into(),
            "local".into(),
            PermissionMode::Default,
        );
        let asks = AskBroker::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let asks_bg = asks.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let AgentEvent::Ask { id, .. } = ev {
                    asks_bg.answer(id, "Allow once (with network)".into());
                    break;
                }
            }
        });

        let auth = eng
            .authorize_tool_call(
                "exec",
                &json!({"command": "curl https://example.com"}),
                &asks,
                &tx,
            )
            .await
            .unwrap();
        assert_eq!(
            auth.sandbox,
            SandboxPolicy::WorkspaceWrite {
                cwd: work.display().to_string(),
                extra_rw: vec![],
                allow_net: true,
            }
        );

        let log = std::fs::read_to_string(audit::audit_log_path(&data)).unwrap();
        assert!(log.contains("\"resolution\":\"user_once_net\""), "{log}");
        assert!(log.contains("\"sandbox\":\"ws:net\""), "{log}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn headless_fast_fails_ask_while_skip_permissions_and_sandbox_override_work() {
        let root = tmpdir("headless-perm");
        let data = root.join("data");
        let work = root.join("work");
        let managed = root.join("managed.json");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(&managed, r#"{"deny":["Bash(sudo *)"]}"#).unwrap();

        let eng = PermissionEngine::new(
            data.clone(),
            work.clone(),
            "s-hl".into(),
            "local".into(),
            PermissionMode::Default,
        );
        let eng = Arc::new(
            Arc::try_unwrap(eng)
                .unwrap()
                .with_managed_override(managed),
        );
        let asks = AskBroker::new_headless();
        let (tx, _rx) = mpsc::unbounded_channel();

        // Without skip_permissions, headless Ask fails immediately without 120s wait.
        let err = eng
            .authorize_tool_call("exec", &json!({"command": "curl https://example.com"}), &asks, &tx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("requires interactive approval"), "{err}");

        // With skip_permissions, Ask is bypassed with DangerFullAccess by default,
        // and SandboxOverride can still tighten/change it. Managed Deny stays enforced.
        eng.set_skip_permissions(true);
        let auth = eng
            .authorize_tool_call("exec", &json!({"command": "curl https://example.com"}), &asks, &tx)
            .await
            .unwrap();
        assert_eq!(auth.sandbox, SandboxPolicy::DangerFullAccess);

        eng.set_sandbox_override(Some(SandboxOverride::WorkspaceWrite));
        let auth_ws = eng
            .authorize_tool_call("exec", &json!({"command": "ls"}), &asks, &tx)
            .await
            .unwrap();
        assert!(matches!(auth_ws.sandbox, SandboxPolicy::WorkspaceWrite { .. }));

        let denied = eng
            .authorize_tool_call("exec", &json!({"command": "sudo id"}), &asks, &tx)
            .await
            .unwrap_err();
        assert!(denied.to_string().contains("managed deny"), "{denied}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
