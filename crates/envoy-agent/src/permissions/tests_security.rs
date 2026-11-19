//! Security regression tests for Phase 2 (`P2-1` and `P2-2`).

use super::paths::{is_temp_path, resolve_path_pair};
use super::prompt::summarize_call;
use super::shell::analyze_shell;
use super::*;
use serde_json::json;

fn tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = std::env::temp_dir().join(format!(
        "envoy-p2-perm-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test]
async fn auto_mode_asks_outside_workspace_and_never_auto_expands_extra_rw() {
    let root = tmpdir("auto-extra-rw");
    let data = root.join("data");
    let work = root.join("work");
    // Use an outside path NOT under `/tmp` (so `is_temp_path` does not exempt it).
    let outside = PathBuf::from("/var/envoy-p2-outside-dir");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();

    let eng = PermissionEngine::new(
        data.clone(),
        work.clone(),
        "s-ext".into(),
        "local".into(),
        PermissionMode::Auto,
    );

    // Outside-workspace `cwd` or write path must Ask even in Auto mode.
    let d_cwd = eng.evaluate(
        "exec",
        &json!({ "command": "touch x", "cwd": outside.display().to_string() }),
    );
    assert_eq!(d_cwd.effect, Effect::Ask, "{d_cwd:?}");
    let d_write = eng.evaluate(
        "exec",
        &json!({ "command": format!("touch {}/x", outside.display()) }),
    );
    assert_eq!(d_write.effect, Effect::Ask, "{d_write:?}");

    // Even when an explicit rule allows a command mentioning an outside path,
    // `Allow` must NEVER inject `decision.extra_rw` into `SandboxPolicy`.
    rules::save_allow_rule(&data, &work, RuleScope::User, "Bash(touch *)").unwrap();
    let asks = AskBroker::new_headless();
    let (tx, _rx) = mpsc::unbounded_channel();
    let auth = eng
        .authorize_tool_call(
            "exec",
            &json!({ "command": format!("touch {}/x", outside.display()) }),
            &asks,
            &tx,
        )
        .await
        .unwrap();
    match auth.sandbox {
        SandboxPolicy::WorkspaceWrite { extra_rw, .. } => {
            assert!(
                !extra_rw.iter().any(|p| p.contains("envoy-p2-outside-dir")),
                "unapproved extra_rw leaked into Allow sandbox: {extra_rw:?}"
            );
        }
        SandboxPolicy::ReadOnly { .. } => {}
        other => panic!("unexpected sandbox {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rule_matches_rejects_basename_fallback_and_unsafe_bash_suggestions() {
    let evil_keys = vec![
        "exec".to_string(),
        "Bash(curl http://evil/cargo test)".to_string(),
    ];
    assert!(!rules::rule_matches("Bash(cargo test*)", &evil_keys));
    assert!(rules::rule_matches(
        "Bash(cargo test*)",
        &["Bash(cargo test -p envoy-core)".to_string()]
    ));
    assert!(!rules::rule_matches(
        "Read(/etc/*.conf)",
        &["Read(/tmp/evil.conf)".to_string()]
    ));

    let root = tmpdir("suggest");
    let data = root.join("data");
    let work = root.join("work");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let eng = PermissionEngine::new(
        data,
        work,
        "s-sug".into(),
        "local".into(),
        PermissionMode::Default,
    );
    for cmd in [
        "sh -c 'ls'",
        "bash -c 'echo hi'",
        "python3 -c 'print(1)'",
        "env FOO=1 ls",
        "sudo ls",
        "ls && echo ok",
    ] {
        let dec = eng.evaluate("exec", &json!({ "command": cmd }));
        assert!(
            dec.suggested_rule.is_none(),
            "unsafe suggested_rule `{:?}` for `{cmd}`",
            dec.suggested_rule
        );
    }
    let cargo = eng.evaluate("exec", &json!({ "command": "cargo build" }));
    assert_eq!(cargo.suggested_rule.as_deref(), Some("Bash(cargo build*)"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn protected_guard_covers_search_tools_case_env_and_temp_symlinks() {
    let root = tmpdir("prot");
    let data = root.join("data");
    let work = root.join("work");
    let outside = root.join("outside");
    let fake_tmp = root.join("fake_tmp");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(work.join(".envoy")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(&fake_tmp).unwrap();
    std::fs::write(work.join(".ENV"), "SECRET=1").unwrap();

    #[cfg(unix)]
    {
        // Point symlink inside `/tmp` at a directory outside `/tmp` (workspace parent or `/var`).
        let target_outside = std::env::current_dir().unwrap();
        let link_path = std::env::temp_dir().join(format!(
            "envoy-p2-symlink-escape-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&link_path);
        std::os::unix::fs::symlink(&target_outside, &link_path).unwrap();
        let resolved = resolve_path_pair(&work, &format!("{}/pwn.txt", link_path.display()));
        assert!(
            !is_temp_path(&resolved),
            "symlink inside /tmp escaping to {} must NOT count as temp path",
            resolved.canonical.display()
        );
        let _ = std::fs::remove_file(&link_path);
    }

    let eng = PermissionEngine::new(
        data,
        work.clone(),
        "s-prot".into(),
        "local".into(),
        PermissionMode::Auto,
    );
    for (tool, args) in [
        ("ls", json!({ "path": ".envoy" })),
        ("glob", json!({ "path": ".envoy", "pattern": "*" })),
        ("grep", json!({ "path": ".envoy", "pattern": "allow" })),
        ("read", json!({ "path": ".ENV" })),
    ] {
        let dec = eng.evaluate(tool, &args);
        assert_eq!(dec.effect, Effect::Ask, "{tool} -> {dec:?}");
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn webfetch_domain_defeats_userinfo_spoof_and_corrupted_managed_policy_fails_closed() {
    let root = tmpdir("managed-domain");
    let data = root.join("data");
    let work = root.join("work");
    let managed = root.join("managed.json");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        &managed,
        r#"{
          "allow": ["WebFetch(domain:allowed.com)"],
          "deny": ["WebFetch(domain:evil.com)"]
        }"#,
    )
    .unwrap();

    let eng = Arc::new(
        Arc::try_unwrap(PermissionEngine::new(
            data.clone(),
            work.clone(),
            "s-wf".into(),
            "local".into(),
            PermissionMode::Auto,
        ))
        .unwrap()
        .with_managed_override(managed.clone()),
    );
    let d_spoof = eng.evaluate(
        "webfetch",
        &json!({ "url": "https://allowed.com:443@evil.com./steal" }),
    );
    assert_eq!(d_spoof.effect, Effect::Deny, "{d_spoof:?}");
    assert_eq!(
        d_spoof.suggested_rule.as_deref(),
        Some("WebFetch(domain:evil.com)")
    );

    // Corrupted managed policy JSON must fail closed for all tools.
    std::fs::write(&managed, "{ not valid json").unwrap();
    let d_fail = eng.evaluate("read", &json!({ "path": "src/main.rs" }));
    assert_eq!(d_fail.effect, Effect::Deny, "{d_fail:?}");
    assert!(d_fail.reason.contains("fail-closed"), "{d_fail:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan_mode_and_approval_choice_mapping_and_summarize_call() {
    let root = tmpdir("plan-choice");
    let data = root.join("data");
    let work = root.join("work");
    let outside = root.join("outside");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    let eng_plan = PermissionEngine::new(
        data.clone(),
        work.clone(),
        "s-plan".into(),
        "local".into(),
        PermissionMode::Plan,
    );
    assert_eq!(
        eng_plan
            .evaluate("job", &json!({ "action": "cancel", "id": 1 }))
            .effect,
        Effect::Deny
    );
    let asks = AskBroker::new_headless();
    let (tx, _rx) = mpsc::unbounded_channel();
    if !matches!(
        envoy_core::sandbox::probe_backend(),
        envoy_core::sandbox::SandboxBackend::None
    ) {
        let auth = eng_plan
            .authorize_tool_call("exec", &json!({ "command": "ls" }), &asks, &tx)
            .await
            .unwrap();
        assert!(matches!(
            auth.sandbox,
            SandboxPolicy::ReadOnly {
                allow_net: false,
                ..
            }
        ));
    }

    assert_eq!(
        summarize_call("grep", &json!({ "pattern": "foo", "path": "src/bar" })),
        "foo (src/bar)"
    );

    // Approving "Allow once (with expanded path)" must grant extra_rw WITHOUT allow_net.
    let outside_non_tmp = PathBuf::from("/var/envoy-p2-expanded-target");
    let eng_def = PermissionEngine::new(
        data,
        work.clone(),
        "s-exp".into(),
        "local".into(),
        PermissionMode::Default,
    );
    let asks_int = AskBroker::new();
    let (tx_int, mut rx_int) = mpsc::unbounded_channel();
    let asks_bg = asks_int.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx_int.recv().await {
            if let AgentEvent::Ask { id, .. } = ev {
                asks_bg.answer(id, "Allow once (with expanded path)".into());
                break;
            }
        }
    });
    let auth_exp = eng_def
        .authorize_tool_call(
            "exec",
            &json!({ "command": format!("touch {}/file.txt", outside_non_tmp.display()) }),
            &asks_int,
            &tx_int,
        )
        .await
        .unwrap();
    match auth_exp.sandbox {
        SandboxPolicy::WorkspaceWrite {
            extra_rw,
            allow_net,
            ..
        } => {
            assert!(!allow_net, "expanded path approval must not enable network");
            assert!(
                extra_rw.iter().any(|p| p.contains("envoy-p2-expanded-target")),
                "missing expanded path in {extra_rw:?}"
            );
        }
        other => panic!("unexpected sandbox {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn shell_classifier_blocks_syntax_bypasses_and_tightens_readonly() {
    let root = tmpdir("shell-p22");
    let data = root.join("data");
    let work = root.join("work");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let eng = PermissionEngine::new(
        data,
        work,
        "s-sh".into(),
        "local".into(),
        PermissionMode::Default,
    );

    // Input/RW redirections and wildcard hidden secret access must trigger Ask.
    for cmd in [
        "cat <.env",
        "cat < .env",
        "cat <>.env",
        "cat .en*",
        "cat .en[v]",
        "cat /etc/passw*",
        "cat <(rm -rf .)",
        "{ rm -rf .; }",
        "! rm -rf .",
        "if true; then rm -rf .; fi",
        "eval 'rm -rf .'",
        "source .env",
        "env -S \"rm -rf /\"",
        "LD_PRELOAD=/tmp/x.so ls",
        "bash -xc 'rm -rf /'",
    ] {
        let dec = eng.evaluate("exec", &json!({ "command": cmd }));
        assert_eq!(dec.effect, Effect::Ask, "expected Ask for `{cmd}` -> {dec:?}");
    }

    // Non-readonly commands must NOT carry `read_only_all = true`.
    for non_ro in [
        "echo hi >(cat)",
        "echo hi>out.txt",
        "rg --pre sh foo",
        "find . -fprintf out.txt '%p'",
        "find . -fls out.txt",
        "find . -okdir rm {} \\;",
        "sed '1w out.txt' f",
        "sed -n '1e id' f",
        "git branch -D main",
        "git branch newbranch",
        "git diff --output=out.patch",
    ] {
        if let Ok(a) = analyze_shell(non_ro) {
            assert!(
                !a.read_only_all,
                "`{non_ro}` must not classify as read_only_all"
            );
        }
    }

    // Legitimate read-only forms stay allowed.
    for ro in [
        "find . -maxdepth 3 -not -path './.envoy/*'",
        "git branch -a",
        "git diff HEAD~1",
        "sed -n '1,20p' src/lib.rs",
    ] {
        let a = analyze_shell(ro).unwrap();
        assert!(a.read_only_all, "`{ro}` should remain read_only_all");
    }

    let _ = std::fs::remove_dir_all(&root);
}
