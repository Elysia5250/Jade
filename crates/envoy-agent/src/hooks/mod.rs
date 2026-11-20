//! Deterministic Lifecycle Hooks engine (`HookContext -> HookContext`).
//!
//! Rules execute strictly in `(Managed -> User -> Project, priority asc, decl_index asc)`
//! order, passing every state mutation explicitly through `HookContext`.

pub mod config;
pub mod exec;
#[cfg(test)]
mod tests_security;
#[cfg(test)]
mod tests_stress;
pub mod types;

use crate::agent::{AgentEvent, AskBroker, AskOption};
use crate::permissions::audit::{now_ms, redact_secrets};
use crate::permissions::is_workspace_trusted;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub use types::{
    HookContext, HookDecision, HookEvent, HookRule, HookScope, HookStepStatus, HookStepTrace,
    StageCapability, MAX_STOP_HOOK_REENTRIES, SHUTDOWN_HOOK_TIMEOUT_MS,
};

#[derive(Debug, Clone)]
pub struct HookEngine {
    data_dir: PathBuf,
    cwd: PathBuf,
    managed_override: Option<PathBuf>,
}

#[derive(Serialize)]
struct HookAuditEntry<'a> {
    ts_ms: u64,
    session_id: &'a str,
    turn_seq: u64,
    step: usize,
    depth: u8,
    event: &'a str,
    decision: &'a str,
    reason: String,
    hooks_ran: usize,
    duration_ms: u64,
    trace: &'a [HookStepTrace],
}

impl HookEngine {
    pub fn new(data_dir: PathBuf, cwd: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            data_dir,
            cwd,
            managed_override: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_managed_override(mut self, path: PathBuf) -> Self {
        self.managed_override = Some(path);
        self
    }

    pub fn load_rules(&self) -> Vec<HookRule> {
        self.load_rules_checked().rules
    }

    fn load_rules_checked(&self) -> config::LoadedHooks {
        let trusted = is_workspace_trusted(&self.data_dir, &self.cwd);
        config::load_hooks_checked(
            &self.data_dir,
            &self.cwd,
            trusted,
            self.managed_override.as_deref(),
        )
    }

    /// Execute matching hooks strictly sequentially, passing all state explicitly through `HookContext`.
    pub async fn dispatch(&self, ctx: HookContext) -> HookContext {
        self.dispatch_bounded(ctx, None).await
    }

    /// Like [`HookEngine::dispatch`], with an optional overall wall-clock budget (used at shutdown).
    pub async fn dispatch_bounded(
        &self,
        mut ctx: HookContext,
        total_budget_ms: Option<u64>,
    ) -> HookContext {
        let loaded = self.load_rules_checked();
        if let Some(err) = loaded.managed_error {
            let reason = format!("managed hooks load failed (fail-closed): {err}");
            if ctx.can_block {
                ctx.decision = HookDecision::Block {
                    reason: reason.clone(),
                    by: "managed:load_error".into(),
                    scope: HookScope::Managed,
                };
            }
            ctx.trace.push(HookStepTrace {
                id: "managed:load_error".into(),
                scope: HookScope::Managed,
                priority: i32::MIN,
                decl_index: 0,
                status: HookStepStatus::Errored,
                exit_code: None,
                duration_ms: 0,
                reason,
                mutated_keys: Vec::new(),
            });
            record_hook_audit(&self.data_dir, &ctx, 0);
            return ctx;
        }
        let matching: Vec<HookRule> = loaded
            .rules
            .into_iter()
            .filter(|r| config::matches_rule(r, &ctx))
            .collect();
        if matching.is_empty() {
            return ctx;
        }

        let start = Instant::now();
        let mut halted = false;

        for rule in &matching {
            if halted && !rule.always_run {
                ctx.trace.push(HookStepTrace {
                    id: rule.id.clone(),
                    scope: rule.scope,
                    priority: rule.priority,
                    decl_index: rule.decl_index,
                    status: HookStepStatus::Skipped,
                    exit_code: None,
                    duration_ms: 0,
                    reason: "short-circuited by earlier blocking decision".into(),
                    mutated_keys: Vec::new(),
                });
                continue;
            }

            let remaining_ms = total_budget_ms.map(|budget| {
                let used = start.elapsed().as_millis() as u64;
                budget.saturating_sub(used)
            });
            if matches!(remaining_ms, Some(0)) {
                ctx.trace.push(HookStepTrace {
                    id: rule.id.clone(),
                    scope: rule.scope,
                    priority: rule.priority,
                    decl_index: rule.decl_index,
                    status: HookStepStatus::Skipped,
                    exit_code: None,
                    duration_ms: 0,
                    reason: "skipped: total hook budget exhausted".into(),
                    mutated_keys: Vec::new(),
                });
                continue;
            }

            let (outcome, dur) = exec::run_hook_process(rule, &ctx, remaining_ms).await;
            exec::apply_step_result(&mut ctx, rule, outcome, dur, halted);
            if ctx.decision.is_blocking() {
                halted = true;
            }
        }

        let total_ms = start.elapsed().as_millis() as u64;
        record_hook_audit(&self.data_dir, &ctx, total_ms);
        ctx
    }
}

fn record_hook_audit(data_dir: &Path, ctx: &HookContext, duration_ms: u64) {
    let path = data_dir.join("agent").join("hook_audit.jsonl");
    envoy_core::audit::rotate_if_oversized(&path, envoy_core::audit::LOG_ROTATE_BYTES);
    if let Some(parent) = path.parent() {
        let _ = envoy_core::fs::ensure_private_dir(parent);
    }
    let trace: Vec<HookStepTrace> = ctx
        .trace
        .iter()
        .cloned()
        .map(|mut step| {
            step.reason = redact_secrets(&step.reason);
            step
        })
        .collect();
    let entry = HookAuditEntry {
        ts_ms: now_ms(),
        session_id: &ctx.session_id,
        turn_seq: ctx.turn_seq,
        step: ctx.step,
        depth: ctx.depth,
        event: ctx.event.label(),
        decision: ctx.decision.label(),
        reason: redact_secrets(ctx.decision.reason().unwrap_or("")),
        hooks_ran: trace
            .iter()
            .filter(|t| t.status != HookStepStatus::Skipped)
            .count(),
        duration_ms,
        trace: &trace,
    };
    if let Ok(line) = serde_json::to_string(&entry) {
        let mut buf = line.into_bytes();
        buf.push(b'\n');
        if let Ok(mut f) = envoy_core::fs::open_append_private(&path) {
            let _ = f.write_all(&buf);
        }
    }
}

/// Emit `AgentEvent::HookRan` summarizing executed hooks (noop when no hook matched).
pub fn emit_hook_event(tx: &mpsc::UnboundedSender<AgentEvent>, ctx: &HookContext) {
    if ctx.trace.is_empty() {
        return;
    }
    let count = ctx
        .trace
        .iter()
        .filter(|t| t.status != HookStepStatus::Skipped)
        .count();
    let duration_ms: u64 = ctx.trace.iter().map(|t| t.duration_ms).sum();
    let _ = tx.send(AgentEvent::HookRan {
        event: ctx.event.label().to_string(),
        count,
        blocked: ctx.decision.is_blocking(),
        reason: ctx.decision.reason().unwrap_or("").to_string(),
        duration_ms,
    });
}

/// Prompt the user interactively when a hook yields `HookDecision::Ask`.
pub async fn confirm_hook_ask(
    asks: &Arc<AskBroker>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    stage: HookEvent,
    reason: &str,
    by: &str,
) -> std::result::Result<(), String> {
    if asks.is_non_interactive() {
        return Err(format!(
            "hook `{by}` ({}) requires interactive confirmation in headless mode: {reason}",
            stage.label()
        ));
    }
    let _gate = asks.acquire_prompt().await;
    let id = asks.next_id();
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    asks.register(id, resp_tx);
    let options = vec![
        AskOption {
            label: "Allow once".into(),
            description: Some("Proceed despite hook check".into()),
            recommended: false,
        },
        AskOption {
            label: "Deny".into(),
            description: Some("Block this action".into()),
            recommended: true,
        },
    ];
    let _ = tx.send(AgentEvent::Ask {
        id,
        question: format!(
            "Hook `{by}` ({}) requests confirmation: {reason}",
            stage.label()
        ),
        options,
        recommended: 1,
        timeout_ms: 120_000,
    });
    let res = match tokio::time::timeout(Duration::from_secs(120), resp_rx).await {
        Ok(Ok(label)) if label == "Allow once" => Ok(()),
        Ok(Ok(label)) => Err(format!(
            "denied by user ({label}) after hook `{by}` ask: {reason}"
        )),
        Ok(Err(_)) | Err(_) => {
            asks.forget(id);
            Err(format!(
                "timed out waiting for hook `{by}` confirmation: {reason}"
            ))
        }
    };
    let _ = tx.send(AgentEvent::AskClosed { id });
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::set_workspace_trusted;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "envoy-hooks-mod-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn sequential_pipeline_passes_all_state_explicitly_via_hook_context() {
        let dir = tmpdir("ctx");
        let data = dir.join("data");
        let cwd = dir.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(cwd.join(".envoy")).unwrap();
        set_workspace_trusted(&data, &cwd, true).unwrap();

        // Hook 0 (Managed): sets state.seq = ["m"] and rewrites tool_input.flag = 1
        let managed = dir.join("managed.json");
        std::fs::write(
            &managed,
            r#"{
              "hooks": {
                "PreToolUse": [
                  {
                    "id": "m0",
                    "matcher": "exec",
                    "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); print(json.dumps({\"statePatch\":{\"trace_seq\":[\"m\"],\"seen_in_m\":c[\"tool_input\"][\"cmd\"]},\"updatedInput\":{\"cmd\":c[\"tool_input\"][\"cmd\"],\"flag\":1}}))'"
                  }
                ]
              }
            }"#,
        )
        .unwrap();

        // Hook 1 (User): reads state.trace_seq written by Managed and updatedInput.flag, appends "u" + additionalContext
        std::fs::write(
            config::user_hooks_path(&data),
            r#"{
              "hooks": {
                "PreToolUse": [
                  {
                    "id": "u0",
                    "matcher": "exec",
                    "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); seq=c[\"state\"][\"trace_seq\"]+[\"u\"]; print(json.dumps({\"statePatch\":{\"trace_seq\":seq,\"flag_seen\":c[\"tool_input\"][\"flag\"]},\"additionalContext\":\"from-user\"}))'"
                  }
                ]
              }
            }"#,
        )
        .unwrap();

        // Hook 2 (Project): asks user, sees ["m","u"]
        std::fs::write(
            config::project_hooks_path(&cwd),
            r#"{
              "hooks": {
                "PreToolUse": [
                  {
                    "id": "p0",
                    "matcher": "exec",
                    "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); seq=c[\"state\"][\"trace_seq\"]+[\"p\"]; print(json.dumps({\"decision\":\"ask\",\"reason\":\"confirm\",\"statePatch\":{\"trace_seq\":seq}}))'"
                  }
                ]
              }
            }"#,
        )
        .unwrap();

        let engine = (*HookEngine::new(data.clone(), cwd.clone()))
            .clone()
            .with_managed_override(managed);

        let mut ctx = HookContext::new(
            "s1",
            cwd.clone(),
            "local",
            "default",
            HookEvent::PreToolUse,
            BTreeMap::new(),
        );
        ctx.tool_name = Some("exec".into());
        ctx.tool_input = Some(json!({"cmd": "ls"}));

        let out = engine.dispatch(ctx).await;
        assert_eq!(out.state.get("trace_seq"), Some(&json!(["m", "u", "p"])));
        assert_eq!(out.state.get("flag_seen"), Some(&json!(1)));
        assert_eq!(out.tool_input, Some(json!({"cmd": "ls", "flag": 1})));
        assert_eq!(out.additional_context, vec!["from-user".to_string()]);
        assert!(matches!(
            out.decision,
            HookDecision::Ask { ref by, scope: HookScope::Project, .. } if by == "p0"
        ));
        assert_eq!(out.trace.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn conflict_resolution_ask_cannot_be_downgraded_and_block_short_circuits() {
        let dir = tmpdir("conflict");
        let data = dir.join("data");
        let cwd = dir.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(cwd.join(".envoy")).unwrap();
        set_workspace_trusted(&data, &cwd, true).unwrap();

        // User asks; Project returns allow (cannot downgrade Ask!), then User2 blocks, then Project2 is skipped, ProjectAlways runs.
        std::fs::write(
            config::user_hooks_path(&data),
            r#"{
              "hooks": {
                "PreToolUse": [
                  { "id": "u-ask", "priority": 1, "command": "echo '{\"decision\":\"ask\",\"reason\":\"u-reason\"}'" },
                  { "id": "u-allow", "priority": 2, "command": "echo '{\"decision\":\"allow\"}'" },
                  { "id": "u-block", "priority": 3, "command": "echo 'stop now' >&2; exit 2" },
                  { "id": "u-after", "priority": 4, "command": "echo '{\"statePatch\":{\"ran_after\":true}}'" },
                  { "id": "u-audit", "priority": 5, "always_run": true, "command": "echo '{\"decision\":\"allow\",\"statePatch\":{\"audited\":true}}'" }
                ]
              }
            }"#,
        )
        .unwrap();

        let engine = HookEngine::new(data.clone(), cwd.clone());
        let mut ctx = HookContext::new(
            "s1",
            cwd.clone(),
            "local",
            "default",
            HookEvent::PreToolUse,
            BTreeMap::new(),
        );
        ctx.tool_name = Some("exec".into());
        let out = engine.dispatch(ctx).await;

        assert!(matches!(
            out.decision,
            HookDecision::Block { ref by, ref reason, .. } if by == "u-block" && reason == "stop now"
        ));
        assert_eq!(out.trace.len(), 5);
        assert_eq!(out.trace[0].status, HookStepStatus::Asked);
        assert_eq!(out.trace[1].status, HookStepStatus::Succeeded);
        assert_eq!(out.trace[2].status, HookStepStatus::Blocked);
        assert_eq!(out.trace[3].status, HookStepStatus::Skipped);
        // always_run executed post-halt without overwriting Block
        assert_eq!(out.trace[4].status, HookStepStatus::Succeeded);
        assert!(!out.state.contains_key("ran_after"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn timeout_fail_closed_blocks_and_non_blocking_stage_ignores_block() {
        let dir = tmpdir("timeout");
        let data = dir.join("data");
        let cwd = dir.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        std::fs::write(
            config::user_hooks_path(&data),
            r#"{
              "hooks": {
                "PreModelCall": [
                  { "id": "slow-closed", "timeout_ms": 100, "fail_closed": true, "command": "sleep 2" }
                ],
                "PostToolUse": [
                  { "id": "post-try-block", "command": "echo '{\"decision\":\"block\",\"reason\":\"nope\",\"statePatch\":{\"linted\":true},\"additionalContext\":\"lint ok\"}'" }
                ]
              }
            }"#,
        )
        .unwrap();

        let engine = HookEngine::new(data.clone(), cwd.clone());

        let mut ctx1 = HookContext::new(
            "s1",
            cwd.clone(),
            "local",
            "default",
            HookEvent::PreModelCall,
            BTreeMap::new(),
        );
        ctx1.model_id = Some("claude".into());
        let out1 = engine.dispatch(ctx1).await;
        assert!(out1.decision.is_blocking());
        assert_eq!(out1.trace[0].status, HookStepStatus::TimedOut);

        let mut ctx2 = HookContext::new(
            "s1",
            cwd.clone(),
            "local",
            "default",
            HookEvent::PostToolUse,
            BTreeMap::new(),
        );
        ctx2.tool_name = Some("write".into());
        let out2 = engine.dispatch(ctx2).await;
        assert_eq!(out2.decision, HookDecision::Continue);
        assert_eq!(out2.state.get("linted"), Some(&json!(true)));
        assert_eq!(out2.additional_context, vec!["lint ok".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_hook_audit_redacts_trace_reasons() {
        let dir = tmpdir("audit-redact");
        let mut ctx = HookContext::new(
            "s1",
            dir.clone(),
            "local",
            "default",
            HookEvent::PreToolUse,
            BTreeMap::new(),
        );
        ctx.decision = HookDecision::Block {
            reason: "blocked with Bearer supersecret123".into(),
            by: "h1".into(),
            scope: HookScope::User,
        };
        ctx.trace.push(HookStepTrace {
            id: "h1".into(),
            scope: HookScope::User,
            priority: 1,
            decl_index: 0,
            status: HookStepStatus::Blocked,
            exit_code: Some(0),
            duration_ms: 5,
            reason: "API_TOKEN=tok999 and sk-secretval".into(),
            mutated_keys: Vec::new(),
        });
        record_hook_audit(&dir, &ctx, 5);
        let raw = std::fs::read_to_string(dir.join("agent/hook_audit.jsonl")).unwrap();
        assert!(!raw.contains("supersecret123"), "{raw}");
        assert!(!raw.contains("tok999"), "{raw}");
        assert!(!raw.contains("sk-secretval"), "{raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
