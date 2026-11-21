//! Wire protocol for the daemon-side agent control socket (`agent.sock`).
//!
//! All types travel via postcard framing (`jade_proto::codec`), so wire fields
//! must NEVER use `skip_serializing_if`.

use crate::agent::checkpoint::CheckpointSummary;
use crate::agent::AgentEvent;
use crate::permissions::PermissionMode;
use crate::session::{ChatMessage, ImageAttachment};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Agent control protocol. Bumped with the job-socket [`jade_proto::AGENT_VERSION`]
/// when the framed handshake or these enums change incompatibly.
pub const AGENT_PROTOCOL_VERSION: u32 = 8;

const _: () = assert!(AGENT_PROTOCOL_VERSION as u8 == jade_proto::AGENT_VERSION);

/// First frame on `agent.sock`, both directions, before any [`AgentReq`].
///
/// On the server hello, `incarnation_id` is this daemon process. On the client
/// hello it is the last incarnation the console observed, or `0` when the
/// console has never attached. A non-zero mismatch forces [`crate::agent::AgentEvent::SessionReset`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHello {
    pub protocol_version: u32,
    pub incarnation_id: u64,
    pub bin_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendMode {
    /// Start a turn if idle, otherwise steer the running one.
    Prompt,
    /// Deliver at the next tool boundary.
    Steer,
    /// Deliver after the current run finishes.
    FollowUp,
    /// Main composer while a worker turn may be running. The daemon opens the
    /// plan sidechat, or starts a normal turn when the session is actually idle.
    Side,
    /// Side composer. Talks only to the side thread.
    SideTalk,
    /// Queue on the side thread. Never touches the worker queue.
    SideQueue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeqEvent {
    pub seq: u64,
    pub event: AgentEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentReq {
    /// Create the session if missing and sync its history.
    Ensure {
        id: String,
        cwd: PathBuf,
        target: String,
        history: Vec<ChatMessage>,
    },
    /// Ensure the session exists, then attach to its event stream. One round
    /// trip so a reconnecting console cannot race the replay.
    Resume {
        id: String,
        cwd: PathBuf,
        target: String,
        history: Vec<ChatMessage>,
        from: u64,
    },
    Send {
        id: String,
        text: String,
        mode: SendMode,
    },
    SendImages {
        id: String,
        text: String,
        images: Vec<ImageAttachment>,
        mode: SendMode,
    },
    Attach {
        id: String,
        from: u64,
    },
    Interrupt {
        id: String,
    },
    Answer {
        id: String,
        ask_id: u64,
        label: String,
    },
    /// Remove the most recently queued follow-up and return it.
    Recall {
        id: String,
    },
    /// Run manual Stage 2 context compaction on an idle session.
    Compact {
        id: String,
        focus: Option<String>,
    },
    /// Summarize an idle session into a clean `/handoff` brief for a new session.
    Handoff {
        id: String,
        focus: Option<String>,
    },
    /// Update active permission mode for this session.
    SetPermissionMode {
        id: String,
        mode: PermissionMode,
    },
    /// Query live daemon-side permission engine status for a session.
    GetPermissions {
        id: String,
    },
    /// Stop an active background monitor in a session.
    StopMonitor {
        id: String,
        monitor_id: u64,
    },
    /// List recorded per-turn checkpoints for a session.
    ListCheckpoints {
        id: String,
    },
    /// Rewind workspace files and conversation state to a checkpoint (or latest if `None`).
    Rewind {
        id: String,
        checkpoint_id: Option<u64>,
    },
    /// Explicitly close an idle session, running `SessionEnd` cleanup hooks.
    CloseSession {
        id: String,
    },
    List,
    /// Point an existing session at a new working directory.
    SetCwd {
        id: String,
        cwd: PathBuf,
    },
    /// Point an existing session at a new execution target.
    SetTarget {
        id: String,
        target: String,
    },
    /// Drop model-visible history so the next turn starts empty.
    Clear {
        id: String,
    },
    /// Probe configured MCP servers from the daemon and return human-readable status lines.
    ProbeMcp {
        cwd: PathBuf,
    },
    /// Refresh models.dev cache if stale and fetch models for the active provider.
    RefreshModels,
    /// The console is showing this prompt and the user is interacting with it,
    /// so the answer clock must not run out underneath them.
    AskTouched {
        id: String,
        ask_id: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentResp {
    /// Opens a replay. `total_items` counts events, not message chunks.
    /// `incarnation_id` is the daemon process the console must send next time.
    SnapshotStart {
        session_id: String,
        running: bool,
        seq: u64,
        total_items: u32,
        incarnation_id: u64,
    },
    /// One slice of the replay log. Each frame stays under about 512 KiB.
    SnapshotChunk {
        events: Vec<SeqEvent>,
    },
    /// Authoritative model-visible history, split the same way as events.
    /// An empty vec still means "the daemon history is empty".
    SnapshotMessages {
        messages: Vec<ChatMessage>,
    },
    /// The replay is complete. Live [`AgentResp::Event`] frames follow.
    SnapshotEnd,
    Event(SeqEvent),
    Sessions(Vec<SessionInfo>),
    /// A queued follow-up returned to the console, or `None` if the queue was
    /// empty.
    Recalled(Option<String>),
    /// Checkpoints recorded for a session.
    Checkpoints(Vec<CheckpointSummary>),
    /// Human-readable MCP server probe lines.
    McpProbe(Vec<String>),
    /// Live models fetched for the active provider.
    ModelsRefreshed {
        provider_id: String,
        models: Vec<crate::agent::context::ListedModel>,
    },
    /// Authoritative daemon-side permission engine state for a session.
    PermissionsStatus {
        mode: PermissionMode,
        sandbox: String,
        workspace_trusted: bool,
        rules: usize,
        extra_rw: usize,
    },
    Ack,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: PathBuf,
    pub target: String,
    pub running: bool,
    pub queued: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_req_compact_and_handoff_round_trip_postcard() {
        for req in [
            AgentReq::Compact {
                id: "s1".into(),
                focus: Some("preserve test failures".into()),
            },
            AgentReq::Compact {
                id: "s2".into(),
                focus: None,
            },
            AgentReq::Handoff {
                id: "s3".into(),
                focus: Some("switch to auth cleanup".into()),
            },
            AgentReq::SetPermissionMode {
                id: "s4".into(),
                mode: PermissionMode::Plan,
            },
            AgentReq::StopMonitor {
                id: "s5".into(),
                monitor_id: 7,
            },
            AgentReq::ListCheckpoints { id: "s6".into() },
            AgentReq::Rewind {
                id: "s7".into(),
                checkpoint_id: Some(3),
            },
            AgentReq::CloseSession { id: "s8".into() },
        ] {
            let bytes = postcard::to_allocvec(&req).unwrap();
            let decoded: AgentReq = postcard::from_bytes(&bytes).unwrap();
            match (&req, &decoded) {
                (
                    AgentReq::Compact { id: a, focus: fa },
                    AgentReq::Compact { id: b, focus: fb },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(fa, fb);
                }
                (
                    AgentReq::Handoff { id: a, focus: fa },
                    AgentReq::Handoff { id: b, focus: fb },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(fa, fb);
                }
                (
                    AgentReq::SetPermissionMode { id: a, mode: ma },
                    AgentReq::SetPermissionMode { id: b, mode: mb },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(ma, mb);
                }
                (
                    AgentReq::StopMonitor {
                        id: a,
                        monitor_id: ma,
                    },
                    AgentReq::StopMonitor {
                        id: b,
                        monitor_id: mb,
                    },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(ma, mb);
                }
                (AgentReq::ListCheckpoints { id: a }, AgentReq::ListCheckpoints { id: b }) => {
                    assert_eq!(a, b);
                }
                (
                    AgentReq::Rewind {
                        id: a,
                        checkpoint_id: ca,
                    },
                    AgentReq::Rewind {
                        id: b,
                        checkpoint_id: cb,
                    },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(ca, cb);
                }
                (AgentReq::CloseSession { id: a }, AgentReq::CloseSession { id: b }) => {
                    assert_eq!(a, b);
                }
                _ => panic!("mismatched variant"),
            }
        }

        let resp = AgentResp::Checkpoints(vec![CheckpointSummary {
            id: 2,
            label: "edit main".into(),
            ts_ms: 123456,
            remaining_user_turns: 1,
            files_touched: 3,
        }]);
        let bytes = postcard::to_allocvec(&resp).unwrap();
        let decoded: AgentResp = postcard::from_bytes(&bytes).unwrap();
        match decoded {
            AgentResp::Checkpoints(list) => assert_eq!(list.len(), 1),
            other => panic!("unexpected resp: {other:?}"),
        }
    }

    #[test]
    fn agent_req_and_resp_mcp_and_models_round_trip_postcard() {
        for req in [
            AgentReq::ProbeMcp {
                cwd: PathBuf::from("/tmp/work"),
            },
            AgentReq::RefreshModels,
        ] {
            assert_postcard(&req);
        }
        assert_postcard(&AgentResp::McpProbe(vec![
            "srv [project - stdio] connected (8 tools)".into(),
        ]));
        assert_postcard(&AgentResp::ModelsRefreshed {
            provider_id: "openai".into(),
            models: vec![crate::agent::context::ListedModel {
                id: "gpt-5.4".into(),
                route: Some(crate::auth::ModelRoute {
                    protocol: crate::auth::ProviderProtocol::OpenAiChat,
                    base_url: "https://example.com/v1".into(),
                }),
            }],
        });
    }

    fn assert_postcard<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    {
        let bytes = postcard::to_allocvec(value).unwrap();
        let (decoded, rest): (T, &[u8]) = postcard::take_from_bytes(&bytes).unwrap();
        assert!(rest.is_empty(), "trailing {}", rest.len());
        assert_eq!(format!("{value:?}"), format!("{decoded:?}"));
        assert_eq!(bytes, postcard::to_allocvec(&decoded).unwrap());
    }

    fn wire_message(content: &str) -> crate::session::ChatMessage {
        crate::session::ChatMessage {
            role: crate::session::ChatRole::Assistant,
            content: content.into(),
            reasoning: Some("because".into()),
            tool_calls: vec![crate::session::ToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: "{\"path\":\"a.rs\"}".into(),
            }],
            tool_call_id: Some("call-1".into()),
            tool_name: Some("read".into()),
            reasoning_blocks: vec![crate::session::ReasoningBlock::Thinking {
                text: "step".into(),
                signature: "sig".into(),
            }],
            images: vec![ImageAttachment {
                mime: "image/png".into(),
                data_base64: "iVBORw0KGgo=".into(),
                name: Some("wire.png".into()),
            }],
        }
    }

    fn sample_reqs_open() -> Vec<AgentReq> {
        let history = vec![wire_message("prior")];
        vec![
            AgentReq::Ensure {
                id: "s-ensure".into(),
                cwd: PathBuf::from("/work/ensure"),
                target: "local".into(),
                history: history.clone(),
            },
            AgentReq::Resume {
                id: "s-resume".into(),
                cwd: PathBuf::from("/work/resume"),
                target: "peer-1".into(),
                history,
                from: 9,
            },
            AgentReq::Send {
                id: "s-send".into(),
                text: "do the thing".into(),
                mode: SendMode::Steer,
            },
            AgentReq::SendImages {
                id: "s-send-img".into(),
                text: "look".into(),
                images: vec![ImageAttachment {
                    mime: "image/png".into(),
                    data_base64: "iVBORw0KGgo=".into(),
                    name: Some("clip.png".into()),
                }],
                mode: SendMode::Prompt,
            },
            AgentReq::Attach {
                id: "s-attach".into(),
                from: 4,
            },
            AgentReq::Interrupt { id: "s-int".into() },
            AgentReq::Answer {
                id: "s-ans".into(),
                ask_id: 11,
                label: "allow".into(),
            },
            AgentReq::AskTouched {
                id: "s-touch".into(),
                ask_id: 11,
            },
            AgentReq::Recall {
                id: "s-recall".into(),
            },
            AgentReq::Compact {
                id: "s-compact".into(),
                focus: Some("keep failures".into()),
            },
            AgentReq::Handoff {
                id: "s-handoff".into(),
                focus: None,
            },
        ]
    }

    fn sample_reqs_edit() -> Vec<AgentReq> {
        vec![
            AgentReq::SetPermissionMode {
                id: "s-mode".into(),
                mode: PermissionMode::Plan,
            },
            AgentReq::GetPermissions {
                id: "s-perm".into(),
            },
            AgentReq::StopMonitor {
                id: "s-stop".into(),
                monitor_id: 8,
            },
            AgentReq::ListCheckpoints {
                id: "s-list-cp".into(),
            },
            AgentReq::Rewind {
                id: "s-rewind".into(),
                checkpoint_id: Some(3),
            },
            AgentReq::CloseSession {
                id: "s-close".into(),
            },
            AgentReq::List,
            AgentReq::SetCwd {
                id: "s-cwd".into(),
                cwd: PathBuf::from("/work/next"),
            },
            AgentReq::SetTarget {
                id: "s-target".into(),
                target: "peer-9".into(),
            },
            AgentReq::Clear {
                id: "s-clear".into(),
            },
            AgentReq::ProbeMcp {
                cwd: PathBuf::from("/work/mcp"),
            },
            AgentReq::RefreshModels,
        ]
    }

    fn req_index(req: &AgentReq) -> usize {
        match req {
            AgentReq::Ensure { .. } => 0,
            AgentReq::Resume { .. } => 1,
            AgentReq::Send { .. } => 2,
            AgentReq::Attach { .. } => 3,
            AgentReq::Interrupt { .. } => 4,
            AgentReq::Answer { .. } => 5,
            AgentReq::Recall { .. } => 6,
            AgentReq::Compact { .. } => 7,
            AgentReq::Handoff { .. } => 8,
            AgentReq::SetPermissionMode { .. } => 9,
            AgentReq::StopMonitor { .. } => 10,
            AgentReq::ListCheckpoints { .. } => 11,
            AgentReq::Rewind { .. } => 12,
            AgentReq::CloseSession { .. } => 13,
            AgentReq::List => 14,
            AgentReq::SetCwd { .. } => 15,
            AgentReq::SetTarget { .. } => 16,
            AgentReq::Clear { .. } => 17,
            AgentReq::ProbeMcp { .. } => 18,
            AgentReq::RefreshModels => 19,
            AgentReq::AskTouched { .. } => 20,
            AgentReq::SendImages { .. } => 21,
            AgentReq::GetPermissions { .. } => 22,
        }
    }

    fn sample_resps() -> Vec<AgentResp> {
        let event = SeqEvent {
            seq: 6,
            event: AgentEvent::TextDelta("delta".into()),
        };
        vec![
            AgentResp::SnapshotStart {
                session_id: "s1".into(),
                running: true,
                seq: 12,
                total_items: 3,
                incarnation_id: 99,
            },
            AgentResp::SnapshotChunk {
                events: vec![event.clone()],
            },
            AgentResp::SnapshotMessages {
                messages: vec![wire_message("visible")],
            },
            AgentResp::SnapshotEnd,
            AgentResp::Event(event),
            AgentResp::Sessions(vec![SessionInfo {
                id: "s1".into(),
                cwd: PathBuf::from("/work"),
                target: "peer-9".into(),
                running: true,
                queued: 4,
            }]),
            AgentResp::Recalled(Some("queued follow-up".into())),
            AgentResp::Checkpoints(vec![CheckpointSummary {
                id: 2,
                label: "edit main".into(),
                ts_ms: 123_456,
                remaining_user_turns: 1,
                files_touched: 3,
            }]),
            AgentResp::McpProbe(vec!["srv ok".into()]),
            AgentResp::ModelsRefreshed {
                provider_id: "openai".into(),
                models: Vec::new(),
            },
            AgentResp::PermissionsStatus {
                mode: PermissionMode::AcceptEdits,
                sandbox: "bwrap".into(),
                workspace_trusted: true,
                rules: 3,
                extra_rw: 1,
            },
            AgentResp::Ack,
            AgentResp::Error("no such session: s9".into()),
        ]
    }

    fn resp_index(resp: &AgentResp) -> usize {
        match resp {
            AgentResp::SnapshotStart { .. } => 0,
            AgentResp::SnapshotChunk { .. } => 1,
            AgentResp::SnapshotMessages { .. } => 2,
            AgentResp::SnapshotEnd => 3,
            AgentResp::Event(_) => 4,
            AgentResp::Sessions(_) => 5,
            AgentResp::Recalled(_) => 6,
            AgentResp::Checkpoints(_) => 7,
            AgentResp::McpProbe(_) => 8,
            AgentResp::ModelsRefreshed { .. } => 9,
            AgentResp::Ack => 10,
            AgentResp::Error(_) => 11,
            AgentResp::PermissionsStatus { .. } => 12,
        }
    }

    fn assert_each<T>(samples: Vec<T>, index: fn(&T) -> usize, count: usize)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    {
        let mut seen = vec![false; count];
        for sample in &samples {
            let slot = index(sample);
            assert!(slot < count && !seen[slot], "duplicate or unknown {slot}");
            seen[slot] = true;
            assert_postcard(sample);
        }
        assert!(seen.iter().all(|hit| *hit), "missing wire variant");
    }

    #[test]
    fn every_agent_req_and_resp_round_trips_postcard() {
        let mut reqs = sample_reqs_open();
        reqs.extend(sample_reqs_edit());
        assert_each(reqs, req_index, 23);
        assert_each(sample_resps(), resp_index, 13);
        assert_postcard(&AgentHello {
            protocol_version: AGENT_PROTOCOL_VERSION,
            incarnation_id: 7,
            bin_version: "0.1.0".into(),
        });
    }

    #[test]
    fn side_send_modes_round_trip_postcard() {
        for mode in [SendMode::Side, SendMode::SideTalk, SendMode::SideQueue] {
            assert_postcard(&AgentReq::Send {
                id: "s-side".into(),
                text: "plan this".into(),
                mode,
            });
        }
    }
}
