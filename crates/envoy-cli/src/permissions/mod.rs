//! Permission engine: modes, rules, path/shell guards, approvals, and audit.
//!
//! No `ratatui` or provider types live here. The rule engine decides `allow`,
//! `ask`, or `deny` and selects the [`SandboxPolicy`]; the OS sandbox enforces
//! the real boundary.

pub mod audit;
pub mod paths;
pub mod rules;
pub mod shell;
pub mod types;

use crate::agent::{AgentEvent, AskBroker, AskOption};
use anyhow::{bail, Result};
use envoy_proto::SandboxPolicy;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub use rules::{is_workspace_trusted, set_workspace_trusted};
pub use types::{
    Decision, Effect, PermissionMode, PermissionRule, PolicyFile, RuleScope, ToolAuth,
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
                let sandbox = sandbox_for(local_target, &self.cwd, &extra_rw, false);
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
                    sandbox_for(local_target, &self.cwd, &extra_rw, allow_net)
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

fn sandbox_for(local: bool, cwd: &Path, extra_rw: &[String], allow_net: bool) -> SandboxPolicy {
    if !local {
        return SandboxPolicy::Off;
    }
    SandboxPolicy::WorkspaceWrite {
        cwd: cwd.display().to_string(),
        extra_rw: extra_rw.to_vec(),
        allow_net,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalChoice {
    OnceOffline,
    OnceWithNet,
    OnceOutsideSandbox,
    Session,
    Project,
    Deny,
}

async fn prompt_user_approval(
    tool: &str,
    detail: &str,
    decision: &Decision,
    asks: &Arc<AskBroker>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<ApprovalChoice> {
    let mut options = Vec::new();
    if tool == "exec" && decision.needs_net {
        options.push(AskOption {
            label: "Allow once (with network)".into(),
            description: Some("Run in workspace sandbox with network enabled for this command".into()),
            recommended: false,
        });
        options.push(AskOption {
            label: "Allow once (offline)".into(),
            description: Some("Run inside default network-disabled sandbox".into()),
            recommended: false,
        });
    } else if tool == "exec" && decision.outside_workspace {
        options.push(AskOption {
            label: "Allow once (with expanded path)".into(),
            description: Some("Run inside sandbox with the referenced path bound".into()),
            recommended: false,
        });
        options.push(AskOption {
            label: "Allow once (unsandboxed)".into(),
            description: Some("Run without OS sandbox restrictions for this command only".into()),
            recommended: false,
        });
    } else {
        options.push(AskOption {
            label: "Allow once".into(),
            description: Some("Approve this single tool invocation".into()),
            recommended: false,
        });
    }
    if let Some(rule) = &decision.suggested_rule {
        options.push(AskOption {
            label: "Allow for session".into(),
            description: Some(format!("Session allow: {rule}")),
            recommended: false,
        });
        options.push(AskOption {
            label: "Always allow (project)".into(),
            description: Some(format!("Save to .envoy/permissions.json: {rule}")),
            recommended: false,
        });
    }
    let deny_idx = options.len();
    options.push(AskOption {
        label: "Deny".into(),
        description: Some("Refuse this operation".into()),
        recommended: true,
    });

    let question = format!(
        "Permission required for `{tool}` ({}):\n  {detail}",
        decision.reason
    );
    let timeout_s = 120u64;
    let id = asks.next_id();
    let (send, recv) = oneshot::channel();
    asks.register(id, send);
    let _ = tx.send(AgentEvent::Ask {
        id,
        question,
        options: options.clone(),
        recommended: deny_idx,
        timeout_ms: timeout_s * 1000,
    });

    let answer = tokio::time::timeout(Duration::from_secs(timeout_s), recv).await;
    asks.forget(id);
    let chosen = match answer {
        Ok(Ok(label)) => label,
        _ => {
            let _ = tx.send(AgentEvent::AskClosed { id });
            return Ok(ApprovalChoice::Deny);
        }
    };

    Ok(match chosen.as_str() {
        "Allow once (with network)" | "Allow once (with expanded path)" => {
            ApprovalChoice::OnceWithNet
        }
        "Allow once (unsandboxed)" => ApprovalChoice::OnceOutsideSandbox,
        "Allow once" | "Allow once (offline)" => ApprovalChoice::OnceOffline,
        "Allow for session" => ApprovalChoice::Session,
        "Always allow (project)" => ApprovalChoice::Project,
        _ => ApprovalChoice::Deny,
    })
}

fn summarize_call(tool: &str, args: &Value) -> String {
    match tool {
        "exec" => args["command"].as_str().unwrap_or("").to_string(),
        "read" | "write" | "edit" | "ls" => args["path"].as_str().unwrap_or(".").to_string(),
        "glob" | "grep" => format!(
            "{} ({})",
            args["pattern"].as_str().unwrap_or("*"),
            args["root"].as_str().unwrap_or(".")
        ),
        "webfetch" => args["url"].as_str().unwrap_or("").to_string(),
        other => other.to_string(),
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
}
