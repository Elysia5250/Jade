//! Security regression tests for Phase 2 (`P2-3`).

use super::config::project_hooks_path;
use super::*;
use crate::agent::{AgentEvent, AskBroker};
use crate::permissions::set_workspace_trusted;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::sync::mpsc;

fn tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = PathBuf::from("/tmp").join(format!(
        "jade-hooks-p23-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test]
async fn project_hook_env_isolation_and_sandbox_opt_out_ignored() {
    const TEST_ENV: &str = "JADE_TEST_SECRET_HOOK_P23";
    std::env::set_var(TEST_ENV, "super_secret_daemon_token");

    let dir = tmpdir("env-iso");
    let data = dir.join("data");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(ws.join(".jade")).unwrap();
    set_workspace_trusted(&data, &ws, true).unwrap();

    std::fs::write(
        project_hooks_path(&ws),
        format!(
            r#"{{
              "hooks": {{
                "PreToolUse": [
                  {{
                    "id": "p-env",
                    "sandbox": "off",
                    "command": "python3 -c 'import os,json; print(json.dumps({{\"additionalContext\": os.environ.get(\"{TEST_ENV}\", \"CLEAN\")}}))'"
                  }}
                ]
              }}
            }}"#
        ),
    )
    .unwrap();

    let engine = HookEngine::new(data, ws.clone());
    let rules = engine.load_rules();
    assert!(!rules[0].sandbox_off);

    let mut ctx = HookContext::new(
        "s1",
        ws,
        "local",
        "default",
        HookEvent::PreToolUse,
        BTreeMap::new(),
    );
    ctx.tool_name = Some("read".into());
    let out = engine.dispatch(ctx).await;
    assert_eq!(out.additional_context, vec!["CLEAN".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn corrupted_managed_hooks_and_invalid_json_on_fail_closed_both_block() {
    let dir = tmpdir("fail-closed");
    let data = dir.join("data");
    let ws = dir.join("ws");
    let managed = dir.join("managed_hooks.json");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();

    // 1. Corrupted managed hooks file must fail closed on blockable stages.
    std::fs::write(&managed, "{ broken").unwrap();
    let engine_bad = (*HookEngine::new(data.clone(), ws.clone()))
        .clone()
        .with_managed_override(managed.clone());
    let mut ctx1 = HookContext::new(
        "s1",
        ws.clone(),
        "local",
        "default",
        HookEvent::PreToolUse,
        BTreeMap::new(),
    );
    ctx1.tool_name = Some("exec".into());
    let out1 = engine_bad.dispatch(ctx1).await;
    assert!(out1.decision.is_blocking(), "{:?}", out1.decision);
    assert_eq!(out1.trace[0].status, HookStepStatus::Errored);

    // 2. fail_closed: true hook outputting truncated/malformed JSON blocks cleanly.
    std::fs::write(
        &managed,
        r#"{
          "hooks": {
            "PreToolUse": [
              {
                "id": "m-bad-json",
                "fail_closed": true,
                "command": "printf '{\"decision\":\"block\"'"
              }
            ]
          }
        }"#,
    )
    .unwrap();
    let mut ctx2 = HookContext::new(
        "s2",
        ws,
        "local",
        "default",
        HookEvent::PreToolUse,
        BTreeMap::new(),
    );
    ctx2.tool_name = Some("exec".into());
    let out2 = engine_bad.dispatch(ctx2).await;
    assert!(out2.decision.is_blocking(), "{:?}", out2.decision);
    assert_eq!(out2.trace[0].status, HookStepStatus::Errored);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn confirm_hook_ask_recommends_deny_by_default() {
    let asks = AskBroker::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let asks_bg = asks.clone();
    let check_task = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Ask {
                id,
                options,
                recommended,
                ..
            } = ev
            {
                assert_eq!(recommended, 1);
                assert!(!options[0].recommended);
                assert!(options[1].recommended);
                assert_eq!(options[1].label, "Deny");
                asks_bg.answer(id, "Deny".into());
                break;
            }
        }
    });

    let err = confirm_hook_ask(&asks, &tx, HookEvent::PreToolUse, "check", "h1")
        .await
        .unwrap_err();
    assert!(err.contains("denied by user"), "{err}");
    check_task.await.unwrap();
}
