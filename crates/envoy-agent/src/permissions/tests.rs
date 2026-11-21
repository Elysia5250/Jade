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
    let eng = Arc::new(Arc::try_unwrap(eng).unwrap().with_managed_override(managed));

    let d1 = eng.evaluate("exec", &json!({"command": "rm -rf build"}));
    assert_eq!(d1.effect, Effect::Deny, "{d1:?}");

    let d2 = eng.evaluate("exec", &json!({"command": "git status"}));
    assert_eq!(d2.effect, Effect::Deny, "{d2:?}");

    // Compound command cannot bypass allow via safe first segment.
    rules::save_allow_rule(&data, &work, RuleScope::User, "Bash(ls*)").unwrap();
    let d3 = eng.evaluate("exec", &json!({"command": "ls && rm -rf build"}));
    assert_eq!(d3.effect, Effect::Deny, "{d3:?}");

    // Unparseable shell is contained by an enforcing sandbox, so `auto` lets
    // it through; without one it must Ask.
    let d4 = eng.evaluate("exec", &json!({"command": "echo 'unterminated"}));
    let want = if envoy_core::sandbox::capabilities().enforces_workspace_boundary() {
        Effect::Allow
    } else {
        Effect::Ask
    };
    assert_eq!(d4.effect, want, "{d4:?}");

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
    let eng = Arc::new(Arc::try_unwrap(eng).unwrap().with_managed_override(managed));
    let asks = AskBroker::new_headless();
    let (tx, _rx) = mpsc::unbounded_channel();

    let err = eng
        .authorize_tool_call(
            "exec",
            &json!({"command": "curl https://example.com"}),
            &asks,
            &tx,
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("requires interactive approval"),
        "{err}"
    );

    eng.set_skip_permissions(true);
    let auth = eng
        .authorize_tool_call(
            "exec",
            &json!({"command": "curl https://example.com"}),
            &asks,
            &tx,
        )
        .await
        .unwrap();
    assert_eq!(auth.sandbox, SandboxPolicy::DangerFullAccess);

    eng.set_sandbox_override(Some(SandboxOverride::WorkspaceWrite));
    let auth_ws = eng
        .authorize_tool_call("exec", &json!({"command": "ls"}), &asks, &tx)
        .await
        .unwrap();
    assert!(matches!(
        auth_ws.sandbox,
        SandboxPolicy::WorkspaceWrite { .. }
    ));

    let denied = eng
        .authorize_tool_call("exec", &json!({"command": "sudo id"}), &asks, &tx)
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("managed deny"), "{denied}");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn auto_mode_approves_everything_once_the_sandbox_is_enforcing() {
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

    // Non-allowlisted network exec always Asks in Auto (even with an enforcing
    // sandbox). Allowlisted hosts (e.g. github.com) Allow with `allow_net: true`
    // once a strong sandbox backend enforces the workspace boundary.
    let err = eng
        .authorize_tool_call(
            "exec",
            &json!({"command": "curl https://example.com"}),
            &asks,
            &tx,
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("requires interactive approval"),
        "{err}"
    );
    let isolated = envoy_core::sandbox::capabilities().enforces_workspace_boundary();
    if isolated {
        let net_auth = eng
            .authorize_tool_call(
                "exec",
                &json!({"command": "curl -fsSL https://github.com/org/repo/releases/download/v1/app.tar.gz"}),
                &asks,
                &tx,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                net_auth.sandbox,
                SandboxPolicy::WorkspaceWrite {
                    allow_net: true,
                    ..
                }
            ),
            "allowlisted net exec in auto mode should enable allow_net: {:?}",
            net_auth.sandbox
        );
    }
    // With an enforcing sandbox, Auto stops prompting: the sandbox is the
    // boundary. Without one, the old conservative asks stay.
    let isolated = envoy_core::sandbox::capabilities().enforces_workspace_boundary();
    let want = if isolated { Effect::Allow } else { Effect::Ask };
    for command in ["rm -rf build", "git reset --hard", "kill 1"] {
        assert_eq!(
            eng.evaluate("exec", &json!({"command": command})).effect,
            want,
            "{command}"
        );
    }
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
        Effect::Ask
    );
    assert_eq!(
        eng.evaluate("webfetch", &json!({"url": "https://example.com"}))
            .effect,
        Effect::Ask
    );
    assert_eq!(
        eng.evaluate("webfetch", &json!({"url": "https://github.com/org/repo"}))
            .effect,
        Effect::Allow
    );
    assert_eq!(
        eng.evaluate("write", &json!({"path": "/tmp/probe.txt", "content": "ok"}))
            .effect,
        Effect::Allow
    );

    // Protected paths are the documented floor; everything else the sandbox
    // contains is allowed once isolation is enforcing.
    assert_eq!(
        eng.evaluate("exec", &json!({"command": "sudo ls"})).effect,
        Effect::Ask
    );
    assert_eq!(
        eng.evaluate("read", &json!({"path": ".env"})).effect,
        Effect::Ask
    );
    assert_eq!(
        eng.evaluate(
            "write",
            &json!({"path": ".envoy/permissions.json", "content": "{}"})
        )
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
fn default_allows_in_workspace_exec_only_when_sandboxed() {
    let root = tmpdir("default-exec");
    let data = root.join("data");
    let work = root.join("work");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let eng = PermissionEngine::new(
        data,
        work,
        "s-default".into(),
        "local".into(),
        PermissionMode::Default,
    );
    let isolated = envoy_core::sandbox::capabilities().enforces_workspace_boundary();
    let cargo = eng.evaluate("exec", &json!({"command": "cargo test"}));
    if isolated {
        assert_eq!(cargo.effect, Effect::Allow, "{cargo:?}");
    } else {
        assert_eq!(cargo.effect, Effect::Ask, "{cargo:?}");
    }
    assert_eq!(
        eng.evaluate("exec", &json!({"command": "curl https://example.com"}))
            .effect,
        Effect::Ask
    );
    assert_eq!(
        eng.evaluate("exec", &json!({"command": "rm -rf build"}))
            .effect,
        Effect::Ask
    );
    assert_eq!(
        eng.evaluate("write", &json!({"path": "src/lib.rs", "content": "x"}))
            .effect,
        Effect::Ask
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn remote_target_still_carries_workspace_sandbox_policy() {
    let policy = sandbox_for(Path::new("/work"), &[], false, false, false, None);
    assert!(
        matches!(
            policy,
            SandboxPolicy::WorkspaceWrite {
                allow_net: false,
                ..
            }
        ),
        "{policy:?}"
    );
    let ro = sandbox_for(Path::new("/work"), &[], true, true, true, None);
    assert!(matches!(
        ro,
        SandboxPolicy::ReadOnly {
            allow_net: false,
            ..
        }
    ));
    let off = sandbox_for(
        Path::new("/work"),
        &[],
        true,
        false,
        false,
        Some(SandboxOverride::Off),
    );
    assert!(matches!(off, SandboxPolicy::Off));
    let danger = sandbox_for(Path::new("/work"), &[], false, true, false, None);
    assert!(matches!(danger, SandboxPolicy::DangerFullAccess));
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

#[test]
fn turbo_mode_never_prompts_but_deny_rules_still_win() {
    let root = tmpdir("turbo");
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
        "s-turbo".into(),
        "local".into(),
        PermissionMode::Turbo,
    );

    // Every baseline ask, guard, and unparseable command is allowed.
    for (tool, args) in [
        ("exec", json!({"command": "sudo ls"})),
        ("exec", json!({"command": "rm -rf /"})),
        ("exec", json!({"command": "kill 1"})),
        ("exec", json!({"command": "echo 'unterminated"})),
        (
            "exec",
            json!({"command": "for f in *.md; do echo $f; done"}),
        ),
        (
            "write",
            json!({"path": "/tmp/turbo-out.txt", "content": "x"}),
        ),
        ("webfetch", json!({"url": "https://example.com"})),
        ("mcp__files__read", json!({"path": "a"})),
    ] {
        let d = eng.evaluate(tool, &args);
        assert_eq!(d.effect, Effect::Allow, "{tool} {args}: {d:?}");
    }

    // Protected paths are the one floor turbo keeps.
    assert_eq!(
        eng.evaluate("read", &json!({"path": ".env"})).effect,
        Effect::Ask
    );

    // The hard floor is untouched: an explicit deny still denies.
    rules::save_rule(
        &data,
        &work,
        RuleScope::User,
        Effect::Deny,
        "Bash(git push*)",
    )
    .unwrap();
    let d = eng.evaluate("exec", &json!({"command": "git push origin main"}));
    assert_eq!(d.effect, Effect::Deny, "{d:?}");
    assert_eq!(
        eng.evaluate("exec", &json!({"command": "git status"}))
            .effect,
        Effect::Allow
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn turbo_mode_labels_and_cycles() {
    assert_eq!(PermissionMode::parse("turbo"), Some(PermissionMode::Turbo));
    assert_eq!(PermissionMode::parse("yolo"), Some(PermissionMode::Turbo));
    assert_eq!(PermissionMode::Turbo.label(), "turbo");
    assert_eq!(PermissionMode::Turbo.short_badge(), "perm:turbo");
    assert_eq!(PermissionMode::Auto.cycle(), PermissionMode::Turbo);
    assert_eq!(PermissionMode::Turbo.cycle(), PermissionMode::Default);
}

#[tokio::test]
async fn turbo_mode_keeps_the_workspace_sandbox() {
    let root = tmpdir("turbo-sandbox");
    let data = root.join("data");
    let work = root.join("work");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let eng = PermissionEngine::new(
        data,
        work,
        "s-turbo-sb".into(),
        "local".into(),
        PermissionMode::Turbo,
    );
    // Allowing every rule decision does not widen the OS sandbox.
    let (tx, _rx) = mpsc::unbounded_channel();
    let auth = eng
        .authorize_tool_call(
            "exec",
            &json!({"command": "cargo build"}),
            &AskBroker::new_headless(),
            &tx,
        )
        .await
        .unwrap();
    assert!(
        !matches!(auth.sandbox, SandboxPolicy::DangerFullAccess),
        "turbo must not disable the sandbox: {:?}",
        auth.sandbox
    );

    let _ = std::fs::remove_dir_all(&root);
}
