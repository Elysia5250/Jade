//! Phase 2 stress & fault-injection tests for Deterministic Lifecycle Hooks.

use super::config::{project_hooks_path, user_hooks_path};
use super::*;
use crate::permissions::set_workspace_trusted;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

fn tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = PathBuf::from("/tmp").join(format!(
        "ehs-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_12_rule_chain_and_concurrent_audit_jsonl_integrity() {
    let dir = tmpdir("chain");
    let data = dir.join("data");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    set_workspace_trusted(&data, &ws, true).unwrap();

    let managed = dir.join("managed.json");
    // 4 Managed + 4 User + 4 Project rules with mixed priorities and explicit statePatch accumulation.
    std::fs::write(
        &managed,
        r#"{
          "hooks": {
            "PreToolUse": [
              {"id": "m-p20", "priority": 20, "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); s=c[\"state\"].get(\"seq\",[]); print(json.dumps({\"statePatch\":{\"seq\":s+[\"m-p20\"]}}))'"},
              {"id": "m-p10-a", "priority": 10, "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); s=c[\"state\"].get(\"seq\",[]); print(json.dumps({\"statePatch\":{\"seq\":s+[\"m-p10-a\"]}}))'"},
              {"id": "m-p10-b", "priority": 10, "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); s=c[\"state\"].get(\"seq\",[]); print(json.dumps({\"statePatch\":{\"seq\":s+[\"m-p10-b\"]}}))'"},
              {"id": "m-always", "priority": 99, "always_run": true, "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); s=c[\"state\"].get(\"seq\",[]); print(json.dumps({\"statePatch\":{\"seq\":s+[\"m-always\"]}}))'"}
            ]
          }
        }"#,
    )
    .unwrap();
    std::fs::write(
        user_hooks_path(&data),
        r#"{
          "hooks": {
            "PreToolUse": [
              {"id": "u-1", "priority": 1, "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); s=c[\"state\"].get(\"seq\",[]); print(json.dumps({\"statePatch\":{\"seq\":s+[\"u-1\"]}}))'"},
              {"id": "u-2", "priority": 2, "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); s=c[\"state\"].get(\"seq\",[]); print(json.dumps({\"statePatch\":{\"seq\":s+[\"u-2\"]}, \"updatedInput\":{\"extra\":\"ok\"}}))'"},
              {"id": "u-3", "priority": 3, "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); s=c[\"state\"].get(\"seq\",[]); print(json.dumps({\"statePatch\":{\"seq\":s+[\"u-3\"]}}))'"},
              {"id": "u-4", "priority": 4, "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); s=c[\"state\"].get(\"seq\",[]); print(json.dumps({\"statePatch\":{\"seq\":s+[\"u-4\"]}}))'"}
            ]
          }
        }"#,
    )
    .unwrap();
    std::fs::create_dir_all(ws.join(".envoy")).unwrap();
    std::fs::write(
        project_hooks_path(&ws),
        r#"{
          "hooks": {
            "PreToolUse": [
              {"id": "p-1", "priority": 1, "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); s=c[\"state\"].get(\"seq\",[]); print(json.dumps({\"statePatch\":{\"seq\":s+[\"p-1\"]}}))'"},
              {"id": "p-2", "priority": 2, "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); s=c[\"state\"].get(\"seq\",[]); print(json.dumps({\"statePatch\":{\"seq\":s+[\"p-2\"]}}))'"},
              {"id": "p-3", "priority": 3, "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); s=c[\"state\"].get(\"seq\",[]); print(json.dumps({\"statePatch\":{\"seq\":s+[\"p-3\"]}}))'"},
              {"id": "p-4", "priority": 4, "command": "python3 -c 'import sys,json; c=json.load(sys.stdin); s=c[\"state\"].get(\"seq\",[]); print(json.dumps({\"statePatch\":{\"seq\":s+[\"p-4\"]}}))'"}
            ]
          }
        }"#,
    )
    .unwrap();

    let engine = Arc::new((*HookEngine::new(data.clone(), ws.clone())).clone().with_managed_override(managed));

    // Fire 12 concurrent dispatches (simulating main agent + parallel subagents writing large audit lines).
    let started = Instant::now();
    let mut handles = Vec::new();
    for i in 0..12 {
        let eng = Arc::clone(&engine);
        let ws_c = ws.clone();
        handles.push(tokio::spawn(async move {
            let mut ctx = HookContext::new(
                format!("sess-{i}"),
                ws_c,
                "local",
                "default",
                HookEvent::PreToolUse,
                BTreeMap::new(),
            );
            ctx.turn_seq = 1;
            ctx.step = i;
            ctx.tool_name = Some("shell".into());
            ctx.tool_input = Some(json!({"command": format!("echo {i}")}));
            let out = eng.dispatch(ctx).await;
            let seq: Vec<String> = serde_json::from_value(out.state["seq"].clone()).unwrap();
            assert_eq!(
                seq,
                vec![
                    "m-p10-a", "m-p10-b", "m-p20", "m-always",
                    "u-1", "u-2", "u-3", "u-4",
                    "p-1", "p-2", "p-3", "p-4"
                ]
            );
            assert_eq!(out.tool_input.unwrap()["extra"], "ok");
            assert_eq!(out.trace.len(), 12);
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    // Every line in hook_audit.jsonl must be valid standalone JSON (no torn writes).
    let audit_path = data.join("agent").join("hook_audit.jsonl");
    let raw = std::fs::read_to_string(&audit_path).unwrap();
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 12);
    for (idx, line) in lines.iter().enumerate() {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("corrupt audit line {idx}: {e}\n{line}"));
        assert_eq!(v["hooks_ran"].as_u64(), Some(12));
    }

    eprintln!(
        "[stress_hooks] 12x12 hook chain + audit jsonl passed in {:?}",
        started.elapsed()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_faulty_hooks_flood_crash_timeout_and_shutdown_budget() {
    let dir = tmpdir("faults");
    let data = dir.join("data");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();

    // 1MB garbage stdout + non-zero exit (fail_open) + fail_closed timeout with child grandchild
    std::fs::write(
        user_hooks_path(&data),
        r#"{
          "hooks": {
            "PreToolUse": [
              {
                "id": "flood-1mb",
                "priority": 1,
                "command": "python3 -c 'import sys; sys.stdout.write(\"A\"*(1024*1024)); sys.stdout.flush()'"
              },
              {
                "id": "crash-fail-open",
                "priority": 2,
                "fail_closed": false,
                "command": "echo 'garbage-not-json'; exit 42"
              },
              {
                "id": "timeout-fail-closed",
                "priority": 3,
                "timeout_ms": 150,
                "fail_closed": true,
                "command": "sleep 20 & wait"
              },
              {
                "id": "never-reached",
                "priority": 4,
                "command": "echo '{\"decision\":\"allow\"}'"
              },
              {
                "id": "always-cleanup",
                "priority": 5,
                "always_run": true,
                "command": "touch cleanup.ok && echo '{\"statePatch\":{\"cleaned\":true}}'"
              }
            ],
            "SessionEnd": [
              {"id": "end-1", "priority": 1, "timeout_ms": 5000, "command": "sleep 10"},
              {"id": "end-2", "priority": 2, "timeout_ms": 5000, "command": "sleep 10"},
              {"id": "end-3", "priority": 3, "timeout_ms": 5000, "command": "sleep 10"}
            ]
          }
        }"#,
    )
    .unwrap();

    let engine = HookEngine::new(data.clone(), ws.clone());

    let mut ctx = HookContext::new(
        "s-fault",
        ws.clone(),
        "local",
        "default",
        HookEvent::PreToolUse,
        BTreeMap::new(),
    );
    ctx.turn_seq = 1;
    ctx.step = 1;
    ctx.tool_name = Some("shell".into());
    let t0 = Instant::now();
    let out = engine.dispatch(ctx).await;
    let pre_elapsed = t0.elapsed();
    assert!(
        pre_elapsed < Duration::from_secs(2),
        "PreToolUse timeout + PGID kill took too long: {pre_elapsed:?}"
    );
    assert!(out.decision.is_blocking());
    assert!(ws.join("cleanup.ok").is_file(), "always_run command did not execute");
    assert!(!out.state.contains_key("cleaned"), "halted hook chain must freeze state mutations");
    assert_eq!(out.trace.len(), 5);
    assert_eq!(out.trace[2].status, HookStepStatus::TimedOut);
    assert_eq!(out.trace[3].status, HookStepStatus::Skipped);
    assert_eq!(out.trace[4].status, HookStepStatus::Succeeded);

    // SessionEnd must honor SHUTDOWN_HOOK_TIMEOUT_MS total budget even with 3x 10s sleeping hooks.
    let mut end_ctx = HookContext::new(
        "s-fault",
        ws.clone(),
        "local",
        "default",
        HookEvent::SessionEnd,
        BTreeMap::new(),
    );
    end_ctx.turn_seq = 1;
    end_ctx.step = 2;
    let t1 = Instant::now();
    let end_out = engine
        .dispatch_bounded(end_ctx, Some(SHUTDOWN_HOOK_TIMEOUT_MS))
        .await;
    let end_elapsed = t1.elapsed();
    assert!(
        end_elapsed < Duration::from_millis(SHUTDOWN_HOOK_TIMEOUT_MS + 350),
        "SessionEnd exceeded budget: {end_elapsed:?}"
    );
    assert_eq!(end_out.trace[0].status, HookStepStatus::TimedOut);
    assert!(end_out.trace[1..]
        .iter()
        .all(|t| t.status == HookStepStatus::Skipped));

    eprintln!(
        "[stress_hooks] fault-injection PreToolUse={pre_elapsed:?}, SessionEnd={end_elapsed:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
