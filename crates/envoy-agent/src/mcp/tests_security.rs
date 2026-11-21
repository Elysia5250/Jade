//! Security regression tests for Phase 2 (`P2-4`).

use super::client::McpManager;
use super::config::{load_mcp_configs, qualify_tool_name, safe_stdio_env, McpScope};
use super::server::{call_tool, Mcp};
use crate::agent::tools::web::validate_http_url;
use crate::permissions::{
    rules, set_workspace_trusted, Effect, PermissionEngine, PermissionMode, RuleScope,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn tmpdir(tag: &str) -> PathBuf {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let p = PathBuf::from("/tmp").join(format!(
        "envoy-mcp-p24-{tag}-{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3]
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test]
async fn mcp_server_normalizes_relative_paths_rejects_ld_preload_and_audits() {
    let dir = tmpdir("srv-rel");
    let data = dir.join("data");
    let cwd = dir.join("repo");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    // Auto mode would auto-allow relative `.bashrc` if resolved to `$HOME/.bashrc`,
    // Wait: normalizing relative paths to `mcp.cwd` means `.bashrc` targets
    // `<cwd>/.bashrc` (NEVER `$HOME/.bashrc`), while `../.bashrc` escapes workspace -> Ask!
    let mcp = Mcp {
        data_dir: data.clone(),
        cwd: cwd.clone(),
        conduit: None,
        permissions: PermissionEngine::new(
            data.clone(),
            cwd.clone(),
            "mcp-sec".into(),
            "local".into(),
            PermissionMode::Default,
        ),
    };

    // 1. Escaping relative path `../.bashrc` must ask & be logged in audit.
    let (msg, _, is_err) = call_tool(
        &mcp,
        &json!({ "name": "write", "arguments": { "path": "../.bashrc", "content": "pwn" } }),
    )
    .await
    .unwrap();
    assert!(is_err, "{msg}");
    assert!(msg.contains("requires approval"), "{msg}");
    let audit = std::fs::read_to_string(crate::permissions::audit::audit_log_path(&data)).unwrap();
    assert!(audit.contains("headless_ask_denied"), "{audit}");

    // 2. Dangerous env vars like LD_PRELOAD and BASH_ENV are rejected outright.
    for key in ["LD_PRELOAD", "BASH_ENV", "PATH"] {
        let mut env = serde_json::Map::new();
        env.insert(key.into(), json!("/tmp/evil"));
        let (msg, _, is_err) = call_tool(
            &mcp,
            &json!({
                "name": "exec",
                "arguments": { "command": "ls", "env": env }
            }),
        )
        .await
        .unwrap();
        assert!(is_err, "{key} -> {msg}");
        assert!(
            msg.contains("disallowed environment variable"),
            "{key} -> {msg}"
        );
    }

    // 3. Relative `.bashrc` is read from `mcp.cwd/.bashrc` (NEVER `$HOME/.bashrc`),
    //    while `../../...` escapes workspace and requires approval!
    let _ = envoy_core::AgentConfigStore::open(data.clone()).unwrap();
    let sock_path = crate::daemon::socket_path(&data);
    let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
    crate::daemon::park_agent_sock(&data);
    let srv = envoy_service::AgentServer::open_store(data.clone()).unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        crate::daemon::run_server(srv, listener, sock_path, None, async {
            let _ = stop_rx.await;
        })
        .await;
    });

    std::fs::write(cwd.join(".bashrc"), "WORKSPACE_RC_ONLY\n").unwrap();
    let eng_auto = Mcp {
        data_dir: data.clone(),
        cwd: cwd.clone(),
        conduit: None,
        permissions: PermissionEngine::new(
            data,
            cwd,
            "mcp-auto".into(),
            "local".into(),
            PermissionMode::Auto,
        ),
    };
    let local_rc = call_tool(
        &eng_auto,
        &json!({ "name": "read", "arguments": { "path": ".bashrc" } }),
    )
    .await
    .unwrap();
    assert!(
        !local_rc.2,
        "expected local .bashrc read ok: {}",
        local_rc.0
    );
    assert!(local_rc.0.contains("WORKSPACE_RC_ONLY"), "{}", local_rc.0);

    // A path that climbs out of the workspace must never land on disk. With an
    // enforcing sandbox `auto` allows the call and the OS boundary rejects it;
    // without one the policy layer asks (and headless asks fail closed).
    let escaped = std::path::Path::new("/var/nonexistent_file_xyz");
    let res = call_tool(
        &eng_auto,
        &json!({ "name": "write", "arguments": { "path": "../../../../../../var/nonexistent_file_xyz", "content": "x" } }),
    )
    .await;
    let why = match res {
        Ok((msg, _, is_err)) => {
            assert!(is_err, "escaping write must be an error result: {msg}");
            msg
        }
        Err(e) => format!("{e}"),
    };
    assert!(!escaped.exists(), "escaping write reached the disk: {why}");
    assert!(!why.is_empty(), "the failure must be reported");
    let _ = stop_tx.send(());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mcp_config_blocks_project_override_and_secret_env_expansion() {
    const TEST_ENV: &str = "ENVOY_TEST_SECRET_MCP_P24";
    std::env::set_var(TEST_ENV, "secret_value_should_not_leak");

    let dir = tmpdir("cfg");
    let data = dir.join("data");
    let cwd = dir.join("repo");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    set_workspace_trusted(&data, &cwd, true).unwrap();

    std::fs::write(
        data.join("mcp.json"),
        r#"{ "mcpServers": { "shared": { "command": "user-bin" } } }"#,
    )
    .unwrap();
    std::fs::write(
        cwd.join(".mcp.json"),
        r#"{ "mcpServers": {
            "shared": { "command": "evil-project-bin" },
            "proj": { "command": "echo" }
        } }"#,
    )
    .unwrap();

    let report = load_mcp_configs(&data, &cwd);
    let shared = report.servers.iter().find(|s| s.name == "shared").unwrap();
    assert_eq!(shared.scope, McpScope::User);
    assert!(
        matches!(&shared.transport, super::config::McpTransportConfig::Stdio { command, .. } if command == "user-bin")
    );

    let mut declared = BTreeMap::new();
    declared.insert("SAFE_ARG".into(), format!("${{{TEST_ENV}}}"));
    declared.insert("PATH_CHECK".into(), "${PATH}".into());
    declared.insert("LD_PRELOAD".into(), "/tmp/pwn.so".into());
    let proj_env = safe_stdio_env(&declared, McpScope::Project);
    assert_eq!(proj_env.get("SAFE_ARG").map(String::as_str), Some(""));
    assert!(!proj_env
        .get("PATH_CHECK")
        .map(String::as_str)
        .unwrap_or("")
        .is_empty());
    assert!(!proj_env.contains_key("LD_PRELOAD"));

    // User scope keeps full env var expansion.
    let user_env = safe_stdio_env(&declared, McpScope::User);
    assert_eq!(
        user_env.get("SAFE_ARG").map(String::as_str),
        Some("secret_value_should_not_leak")
    );

    // Project HTTP SSRF guard blocks metadata/RFC1918/Fake-IP while allowing loopback.
    for bad in [
        "http://169.254.169.254/mcp",
        "http://10.0.0.1/mcp",
        "http://198.18.0.1/mcp",
    ] {
        assert!(
            validate_http_url(&reqwest::Url::parse(bad).unwrap(), true).is_err(),
            "{bad}"
        );
    }
    assert!(validate_http_url(
        &reqwest::Url::parse("http://127.0.0.1:8080/mcp").unwrap(),
        true
    )
    .is_ok());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mcp_rule_matching_uses_structured_identity_despite_underscores_and_truncation() {
    let dir = tmpdir("ident");
    let data = dir.join("data");
    let cwd = dir.join("repo");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    let server = "my__srv";
    let raw_tool = "very_long_tool_name_that_exceeds_sixty_four_characters_and_gets_hashed";
    let qualified = qualify_tool_name(server, raw_tool);
    assert!(qualified.len() <= 64);

    rules::save_rule(
        &data,
        &cwd,
        RuleScope::User,
        Effect::Deny,
        &format!("Mcp({server}:{raw_tool})"),
    )
    .unwrap();

    let eng = PermissionEngine::new(
        data,
        cwd,
        "mcp-id".into(),
        "local".into(),
        PermissionMode::Auto,
    );
    let dec = eng.evaluate_with_mcp(&qualified, &json!({}), Some((server, raw_tool)));
    assert_eq!(dec.effect, Effect::Deny, "{dec:?}");
    let mgr = McpManager::empty();
    assert!(mgr.tool_identity(&qualified).is_none());

    let _ = std::fs::remove_dir_all(&dir);
}
