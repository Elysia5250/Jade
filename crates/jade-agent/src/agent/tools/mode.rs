//! Model-driven permission mode switching (`mode` tool).
//!
//! Privilege downgrades take effect immediately with a `say` notice; privilege
//! upgrades require interactive user confirmation via `AskBroker`. Leaving
//! `plan` mode requires a non-empty plan file written under `.jade/plans/*.md`
//! (or `<data_dir>/plans/*.md`) during the active plan cycle, matching sidechat.

use crate::agent::asks::AskBroker;
use crate::agent::events::{AgentEvent, AskOption};
use crate::permissions::paths::canonicalize_with_missing_tail;
use crate::permissions::{PermissionEngine, PermissionMode};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const UPGRADE_ASK_TIMEOUT: Duration = Duration::from_secs(120);
const EXIT_PLAN_ASK_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) fn summarize_mode(args: &Value) -> (String, String) {
    let action = args["action"].as_str().unwrap_or("");
    let title = match action {
        "set" => match args["target"].as_str().filter(|s| !s.is_empty()) {
            Some(target) => format!("mode set {target}"),
            None => "mode set".into(),
        },
        "" => "mode".into(),
        other => format!("mode {other}"),
    };
    let subtitle = args["plan_path"]
        .as_str()
        .or_else(|| args["reason"].as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    (title, subtitle)
}

pub(crate) async fn mode_tool(
    target_machine: &str,
    cwd: &Path,
    args: &Value,
    asks: &Arc<AskBroker>,
    permissions: &Arc<PermissionEngine>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<String> {
    if asks.is_non_interactive() {
        bail!("mode switching is disabled in headless / non-interactive mode");
    }
    let action = args["action"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("missing `action` (use `enter_plan`, `exit_plan`, or `set`)")?;
    let reason = args["reason"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match action {
        "enter_plan" => Ok(enter_plan(permissions, reason, tx)),
        "exit_plan" => exit_plan(target_machine, cwd, args, reason, asks, permissions, tx).await,
        "set" => set_mode(args, reason, asks, permissions, tx).await,
        other => bail!("unknown mode action `{other}` (use `enter_plan`, `exit_plan`, or `set`)"),
    }
}

fn enter_plan(
    permissions: &Arc<PermissionEngine>,
    reason: Option<&str>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> String {
    let current = permissions.mode();
    if current == PermissionMode::Plan {
        return json!({
            "mode": "plan",
            "changed": false,
            "note": "Already in plan mode. Write the plan under `.jade/plans/*.md` before calling `mode` with `action: \"exit_plan\"`."
        })
        .to_string();
    }
    apply_downgrade(
        permissions,
        current,
        PermissionMode::Plan,
        reason.unwrap_or("enter plan mode"),
        tx,
    );
    json!({
        "mode": "plan",
        "previous_mode": current.label(),
        "changed": true,
        "note": "Entered plan mode (read-only workspace; writes allowed only to `.jade/plans/*.md`). Write a non-empty plan file before calling `mode` with `action: \"exit_plan\"`."
    })
    .to_string()
}

async fn exit_plan(
    target_machine: &str,
    cwd: &Path,
    args: &Value,
    reason: Option<&str>,
    asks: &Arc<AskBroker>,
    permissions: &Arc<PermissionEngine>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<String> {
    let current = permissions.mode();
    if current != PermissionMode::Plan {
        bail!(
            "not currently in plan mode (active mode is `{}`)",
            current.label()
        );
    }

    let req_path = args["plan_path"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(plan_file) = permissions.resolve_written_plan_file(req_path) else {
        match req_path {
            Some(p) => bail!(
                "cannot exit plan mode: `{p}` was not written as a non-empty plan file under `.jade/plans/*.md` during this plan session"
            ),
            None => bail!(
                "cannot exit plan mode before writing a non-empty plan file under `.jade/plans/*.md`"
            ),
        }
    };

    let is_local = target_machine.is_empty() || target_machine == "local";
    if is_local {
        let body = std::fs::read_to_string(&plan_file).with_context(|| {
            format!(
                "cannot exit plan mode: plan file `{}` is missing or unreadable",
                plan_file.display()
            )
        })?;
        if body.trim().is_empty() {
            bail!(
                "cannot exit plan mode: plan file `{}` is empty",
                plan_file.display()
            );
        }
    }

    let restore = permissions.plan_exit_target();
    let display_path = display_plan_path(cwd, &plan_file);
    let approve_label = format!("Approve ({})", restore.label());
    let stay_label = "Stay in plan".to_string();
    let question = match reason {
        Some(r) => format!(
            "Plan ready ({display_path}). Exit plan mode and switch to `{}`?\n{r}",
            restore.label()
        ),
        None => format!(
            "Plan ready ({display_path}). Exit plan mode and switch to `{}`?",
            restore.label()
        ),
    };
    let options = vec![
        AskOption {
            label: approve_label.clone(),
            description: Some(format!(
                "Approve the plan and switch to `{}` mode",
                restore.label()
            )),
            recommended: false,
        },
        AskOption {
            label: stay_label,
            description: Some("Keep plan mode and continue revising".into()),
            recommended: true,
        },
    ];

    let audit_reason = match reason {
        Some(r) => format!("plan={display_path}; {r}"),
        None => format!("plan={display_path}"),
    };
    let chosen = prompt_mode_confirmation(asks, tx, question, options, EXIT_PLAN_ASK_TIMEOUT).await;
    if chosen.as_deref() == Some(approve_label.as_str()) {
        permissions.set_mode(restore);
        let _ = tx.send(AgentEvent::PermissionModeChanged { mode: restore });
        permissions.record_mode_audit(
            PermissionMode::Plan,
            restore,
            "allow",
            "model_exit_plan_approved",
            &audit_reason,
            tx,
        );
        Ok(json!({
            "approved": true,
            "mode": restore.label(),
            "plan_path": display_path,
            "changed": true,
        })
        .to_string())
    } else {
        permissions.record_mode_audit(
            PermissionMode::Plan,
            restore,
            "deny",
            "model_exit_plan_denied",
            &audit_reason,
            tx,
        );
        bail!(
            "user chose to stay in plan mode; refine `{display_path}` before requesting exit again"
        );
    }
}

async fn set_mode(
    args: &Value,
    reason: Option<&str>,
    asks: &Arc<AskBroker>,
    permissions: &Arc<PermissionEngine>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<String> {
    let raw_target = args["target"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("missing `target` for `mode` action `set`")?;
    let target = PermissionMode::parse(raw_target).with_context(|| {
        format!("unknown permission mode `{raw_target}` (use `plan`, `default`, `accept-edits`, or `auto`)")
    })?;
    let current = permissions.mode();

    if target == PermissionMode::Turbo {
        permissions.record_mode_audit(
            current,
            target,
            "deny",
            "model_set_forbidden",
            "turbo mode is user-only",
            tx,
        );
        bail!("model cannot switch to `turbo` mode; only the user may select `turbo`");
    }

    if current == PermissionMode::Plan && target != PermissionMode::Plan {
        bail!(
            "cannot leave `plan` mode via `set`; write a plan file under `.jade/plans/*.md` and call `mode` with `action: \"exit_plan\"`"
        );
    }

    if target == current {
        return Ok(json!({
            "mode": current.label(),
            "changed": false,
        })
        .to_string());
    }

    if target == PermissionMode::Plan {
        return Ok(enter_plan(permissions, reason, tx));
    }

    if target.privilege_rank() < current.privilege_rank() {
        apply_downgrade(
            permissions,
            current,
            target,
            reason.unwrap_or("mode downgrade"),
            tx,
        );
        return Ok(json!({
            "mode": target.label(),
            "previous_mode": current.label(),
            "changed": true,
        })
        .to_string());
    }

    let approve_label = format!("Switch to {}", target.label());
    let keep_label = format!("Keep {}", current.label());
    let question = match reason {
        Some(r) => format!(
            "Switch permission mode from `{}` to `{}`?\n{r}",
            current.label(),
            target.label()
        ),
        None => format!(
            "Switch permission mode from `{}` to `{}`?",
            current.label(),
            target.label()
        ),
    };
    let options = vec![
        AskOption {
            label: approve_label.clone(),
            description: Some(format!("Continue in `{}` mode", target.label())),
            recommended: false,
        },
        AskOption {
            label: keep_label,
            description: Some("Stay in the current permission mode".into()),
            recommended: true,
        },
    ];
    let audit_reason = reason.unwrap_or("mode upgrade");
    let chosen = prompt_mode_confirmation(asks, tx, question, options, UPGRADE_ASK_TIMEOUT).await;
    if chosen.as_deref() == Some(approve_label.as_str()) {
        permissions.set_mode(target);
        let _ = tx.send(AgentEvent::PermissionModeChanged { mode: target });
        permissions.record_mode_audit(
            current,
            target,
            "allow",
            "model_upgrade_approved",
            audit_reason,
            tx,
        );
        Ok(json!({
            "approved": true,
            "mode": target.label(),
            "previous_mode": current.label(),
            "changed": true,
        })
        .to_string())
    } else {
        permissions.record_mode_audit(
            current,
            target,
            "deny",
            "model_upgrade_denied",
            audit_reason,
            tx,
        );
        bail!(
            "user declined switching permission mode from `{}` to `{}`",
            current.label(),
            target.label()
        );
    }
}

fn apply_downgrade(
    permissions: &Arc<PermissionEngine>,
    from: PermissionMode,
    to: PermissionMode,
    reason: &str,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    permissions.set_mode(to);
    let _ = tx.send(AgentEvent::PermissionModeChanged { mode: to });
    let say_text = if reason.is_empty() || reason == "enter plan mode" || reason == "mode downgrade"
    {
        format!("mode: {} -> {}", from.label(), to.label())
    } else {
        format!("mode: {} -> {} ({reason})", from.label(), to.label())
    };
    let _ = tx.send(AgentEvent::Say {
        text: say_text,
        kind: "note".into(),
    });
    permissions.record_mode_audit(from, to, "allow", "model_downgrade", reason, tx);
}

async fn prompt_mode_confirmation(
    asks: &Arc<AskBroker>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    question: String,
    options: Vec<AskOption>,
    timeout: Duration,
) -> Option<String> {
    let _gate = asks.acquire_prompt().await;
    let id = asks.next_id();
    let (send, mut recv) = oneshot::channel();
    asks.register(id, send);
    let _ = tx.send(AgentEvent::Ask {
        id,
        question,
        options,
        recommended: 1,
        timeout_ms: timeout.as_millis() as u64,
    });
    let answer = asks.wait_for_answer(id, timeout, &mut recv).await;
    asks.forget(id);
    if answer.is_none() {
        let _ = tx.send(AgentEvent::AskClosed { id });
    }
    answer
}

fn display_plan_path(cwd: &Path, plan_file: &Path) -> String {
    let cwd_canon = canonicalize_with_missing_tail(cwd);
    plan_file
        .strip_prefix(&cwd_canon)
        .unwrap_or(plan_file)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "jade-mode-tool-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn drain_events(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn headless_forbids_all_mode_switches() {
        let dir = tmpdir("headless");
        let perms = PermissionEngine::new(
            dir.join("data"),
            dir.clone(),
            "s-hl".into(),
            "local".into(),
            PermissionMode::Auto,
        );
        let asks = AskBroker::new_headless();
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = mode_tool(
            "local",
            &dir,
            &json!({"action": "enter_plan", "reason": "make a plan"}),
            &asks,
            &perms,
            &tx,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("headless"), "{err}");
        assert_eq!(perms.mode(), PermissionMode::Auto);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn downgrade_enters_plan_via_say_without_ask() {
        let dir = tmpdir("enter");
        let perms = PermissionEngine::new(
            dir.join("data"),
            dir.clone(),
            "s-enter".into(),
            "local".into(),
            PermissionMode::Auto,
        );
        let asks = AskBroker::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = mode_tool(
            "local",
            &dir,
            &json!({"action": "enter_plan", "reason": "user asked to make a plan"}),
            &asks,
            &perms,
            &tx,
        )
        .await
        .unwrap();
        assert_eq!(perms.mode(), PermissionMode::Plan);
        assert_eq!(perms.plan_exit_target(), PermissionMode::Auto);
        assert!(out.contains("\"mode\":\"plan\""), "{out}");

        let events = drain_events(&mut rx);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionModeChanged { mode: PermissionMode::Plan })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Say { text, .. } if text.contains("auto -> plan"))));
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Ask { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exit_plan_requires_written_plan_file_and_restores_previous_mode_via_ask() {
        let dir = tmpdir("exit");
        let perms = PermissionEngine::new(
            dir.join("data"),
            dir.clone(),
            "s-exit".into(),
            "local".into(),
            PermissionMode::Turbo,
        );
        let asks = AskBroker::new();
        let (tx, mut rx) = mpsc::unbounded_channel();

        // Enter plan from Turbo -> downgrade via say.
        mode_tool("local", &dir, &json!({"action": "enter_plan"}), &asks, &perms, &tx)
            .await
            .unwrap();
        let _ = drain_events(&mut rx);

        // Cannot bypass exit_plan via `set`.
        let set_err = mode_tool(
            "local",
            &dir,
            &json!({"action": "set", "target": "auto"}),
            &asks,
            &perms,
            &tx,
        )
        .await
        .unwrap_err();
        assert!(set_err.to_string().contains("exit_plan"), "{set_err}");

        // Cannot exit plan before writing a plan file.
        let no_file_err = mode_tool("local", &dir, &json!({"action": "exit_plan"}), &asks, &perms, &tx)
            .await
            .unwrap_err();
        assert!(
            no_file_err.to_string().contains(".jade/plans/*.md"),
            "{no_file_err}"
        );

        // Write a non-empty plan file and record it.
        let plan_dir = dir.join(".jade/plans");
        std::fs::create_dir_all(&plan_dir).unwrap();
        let plan_file = plan_dir.join("refactor.md");
        std::fs::write(&plan_file, "# Plan\n1. Do the thing\n").unwrap();
        perms.record_plan_file_write(".jade/plans/refactor.md", true);

        // First ask: user stays in plan -> remains in Plan mode.
        let answerer = asks.clone();
        let mut rx_stay = rx;
        let stay_task = tokio::spawn(async move {
            while let Some(ev) = rx_stay.recv().await {
                if let AgentEvent::Ask {
                    id,
                    options,
                    recommended,
                    ..
                } = ev
                {
                    assert_eq!(recommended, 1);
                    assert_eq!(options[1].label, "Stay in plan");
                    answerer.answer(id, options[1].label.clone());
                    break;
                }
            }
            rx_stay
        });
        let stay_err = mode_tool("local", &dir, &json!({"action": "exit_plan"}), &asks, &perms, &tx)
            .await
            .unwrap_err();
        assert!(stay_err.to_string().contains("stay in plan"), "{stay_err}");
        assert_eq!(perms.mode(), PermissionMode::Plan);
        let mut rx_approve = stay_task.await.unwrap();

        // Second ask: user approves -> restores Turbo (the exact pre-plan mode!).
        let answerer = asks.clone();
        let approve_task = tokio::spawn(async move {
            while let Some(ev) = rx_approve.recv().await {
                if let AgentEvent::Ask { id, options, .. } = ev {
                    assert_eq!(options[0].label, "Approve (turbo)");
                    answerer.answer(id, options[0].label.clone());
                    break;
                }
            }
            rx_approve
        });
        let ok = mode_tool("local", &dir, &json!({"action": "exit_plan"}), &asks, &perms, &tx)
            .await
            .unwrap();
        assert!(ok.contains("\"mode\":\"turbo\""), "{ok}");
        assert_eq!(perms.mode(), PermissionMode::Turbo);
        let _ = approve_task.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn starting_in_plan_defaults_exit_to_accept_edits() {
        let dir = tmpdir("start-plan");
        let perms = PermissionEngine::new(
            dir.join("data"),
            dir.clone(),
            "s-sp".into(),
            "local".into(),
            PermissionMode::Plan,
        );
        assert_eq!(perms.plan_exit_target(), PermissionMode::AcceptEdits);

        let plan_dir = dir.join(".jade/plans");
        std::fs::create_dir_all(&plan_dir).unwrap();
        std::fs::write(plan_dir.join("p.md"), "# Steps\n- one\n").unwrap();
        perms.record_plan_file_write(".jade/plans/p.md", true);

        let asks = AskBroker::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let answerer = asks.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let AgentEvent::Ask { id, options, .. } = ev {
                    assert_eq!(options[0].label, "Approve (accept-edits)");
                    answerer.answer(id, options[0].label.clone());
                    break;
                }
            }
        });
        let out = mode_tool("local", &dir, &json!({"action": "exit_plan"}), &asks, &perms, &tx)
            .await
            .unwrap();
        assert!(out.contains("\"mode\":\"accept-edits\""), "{out}");
        assert_eq!(perms.mode(), PermissionMode::AcceptEdits);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn general_set_forbids_turbo_downgrades_via_say_and_upgrades_via_ask() {
        let dir = tmpdir("general");
        let perms = PermissionEngine::new(
            dir.join("data"),
            dir.clone(),
            "s-gen".into(),
            "local".into(),
            PermissionMode::Auto,
        );
        let asks = AskBroker::new();
        let (tx, mut rx) = mpsc::unbounded_channel();

        // Model can never request turbo.
        let turbo_err = mode_tool(
            "local",
            &dir,
            &json!({"action": "set", "target": "turbo"}),
            &asks,
            &perms,
            &tx,
        )
        .await
        .unwrap_err();
        assert!(turbo_err.to_string().contains("turbo"), "{turbo_err}");

        // Downgrade Auto -> Default runs immediately via say.
        mode_tool(
            "local",
            &dir,
            &json!({"action": "set", "target": "default", "reason": "ask before edits"}),
            &asks,
            &perms,
            &tx,
        )
        .await
        .unwrap();
        assert_eq!(perms.mode(), PermissionMode::Default);
        let events = drain_events(&mut rx);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Say { text, .. } if text.contains("auto -> default"))));
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Ask { .. })));

        // Upgrade Default -> AcceptEdits requires ask approval.
        let answerer = asks.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let AgentEvent::Ask {
                    id,
                    options,
                    recommended,
                    ..
                } = ev
                {
                    assert_eq!(recommended, 1);
                    assert_eq!(options[0].label, "Switch to accept-edits");
                    assert_eq!(options[1].label, "Keep default");
                    answerer.answer(id, options[0].label.clone());
                    break;
                }
            }
        });
        let out = mode_tool(
            "local",
            &dir,
            &json!({"action": "set", "target": "accept-edits", "reason": "user approved edits"}),
            &asks,
            &perms,
            &tx,
        )
        .await
        .unwrap();
        assert!(out.contains("\"approved\":true"), "{out}");
        assert_eq!(perms.mode(), PermissionMode::AcceptEdits);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deny_rule_disables_mode_tool() {
        let dir = tmpdir("deny");
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("permissions.json"), r#"{"deny":["mode"]}"#).unwrap();
        let perms = PermissionEngine::new(
            data,
            dir.clone(),
            "s-deny".into(),
            "local".into(),
            PermissionMode::Auto,
        );
        assert!(!perms.is_tool_enabled("mode"));
        let decision = perms.evaluate("mode", &json!({"action": "enter_plan"}));
        assert_eq!(decision.effect, crate::permissions::Effect::Deny);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
