//! Phase 1 regression tests for remote `AgentServer` caller gating and security floor.

use envoy_core::{AgentConfig, Caller};
use envoy_proto::{SandboxPolicy, SubmitRequest, TaskSpec, DEFAULT_GROUP};
use envoy_service::{client, AgentServer, StaticHost};
use std::path::PathBuf;
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = PathBuf::from("/tmp").join(format!("es-rp-{tag}-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

async fn serve_caller(server: &AgentServer, caller: Caller) -> tokio::io::DuplexStream {
    let (client, peer) = tokio::io::duplex(256 * 1024);
    let srv = server.clone();
    tokio::spawn(async move { srv.serve_stream(caller, peer).await });
    client
}

#[tokio::test]
async fn remote_caller_named_local_cannot_bypass_gate() {
    let dir = tmpdir("local-spoof");
    let server = AgentServer::new(
        dir.clone(),
        Arc::new(StaticHost::new(AgentConfig {
            enabled: true,
            allow: vec!["real-peer".into()],
            max_jobs: 4,
        })),
    );
    let peer = serve_caller(&server, Caller::Remote("local".into())).await;
    let err = client::probe(peer, "local")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("refused") || err.contains("allowlisted"), "{err}");

    let disabled = AgentServer::new(dir.clone(), Arc::new(StaticHost::default()));
    let peer = serve_caller(&disabled, Caller::Remote("local".into())).await;
    let err = client::probe(peer, "local")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("refused") || err.contains("disabled"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn remote_caller_cannot_read_or_write_protected_paths() {
    let dir = tmpdir("protected");
    let work = dir.join("work");
    std::fs::create_dir_all(work.join(".git")).unwrap();
    std::fs::write(work.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(work.join(".git/config"), "[core]\n").unwrap();
    std::fs::write(dir.join("auth.json"), "{\"k\":\"v\"}").unwrap();

    let server = AgentServer::new(
        dir.clone(),
        Arc::new(StaticHost::new(AgentConfig {
            enabled: true,
            allow: vec!["peer-a".into()],
            max_jobs: 4,
        })),
    );

    for bad in [
        work.join(".env"),
        work.join(".git/config"),
        dir.join("auth.json"),
        PathBuf::from(".ssh/id_rsa"),
    ] {
        let stream = serve_caller(&server, Caller::Remote("peer-a".into())).await;
        let err = client::read(stream, "peer-a", bad.to_str().unwrap(), 0, 128)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("denied") || err.contains("refused"), "{bad:?}: {err}");
    }

    let stream = serve_caller(&server, Caller::Remote("peer-a".into())).await;
    let err = client::write(
        stream,
        "peer-a",
        work.join(".env").to_str().unwrap(),
        b"X=1",
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("denied") || err.contains("refused"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn remote_submit_clamps_sandbox_off_and_write_defaults_to_0600() {
    let dir = tmpdir("clamp");
    let work = dir.join("work");
    let outside = dir.join("outside.txt");
    std::fs::create_dir_all(&work).unwrap();

    let server = AgentServer::new(
        dir.clone(),
        Arc::new(StaticHost::new(AgentConfig {
            enabled: true,
            allow: vec!["peer-a".into()],
            max_jobs: 4,
        })),
    );

    // 1. Default write mode must be 0o600 on Unix.
    let target = work.join("secret.txt");
    let stream = serve_caller(&server, Caller::Remote("peer-a".into())).await;
    client::write(
        stream,
        "peer-a",
        target.to_str().unwrap(),
        b"hello",
        None,
    )
    .await
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }

    // 2. Submitting SandboxPolicy::Off must be clamped to a workspace sandbox.
    if envoy_core::sandbox::capabilities().enforces_workspace_boundary() {
        let req = SubmitRequest {
            spec: TaskSpec::Shell {
                command: format!("touch '{}' 2>/dev/null; echo ok", outside.display()),
            },
            cwd: Some(work.display().to_string()),
            env: Vec::new(),
            label: None,
            group: DEFAULT_GROUP.to_string(),
            timeout_s: 15,
            sandbox: SandboxPolicy::Off,
        };
        let stream = serve_caller(&server, Caller::Remote("peer-a".into())).await;
        let job = client::submit(stream, "peer-a", req).await.unwrap();
        let stream = serve_caller(&server, Caller::Remote("peer-a".into())).await;
        let (_c, finished) = client::logs(stream, "peer-a", job.id, 0, true, |_| {})
            .await
            .unwrap();
        assert_eq!(finished, Some(0));
        assert!(!outside.exists(), "clamped remote job escaped workspace");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
