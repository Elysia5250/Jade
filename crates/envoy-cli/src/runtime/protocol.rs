//! Wire protocol for the daemon-side agent control socket (`agent.sock`).
//!
//! All types travel via postcard framing (`envoy_proto::codec`), so wire fields
//! must NEVER use `skip_serializing_if`.

use crate::agent::AgentEvent;
use crate::permissions::PermissionMode;
use crate::session::ChatMessage;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendMode {
    /// Start a turn if idle, otherwise steer the running one.
    Prompt,
    /// Deliver at the next tool boundary.
    Steer,
    /// Deliver after the current run finishes.
    FollowUp,
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
    List,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentResp {
    /// Replay payload for an attach: everything after `from`, plus whether a
    /// turn is currently live in the daemon.
    Snapshot { events: Vec<SeqEvent>, running: bool },
    Event(SeqEvent),
    Sessions(Vec<SessionInfo>),
    /// A queued follow-up returned to the console, or `None` if the queue was
    /// empty.
    Recalled(Option<String>),
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
        ] {
            let bytes = postcard::to_allocvec(&req).unwrap();
            let decoded: AgentReq = postcard::from_bytes(&bytes).unwrap();
            match (&req, &decoded) {
                (AgentReq::Compact { id: a, focus: fa }, AgentReq::Compact { id: b, focus: fb }) => {
                    assert_eq!(a, b);
                    assert_eq!(fa, fb);
                }
                (AgentReq::Handoff { id: a, focus: fa }, AgentReq::Handoff { id: b, focus: fb }) => {
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
                _ => panic!("mismatched variant"),
            }
        }
    }
}
