//! Permission engine and shell parser tests.
use super::shell::analyze_shell;
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

    let err = eng
        .authorize_tool_call("exec", &json!({"command": "curl https://example.com"}), &asks, &tx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("requires interactive approval"), "{err}");

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

#[tokio::test]
async fn auto_mode_approves_routine_dev_ops_while_keeping_safety_guards() {
    let root = tmpdir("auto-mode");
    let data = root.join("data");
    let work = root.join("work");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(work.join(".envoy")).unwrap();
    std::fs::write(work.join(".envoy/permissions.json"), "{}").unwrap();
    std::fs::write(work.join(".env"), "SECRET=1").unwrap();

    let eng = PermissionEngine::new(
        data.clone(),
        work.clone(),
        "s-auto".into(),
        "local".into(),
        PermissionMode::Auto,
    );
    let asks = AskBroker::new_headless();
    let (tx, _rx) = mpsc::unbounded_channel();

    // Network exec + workspace rm + .envoy read + webfetch + /tmp write all Allow in Auto
    let net_auth = eng
        .authorize_tool_call("exec", &json!({"command": "curl https://example.com"}), &asks, &tx)
        .await
        .unwrap();
    assert!(
        matches!(net_auth.sandbox, SandboxPolicy::WorkspaceWrite { allow_net: true, .. }),
        "auto mode allowed net exec should enable allow_net: {:?}",
        net_auth.sandbox
    );
    assert_eq!(
        eng.evaluate("exec", &json!({"command": "rm -rf build"})).effect,
        Effect::Allow
    );
    assert_eq!(
        eng.evaluate(
            "exec",
            &json!({"command": "find . -maxdepth 3 -not -path './.envoy/*' | sort"})
        )
        .effect,
        Effect::Allow
    );
    assert_eq!(
        eng.evaluate("read", &json!({"path": ".envoy/permissions.json"}))
            .effect,
        Effect::Allow
    );
    assert_eq!(
        eng.evaluate("webfetch", &json!({"url": "https://example.com"}))
            .effect,
        Effect::Allow
    );
    assert_eq!(
        eng.evaluate("write", &json!({"path": "/tmp/probe.txt", "content": "ok"}))
            .effect,
        Effect::Allow
    );

    // Safety guards still trigger Ask in Auto
    assert_eq!(
        eng.evaluate("exec", &json!({"command": "sudo ls"})).effect,
        Effect::Ask
    );
    assert_eq!(
        eng.evaluate("read", &json!({"path": ".env"})).effect,
        Effect::Ask
    );
    assert_eq!(
        eng.evaluate("write", &json!({"path": ".envoy/permissions.json", "content": "{}"}))
            .effect,
        Effect::Ask
    );
    assert_eq!(
        eng.evaluate("exec", &json!({"command": "rm -rf /"})).effect,
        Effect::Ask
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn shell_splits_compound_and_unwraps_wrappers() {
    let a = analyze_shell("env FOO=1 timeout 10 sudo rm -rf /tmp/x && cat .env").unwrap();
    assert_eq!(a.segments.len(), 2);
    assert!(a.segments[0].uses_sudo);
    assert_eq!(a.segments[0].argv[0], "rm");
    assert!(a.destructive);
    assert!(a.referenced_paths.contains(&".env".to_string()));
    assert!(!a.read_only_all);
}

#[test]
fn shell_unwraps_sh_c_and_subshells() {
    let a = analyze_shell("sh -c 'echo ok; curl https://example.com > out.txt'").unwrap();
    assert!(a.needs_net);
    assert!(a.write_paths.contains(&"out.txt".to_string()));
    assert!(!a.read_only_all);

    let sub = analyze_shell("echo $(rm -rf src)").unwrap();
    assert!(sub.destructive);
    assert!(!sub.read_only_all);
}

#[test]
fn shell_parse_failure_returns_err() {
    assert!(analyze_shell("echo 'unclosed").is_err());
    assert!(analyze_shell("echo $(ls").is_err());
    assert!(analyze_shell("   ").is_err());
}
