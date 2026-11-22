//! Integration tests against a mock `topaz run` control socket.
//!
//! The real thing is not implemented on the topaz side yet, so this pins the
//! contract from `docs/TOPAZ-INTEGRATION.md`: HTTP/1.1 + WebSocket over a Unix
//! socket, `/v1/services/16/open/{target}`, `/v1/services/16/accept?name=jade`,
//! the text handoff frame, and JSON errors.

use futures::SinkExt;
use jade_core::AgentConfig;
use jade_proto::codec::{read_framed, write_framed};
use jade_proto::{ClientMsg, Op, Reply, SandboxPolicy, SubmitRequest, TaskSpec, DEFAULT_GROUP};
use jade_service::{
    spawn_accept_pool, AgentServer, Remote, StaticHost, TopazClient, TopazDialer, WsByteStream,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

fn tmpdir(tag: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Keep socket paths well under macOS SUN_LEN (104 bytes).
    let p = PathBuf::from("/tmp").join(format!("ec-{tag}-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn host(enabled: bool, allow: &[&str]) -> Arc<StaticHost> {
    Arc::new(StaticHost::new(AgentConfig {
        enabled,
        allow: allow.iter().map(|s| s.to_string()).collect(),
        max_jobs: 2,
    }))
}

fn shell(command: &str) -> SubmitRequest {
    SubmitRequest {
        spec: TaskSpec::Shell {
            command: command.into(),
        },
        cwd: None,
        env: Vec::new(),
        label: None,
        group: DEFAULT_GROUP.into(),
        timeout_s: 30,
        sandbox: SandboxPolicy::Off,
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn read_request(stream: &mut UnixStream) -> (String, HashMap<String, String>) {
    let mut head = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        if let Some(end) = find(&head, b"\r\n\r\n") {
            let text = String::from_utf8_lossy(&head[..end]).to_string();
            let mut lines = text.split("\r\n");
            let path = lines
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            let headers = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                .collect();
            return (path, headers);
        }
        let n = stream.read(&mut buf).await.expect("read request");
        assert!(n > 0, "client closed before finishing the request");
        head.extend_from_slice(&buf[..n]);
    }
}

async fn upgrade(
    mut stream: UnixStream,
    headers: &HashMap<String, String>,
) -> WebSocketStream<UnixStream> {
    let key = headers
        .get("sec-websocket-key")
        .expect("missing Sec-WebSocket-Key");
    let accept = derive_accept_key(key.as_bytes());
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    WebSocketStream::from_raw_socket(stream, Role::Server, None).await
}

async fn respond(mut stream: UnixStream, status: u16, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}

#[tokio::test]
async fn dials_and_runs_a_job_through_the_control_socket() {
    let dir = tmpdir("open");
    let socket = dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = AgentServer::new(dir.clone(), host(true, &[]));

    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let (path, headers) = read_request(&mut stream).await;
            if !path.starts_with("/v1/services/16/open/") {
                respond(stream, 404, r#"{"error":"not here"}"#).await;
                continue;
            }
            let ws = WsByteStream::new(upgrade(stream, &headers).await);
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.serve_stream(jade_service::Caller::Local, ws).await;
            });
        }
    });

    let remote = Remote::new(TopazDialer::new(socket.clone()));
    let job = remote
        .submit("laptop", shell("echo through-topaz"))
        .await
        .expect("submit over the control socket");
    assert!(job.id > 0);

    let mut text = String::new();
    let (_cursor, finished) = remote
        .logs("laptop", job.id, 0, true, |records| {
            for record in records {
                text.push_str(&String::from_utf8_lossy(&record.data));
            }
        })
        .await
        .expect("follow the log");
    assert_eq!(finished, Some(0));
    assert!(text.contains("through-topaz"), "{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn accept_pool_serves_the_handed_off_peer_and_trusts_its_identity() {
    let dir = tmpdir("accept");
    let socket = dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // Only the topaz-authenticated device may run anything here.
    let server = AgentServer::with_device(
        dir.clone(),
        host(true, &["dev-remote"]),
        Some("dev-local".into()),
    );

    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("one accept connection");
        let (path, headers) = read_request(&mut stream).await;
        assert!(path.starts_with("/v1/services/16/accept"), "{path}");
        let mut ws = upgrade(stream, &headers).await;

        // 1. topaz hands off the authenticated caller.
        ws.send(Message::text(
            r#"{"event":"accepted","service_id":16,"peer_device_id":"dev-remote","peer_name":"workstation","connection":"direct"}"#,
        ))
        .await
        .unwrap();
        let mut ws = WsByteStream::new(ws);

        // 2. from here on the connection is a transparent agent stream.
        write_framed(
            &mut ws,
            &ClientMsg::Call(jade_proto::Call {
                version: jade_proto::AGENT_VERSION,
                op: Op::Probe,
            }),
        )
        .await
        .unwrap();
        let hello: Reply = read_framed(&mut ws).await.unwrap();
        if let Reply::Hello(hello) = hello {
            tx.send(hello.device.unwrap_or_default()).unwrap();
        }
    });

    let pool = spawn_accept_pool(server, TopazClient::new(socket.clone()), 1);
    let device = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("the pool should hand off within 5s")
        .expect("hello");
    assert_eq!(
        device, "dev-local",
        "the handshake should report the answering device"
    );
    pool.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_non_allowlisted_handoff_is_refused() {
    let dir = tmpdir("deny");
    let socket = dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = AgentServer::new(dir.clone(), host(true, &["someone-else"]));

    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("one accept connection");
        let (_path, headers) = read_request(&mut stream).await;
        let mut ws = upgrade(stream, &headers).await;
        ws.send(Message::text(
            r#"{"event":"accepted","peer_device_id":"intruder"}"#,
        ))
        .await
        .unwrap();
        let mut ws = WsByteStream::new(ws);
        write_framed(
            &mut ws,
            &ClientMsg::Call(jade_proto::Call {
                version: jade_proto::AGENT_VERSION,
                op: Op::Probe,
            }),
        )
        .await
        .unwrap();
        let reply: Reply = read_framed(&mut ws).await.unwrap();
        tx.send(matches!(reply, Reply::Denied { .. })).unwrap();
    });

    let pool = spawn_accept_pool(server, TopazClient::new(socket.clone()), 1);
    let denied = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("handoff within 5s")
        .expect("reply");
    assert!(denied, "an unlisted device must be refused");
    pool.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn http_errors_carry_the_reason_to_the_caller() {
    let dir = tmpdir("errors");
    let socket = dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let (path, _) = read_request(&mut stream).await;
            if path.contains("/open/ghost") {
                respond(stream, 404, r#"{"error":"ghost is not an active device"}"#).await;
            } else {
                respond(stream, 409, r#"{"error":"two devices are named box"}"#).await;
            }
        }
    });

    let err = Remote::new(TopazDialer::new(socket.clone()))
        .probe("ghost")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not an active device"), "{err}");
    assert!(err.contains("404"), "{err}");

    let err = Remote::new(TopazDialer::new(socket.clone()))
        .probe("box")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("two devices"), "{err}");
    assert!(err.contains("409"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn reads_status_and_devices_over_plain_http() {
    let dir = tmpdir("discover");
    let socket = dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let (path, _) = read_request(&mut stream).await;
            match path.as_str() {
                "/v1/devices" => {
                    respond(
                        stream,
                        200,
                        r#"{"account":"acct_1","devices":[
                            {"device_id":"3fa9","name":"workstation","dns_name":"workstation","virtual_ip":"100.64.0.1","status":"active","me":true},
                            {"device_id":"91ff","name":"laptop","dns_name":"laptop","virtual_ip":"100.64.0.2","status":"active","me":false}
                        ]}"#,
                    )
                    .await;
                }
                "/v1/status" => {
                    respond(
                        stream,
                        200,
                        r#"{"account":"acct_1","device_id":"3fa9","device_name":"workstation",
                            "dns_name":"workstation","virtual_ip":"100.64.0.1",
                            "tun":{"enabled":true,"interface":"tun0","cidr":"100.64.0.0/10"},
                            "connection":"direct",
                            "peers":[
                              {"device_id":"91ff","name":"laptop","virtual_ip":"100.64.0.2","path":"direct","rtt_ms":14,
                               "services":[{"service_id":16,"name":"jade"}]},
                              {"device_id":"77aa","name":"router","virtual_ip":"100.64.0.3","path":"direct","rtt_ms":3,
                               "services":[]}
                            ],
                            "services":[{"service_id":16,"name":"jade","online":true,"listeners":3}]}"#,
                    )
                    .await;
                }
                _ => respond(stream, 404, r#"{"error":"unknown"}"#).await,
            }
        }
    });

    let client = TopazClient::new(socket.clone());
    let devices = client.devices().await.unwrap();
    assert_eq!(devices.devices.len(), 2);
    assert_eq!(devices.devices[1].name, "laptop");

    let status = client.status().await.unwrap();
    assert!(status.tun.enabled);
    assert_eq!(status.peers.len(), 2);
    assert_eq!(status.services[0].service_id, 16);
    assert!(status.services[0].online);

    // Agent-capable vs pure VPN node is decided from advertised services.
    assert!(status.peers[0].has_service(16));
    assert!(!status.peers[1].has_service(16));
    assert_eq!(status.peers[0].services[0].name, "jade");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn watches_live_status_events() {
    let dir = tmpdir("events");
    let socket = dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("events connection");
        let (path, headers) = read_request(&mut stream).await;
        assert_eq!(path, "/v1/events");
        let mut ws = upgrade(stream, &headers).await;
        ws.send(Message::text(
            r#"{"type":"status","status":{"device_id":"me","device_name":"workstation",
                "tun":{"enabled":false},"peers":[{"device_id":"a","name":"laptop","path":"direct",
                 "services":[{"service_id":16,"name":"jade"}]}],
                "services":[{"service_id":16,"name":"jade","online":true,"listeners":1}]}}"#,
        ))
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        ws.send(Message::text(
            r#"{"type":"status","status":{"device_id":"me","device_name":"workstation",
                "tun":{"enabled":false},"peers":[{"device_id":"a","name":"laptop"},{"device_id":"b","name":"server"}],
                "services":[{"service_id":16,"name":"jade","online":true,"listeners":3}]}}"#,
        ))
        .await
        .unwrap();
        // Keep the socket open until the receiver has seen both frames.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let client = TopazClient::new(socket.clone());
    let rx = client.watch().await.expect("initial snapshot");
    {
        let status = rx.borrow();
        let status = status.as_ref().expect("seeded");
        assert_eq!(status.device_id, "me");
        assert_eq!(status.peers.len(), 1);
        assert_eq!(status.services[0].listeners, 1);
    }

    let mut seen_two = false;
    for _ in 0..100 {
        if let Some(status) = rx.borrow().clone() {
            if status.peers.len() == 2 {
                assert_eq!(status.services[0].listeners, 3);
                seen_two = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(seen_two, "the second push never arrived");
    let _ = std::fs::remove_dir_all(&dir);
}
