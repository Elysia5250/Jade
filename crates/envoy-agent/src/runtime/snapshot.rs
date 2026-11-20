//! Chunked session replay. One `Snapshot` frame used to exceed the 8 MiB cap.

use super::live::Live;
use super::protocol::{AgentResp, SeqEvent};
use super::server::Runtime;
use crate::agent::AgentEvent;
use crate::session::ChatMessage;
use anyhow::Result;
use envoy_proto::codec::write_framed;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;
use tracing::debug;

/// Soft ceiling for one snapshot frame. The hard frame cap stays 8 MiB.
pub(super) const SNAPSHOT_CHUNK_BYTES: usize = 512 * 1024;

pub(super) struct SnapshotParts {
    pub running: bool,
    pub seq: u64,
    pub incarnation_id: u64,
    pub events: Vec<SeqEvent>,
    pub messages: Vec<ChatMessage>,
}

/// `force` means the caller must see a `SessionReset` before the retained tail.
pub(super) fn replay_window(
    from: u64,
    oldest: u64,
    seq: u64,
    reset_incarnation: bool,
) -> (u64, bool) {
    let gap = from != 0 && oldest > from;
    let ahead = from > seq;
    let force = reset_incarnation || gap || ahead;
    (if force { 0 } else { from }, force)
}

pub(super) fn prepare_snapshot(
    runtime: &Runtime,
    live: &Arc<Mutex<Live>>,
    from: u64,
    reset_incarnation: bool,
) -> (SnapshotParts, mpsc::Receiver<SeqEvent>) {
    let mut state = live.lock().unwrap();
    let oldest = state.events.first().map(|event| event.seq).unwrap_or(0);
    let (cursor, force) = replay_window(from, oldest, state.seq, reset_incarnation);
    debug!(from, cursor, seq = state.seq, force, "attach");
    let active = state.monitors.active_list();
    if !active.is_empty() {
        runtime.publish(&mut state, AgentEvent::MonitorsSync { monitors: active });
    }
    let mut events: Vec<SeqEvent> = state
        .events
        .iter()
        .filter(|event| event.seq > cursor)
        .cloned()
        .collect();
    if force
        && !events
            .iter()
            .any(|event| matches!(event.event, AgentEvent::SessionReset))
    {
        events.insert(
            0,
            SeqEvent {
                seq: 0,
                event: AgentEvent::SessionReset,
            },
        );
    }
    let parts = SnapshotParts {
        running: state.running,
        seq: state.seq,
        incarnation_id: runtime.incarnation_id(),
        events,
        messages: state.messages.clone(),
    };
    let rx = state.subscribe();
    (parts, rx)
}

pub(super) async fn stream_session<W>(
    runtime: &Runtime,
    live: Arc<Mutex<Live>>,
    id: String,
    from: u64,
    reset_incarnation: bool,
    wr: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let (parts, mut rx) = prepare_snapshot(runtime, &live, from, reset_incarnation);
    write_snapshot(wr, &id, parts).await?;
    while let Some(event) = rx.recv().await {
        if write_framed(wr, &AgentResp::Event(event)).await.is_err() {
            break;
        }
    }
    Ok(())
}

pub(super) async fn write_snapshot<W>(wr: &mut W, id: &str, parts: SnapshotParts) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let SnapshotParts {
        running,
        seq,
        incarnation_id,
        events,
        messages,
    } = parts;
    let total_items = u32::try_from(events.len()).unwrap_or(u32::MAX);
    write_framed(
        wr,
        &AgentResp::SnapshotStart {
            session_id: id.to_string(),
            running,
            seq,
            total_items,
            incarnation_id,
        },
    )
    .await?;
    let message_chunks = chunk_items(messages);
    if message_chunks.is_empty() {
        write_framed(
            wr,
            &AgentResp::SnapshotMessages {
                messages: Vec::new(),
            },
        )
        .await?;
    } else {
        for messages in message_chunks {
            write_framed(wr, &AgentResp::SnapshotMessages { messages }).await?;
        }
    }
    for events in chunk_items(events) {
        write_framed(wr, &AgentResp::SnapshotChunk { events }).await?;
    }
    write_framed(wr, &AgentResp::SnapshotEnd).await?;
    Ok(())
}

fn chunk_items<T: Serialize>(items: Vec<T>) -> Vec<Vec<T>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut size = 0usize;
    for item in items {
        let bytes = envoy_proto::codec::payload_len(&item);
        if !current.is_empty() && size.saturating_add(bytes) > SNAPSHOT_CHUNK_BYTES {
            chunks.push(std::mem::take(&mut current));
            size = 0;
        }
        size = size.saturating_add(bytes);
        current.push(item);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}
