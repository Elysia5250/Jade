//! Phase 4: a bad wire header or `Call.version` answers `Reply::VersionMismatch`.

use envoy_core::{AgentConfig, Caller};
use envoy_proto::codec::{read_framed, write_framed};
use envoy_proto::{Call, ClientMsg, Op, Reply, WIRE_MAGIC, WIRE_VERSION};
use envoy_service::{AgentServer, StaticHost};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, DuplexStream};

fn server() -> AgentServer {
    AgentServer::new(
        std::env::temp_dir().join(format!("es-wv-{}", std::process::id())),
        Arc::new(StaticHost::new(AgentConfig {
            enabled: true,
            allow: Vec::new(),
            max_jobs: 2,
        })),
    )
}

fn serve(server: &AgentServer) -> DuplexStream {
    let (client, peer) = tokio::io::duplex(64 * 1024);
    let srv = server.clone();
    tokio::spawn(async move {
        let _ = srv.serve_stream(Caller::Local, peer).await;
    });
    client
}

#[tokio::test]
async fn wire_header_version_mismatch_replies_with_both_versions() {
    let mut client = serve(&server());
    let mut payload = Vec::new();
    payload.extend_from_slice(&WIRE_MAGIC);
    payload.extend_from_slice(&99u16.to_le_bytes());
    client
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .unwrap();
    client.write_all(&payload).await.unwrap();
    let reply: Reply = read_framed(&mut client).await.unwrap();
    match reply {
        Reply::VersionMismatch {
            server_version,
            client_version,
        } => {
            assert_eq!(server_version, WIRE_VERSION);
            assert_eq!(client_version, 99);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn call_version_mismatch_replies_with_version_mismatch() {
    let mut client = serve(&server());
    write_framed(
        &mut client,
        &ClientMsg::Call(Call {
            version: 1,
            op: Op::Probe,
        }),
    )
    .await
    .unwrap();
    let reply: Reply = read_framed(&mut client).await.unwrap();
    match reply {
        Reply::VersionMismatch {
            server_version,
            client_version,
        } => {
            assert_eq!(server_version, WIRE_VERSION);
            assert_eq!(client_version, 1);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}
