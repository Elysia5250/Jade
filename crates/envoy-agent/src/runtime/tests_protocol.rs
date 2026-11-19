//! Phase 4: agent.sock handshake, bounded replay, and session control.

use super::hello::{read_hello, write_hello, AGENT_PROTOCOL_VERSION};
use super::live::{EVENT_BYTE_CAP, TEXT_CAP};
use super::protocol::{AgentHello, AgentReq, AgentResp};
use super::server::Runtime;
use super::{request, serve, socket_path, tell, SendMode};
use crate::agent::{AgentEvent, MonitorInfo};
use crate::session::{ChatMessage, ChatRole};
use envoy_proto::codec::{read_framed, write_framed};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixStream;

fn tmpdir(tag: &str) -> PathBuf {
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    let path = PathBuf::from("/tmp").join(format!(
        "e4-{tag}-{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

struct Boot {
    dir: PathBuf,
    runtime: Arc<Runtime>,
    task: tokio::task::JoinHandle<()>,
}

fn boot(tag: &str) -> Boot {
    let dir = tmpdir(tag);
    let runtime = Runtime::new(dir.clone());
    let listener = tokio::net::UnixListener::bind(socket_path(&dir)).expect("bind agent.sock");
    let served = Arc::clone(&runtime);
    let task = tokio::spawn(async move {
        serve(served, listener).await;
    });
    Boot { dir, runtime, task }
}

impl Drop for Boot {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Snap {
    events: Vec<super::protocol::SeqEvent>,
    messages: Vec<ChatMessage>,
    chunks: usize,
    incarnation: u64,
}

async fn open_session(dir: &Path, incarnation: u64) -> UnixStream {
    let mut stream = UnixStream::connect(socket_path(dir)).await.unwrap();
    write_hello(&mut stream, incarnation).await.unwrap();
    let hello = tokio::time::timeout(Duration::from_secs(5), read_hello(&mut stream))
        .await
        .expect("server hello timed out")
        .unwrap();
    assert_eq!(hello.protocol_version, AGENT_PROTOCOL_VERSION);
    assert_ne!(hello.incarnation_id, 0);
    stream
}

async fn read_snap(stream: &mut UnixStream) -> Snap {
    let mut events = Vec::new();
    let mut messages = Vec::new();
    let mut chunks = 0usize;
    let mut incarnation = 0u64;
    let mut saw_messages = false;
    loop {
        let resp: AgentResp = tokio::time::timeout(Duration::from_secs(5), read_framed(&mut *stream))
            .await
            .expect("snapshot frame timed out")
            .unwrap_or_else(|err| panic!("snapshot frame failed: {err:#}"));
        match resp {
            AgentResp::SnapshotStart { incarnation_id, .. } => incarnation = incarnation_id,
            AgentResp::SnapshotChunk { events: chunk } => {
                chunks += 1;
                events.extend(chunk);
            }
            AgentResp::SnapshotMessages { messages: chunk } => {
                saw_messages = true;
                messages.extend(chunk);
            }
            AgentResp::SnapshotEnd => break,
            AgentResp::Error(message) => panic!("snapshot error: {message}"),
            other => panic!("unexpected snapshot frame: {other:?}"),
        }
    }
    assert!(saw_messages, "snapshot omitted SnapshotMessages");
    Snap {
        events,
        messages,
        chunks,
        incarnation,
    }
}

async fn attach_from(boot: &Boot, id: &str, from: u64, incarnation: u64) -> Snap {
    let mut stream = open_session(&boot.dir, incarnation).await;
    write_framed(
        &mut stream,
        &AgentReq::Attach {
            id: id.to_string(),
            from,
        },
    )
    .await
    .unwrap();
    let snap = read_snap(&mut stream).await;
    drop(stream);
    snap
}

fn history_message(content: &str) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        content: content.into(),
        reasoning: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        reasoning_blocks: Vec::new(),
    }
}

async fn expect_ok(fut: impl std::future::Future<Output = anyhow::Result<()>>) {
    tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .expect("control request timed out")
        .unwrap();
}

#[tokio::test]
async fn agent_hello_protocol_mismatch_answers_after_server_hello() {
    let boot = boot("hello");
    let mut stream = UnixStream::connect(socket_path(&boot.dir)).await.unwrap();
    write_framed(
        &mut stream,
        &AgentHello {
            protocol_version: 99,
            incarnation_id: 0,
            bin_version: "old".into(),
        },
    )
    .await
    .unwrap();
    let hello = tokio::time::timeout(Duration::from_secs(5), read_hello(&mut stream))
        .await
        .expect("server hello timed out")
        .unwrap();
    assert_eq!(hello.protocol_version, AGENT_PROTOCOL_VERSION);
    assert_ne!(hello.incarnation_id, 0);
    let resp: AgentResp = tokio::time::timeout(Duration::from_secs(5), read_framed(&mut stream))
        .await
        .expect("mismatch reply timed out")
        .unwrap();
    match resp {
        AgentResp::Error(message) => {
            assert!(message.contains("99"), "{message}");
            assert!(message.contains("envoy daemon stop"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn incarnation_mismatch_forces_session_reset() {
    let boot = boot("inc");
    let live = boot
        .runtime
        .ensure("s1".into(), boot.dir.clone(), "local".into(), Vec::new());
    {
        let mut state = live.lock().unwrap();
        boot.runtime
            .publish(&mut state, AgentEvent::TextDelta("kept".into()));
    }
    drop(live);
    let server_inc = boot.runtime.incarnation_id();
    let same = attach_from(&boot, "s1", 2, server_inc).await;
    assert_eq!(same.incarnation, server_inc);
    assert!(same.messages.is_empty());
    assert!(
        !same
            .events
            .iter()
            .any(|event| matches!(event.event, AgentEvent::SessionReset)),
        "same incarnation replayed a reset: {:?}",
        same.events
    );

    let other = if server_inc == 1 { 2 } else { 1 };
    let diff = attach_from(&boot, "s1", 2, other).await;
    assert!(
        matches!(
            diff.events.first().map(|event| &event.event),
            Some(AgentEvent::SessionReset)
        ),
        "missing reset: {:?}",
        diff.events
    );
    assert!(diff.events.iter().any(|event| {
        matches!(&event.event, AgentEvent::TextDelta(text) if text == "kept")
    }));
}

#[tokio::test]
async fn oversized_file_changes_replay_in_chunks() {
    let boot = boot("big");
    let live = boot
        .runtime
        .ensure("s1".into(), boot.dir.clone(), "local".into(), Vec::new());
    let big = "x".repeat(256 * 1024);
    let count = 160usize;
    assert!(count * big.len() > 32 * 1024 * 1024);
    {
        let mut state = live.lock().unwrap();
        for n in 0..count {
            boot.runtime.publish(
                &mut state,
                AgentEvent::FileChanged {
                    path: format!("f{n}.txt"),
                    before: String::new(),
                    after: big.clone(),
                    added: 1,
                    removed: 0,
                    existed_before: false,
                    restorable_before: true,
                },
            );
        }
    }
    drop(live);
    let snap = attach_from(&boot, "s1", 0, boot.runtime.incarnation_id()).await;
    assert!(snap.chunks >= 2, "chunks {}", snap.chunks);
    let cap = TEXT_CAP + '…'.len_utf8();
    let mut files = 0usize;
    for event in &snap.events {
        if let AgentEvent::FileChanged { before, after, .. } = &event.event {
            files += 1;
            assert!(before.len() <= cap && after.len() <= cap, "{} {}", before.len(), after.len());
            assert!(after.len() < big.len());
        }
    }
    assert_eq!(files, count);
}

#[tokio::test]
async fn replay_gap_forces_session_reset() {
    let boot = boot("gap");
    let live = boot
        .runtime
        .ensure("s1".into(), boot.dir.clone(), "local".into(), Vec::new());
    let before = "a".repeat(TEXT_CAP);
    let after = "b".repeat(TEXT_CAP);
    {
        let mut state = live.lock().unwrap();
        for n in 0..1_200 {
            boot.runtime.publish(
                &mut state,
                AgentEvent::FileChanged {
                    path: format!("g{n}"),
                    before: before.clone(),
                    after: after.clone(),
                    added: 2,
                    removed: 1,
                    existed_before: true,
                    restorable_before: true,
                },
            );
        }
        assert!(state.event_bytes <= EVENT_BYTE_CAP);
        assert!(state.events.first().expect("ring empty").seq > 1);
    }
    drop(live);
    let snap = attach_from(&boot, "s1", 1, boot.runtime.incarnation_id()).await;
    assert!(
        matches!(
            snap.events.first().map(|event| &event.event),
            Some(AgentEvent::SessionReset)
        ),
        "gap replay: {:?}",
        snap.events.first()
    );
}

#[tokio::test]
async fn monitors_sync_seqs_are_strictly_increasing() {
    let boot = boot("mon");
    let live = boot
        .runtime
        .ensure("s1".into(), boot.dir.clone(), "local".into(), Vec::new());
    live.lock().unwrap().monitors.seed_active_for_test(MonitorInfo {
        id: 7,
        job_id: 9,
        label: "watch".into(),
        pattern: "err".into(),
        command: "true".into(),
        trigger_count: 1,
        max_triggers: Some(3),
        owns_job: false,
        active: true,
    });
    drop(live);
    let inc = boot.runtime.incarnation_id();
    let _first = attach_from(&boot, "s1", 0, inc).await;
    let second = attach_from(&boot, "s1", 0, inc).await;
    let seqs: Vec<u64> = second
        .events
        .iter()
        .filter_map(|event| match &event.event {
            AgentEvent::MonitorsSync { .. } => Some(event.seq),
            _ => None,
        })
        .collect();
    assert!(seqs.len() >= 2, "{seqs:?}");
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
}

async fn ack_req(dir: &Path, req: AgentReq) {
    let resp = tokio::time::timeout(Duration::from_secs(5), request(dir, &req))
        .await
        .expect("request timed out")
        .unwrap();
    assert!(matches!(resp, AgentResp::Ack), "{resp:?}");
}

async fn expect_missing(dir: &Path, req: AgentReq) {
    let err = tokio::time::timeout(Duration::from_secs(5), tell(dir, req))
        .await
        .expect("tell timed out")
        .unwrap_err();
    assert!(err.to_string().contains("no such session"), "{err:#}");
}

fn assert_next(boot: &Boot, cwd: &Path, target: &str, content: Option<&str>) {
    let live = boot.runtime.session("s1").expect("session missing");
    let state = live.lock().unwrap();
    let (got_cwd, got_target, messages) = state.next_turn_context();
    assert_eq!(got_cwd, cwd);
    assert_eq!(got_target, target);
    assert_eq!(state.permissions.cwd(), cwd);
    match content {
        Some(text) => {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].content, text);
        }
        None => assert!(messages.is_empty(), "history refilled: {messages:?}"),
    }
}

#[tokio::test]
async fn set_cwd_target_and_clear_drive_the_next_turn() {
    let boot = boot("ctl");
    let work = boot.dir.join("work");
    let elsewhere = boot.dir.join("elsewhere");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    ack_req(
        &boot.dir,
        AgentReq::Ensure {
            id: "s1".into(),
            cwd: boot.dir.clone(),
            target: "local".into(),
            history: vec![history_message("keep-me")],
        },
    )
    .await;
    expect_ok(tell(
        &boot.dir,
        AgentReq::SetCwd {
            id: "s1".into(),
            cwd: work.clone(),
        },
    ))
    .await;
    expect_ok(tell(
        &boot.dir,
        AgentReq::SetTarget {
            id: "s1".into(),
            target: "peer-9".into(),
        },
    ))
    .await;
    assert_next(&boot, &work, "peer-9", Some("keep-me"));

    expect_ok(tell(&boot.dir, AgentReq::Clear { id: "s1".into() })).await;
    {
        let live = boot.runtime.session("s1").unwrap();
        let state = live.lock().unwrap();
        assert!(state.queued.is_empty());
        assert!(state.steer.lock().unwrap().is_empty());
        assert!(matches!(
            state.events.last().map(|event| &event.event),
            Some(AgentEvent::SessionReset)
        ));
    }
    assert_next(&boot, &work, "peer-9", None);

    ack_req(
        &boot.dir,
        AgentReq::Ensure {
            id: "s1".into(),
            cwd: elsewhere.clone(),
            target: "peer-2".into(),
            history: vec![history_message("should-not-land")],
        },
    )
    .await;
    assert_next(&boot, &elsewhere, "peer-2", None);
    expect_missing(&boot.dir, AgentReq::Interrupt { id: "missing".into() }).await;
    expect_missing(
        &boot.dir,
        AgentReq::Send {
            id: "nope".into(),
            text: "x".into(),
            mode: SendMode::Prompt,
        },
    )
    .await;
}

#[test]
fn agent_protocol_version_matches_the_job_socket() {
    assert_eq!(AGENT_PROTOCOL_VERSION, 4);
    assert_eq!(
        AGENT_PROTOCOL_VERSION,
        u32::from(envoy_proto::AGENT_VERSION)
    );
    assert_eq!(
        envoy_proto::WIRE_VERSION,
        u16::from(envoy_proto::AGENT_VERSION)
    );
}
