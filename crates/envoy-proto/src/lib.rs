//! Standalone agent wire protocol & length-prefixed framing.
//!
//! One bidirectional stream per invocation. The client's first frame is the
//! call — its `version` doubles as the handshake. The agent answers `Hello`
//! (or `Denied` / `Failed`) and then streams events; every operation ends with
//! `Done` or a failure event. Closing the stream is the cancel signal.

pub mod codec;

use serde::{Deserialize, Serialize};

/// Default mux service identifier when carried over a multiplexed transport.
pub const AGENT_SERVICE_ID: u16 = 16;

/// Protocol generation. Bumped only for incompatible changes.
pub const AGENT_VERSION: u8 = 1;
/// Payload ceiling for one `Out` frame.
pub const AGENT_CHUNK: usize = 32 * 1024;
/// Maximum length of a single framed message (8 MiB).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// A command that outlives this is killed; callers may lower it per call.
pub const DEFAULT_EXEC_TIMEOUT: u64 = 600;
/// Hard ceiling on a per-call timeout.
pub const MAX_EXEC_TIMEOUT: u64 = 3600;
/// Largest `Read` a single call returns; callers page with `offset`.
pub const READ_LIMIT: u64 = 4 * 1024 * 1024;

/// What to do on the target machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentOp {
    /// Run `argv` directly — no shell, no quoting surprises. Wrap in
    /// `["sh","-c",script]` when shell syntax is wanted.
    Exec {
        argv: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        timeout_s: u64,
    },
    /// Read up to `limit` bytes at `offset` (`limit` 0 = [`READ_LIMIT`]).
    Read {
        path: String,
        offset: u64,
        limit: u64,
    },
    /// Write a file: `Data` frames follow the call, `End` commits it.
    Write { path: String, mode: Option<u32> },
    /// One directory level, sorted, no filtering.
    List { path: String },
    /// Handshake only: the reply's `Hello` carries os/arch/host.
    Probe,
}

/// The first frame a client sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCall {
    pub version: u8,
    pub op: AgentOp,
}

/// Client -> agent frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMsg {
    Call(AgentCall),
    /// Payload of a `Write` call.
    Data(Vec<u8>),
    /// Commit a `Write` (rename `.part`); EOF without `End` cancels it.
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mode: u32,
}

/// Agent -> client frames. `fd` 0 = stdout, 1 = stderr.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentEvent {
    Hello {
        version: u8,
        os: String,
        arch: String,
        hostname: String,
    },
    /// The target machine refused: agent disabled or caller not allowlisted.
    Denied { reason: String },
    Failed { error: String },
    /// An `Exec` child is running (process group leader).
    Started { pid: u32 },
    Out { fd: u8, data: Vec<u8> },
    /// Precedes `Read` payload: whole-file size and permission bits.
    Meta { size: u64, mode: u32 },
    Listing { entries: Vec<Entry> },
    /// Terminal event for every successful op; `code` is the child's exit
    /// status for `Exec` and 0 otherwise.
    Done { code: i32, ms: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{read_framed, write_framed};

    #[test]
    fn call_survives_a_postcard_roundtrip() {
        let call = AgentCall {
            version: AGENT_VERSION,
            op: AgentOp::Exec {
                argv: vec!["sh".into(), "-c".into(), "echo hi".into()],
                cwd: Some("/tmp".into()),
                env: vec![("NO_COLOR".into(), "1".into())],
                timeout_s: 7,
            },
        };
        let bytes = postcard::to_stdvec(&call).unwrap();
        let back: AgentCall = postcard::from_bytes(&bytes).unwrap();
        match back.op {
            AgentOp::Exec { argv, timeout_s, .. } => {
                assert_eq!(argv[2], "echo hi");
                assert_eq!(timeout_s, 7);
            }
            _ => panic!("wrong op"),
        }
    }

    #[test]
    fn event_survives_a_postcard_roundtrip() {
        let ev = AgentEvent::Out {
            fd: 1,
            data: vec![0u8; AGENT_CHUNK],
        };
        let bytes = postcard::to_stdvec(&ev).unwrap();
        match postcard::from_bytes::<AgentEvent>(&bytes).unwrap() {
            AgentEvent::Out { fd, data } => {
                assert_eq!(fd, 1);
                assert_eq!(data.len(), AGENT_CHUNK);
            }
            _ => panic!("wrong event"),
        }
    }

    #[tokio::test]
    async fn codec_roundtrip_over_duplex() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let msg = AgentMsg::Call(AgentCall {
            version: AGENT_VERSION,
            op: AgentOp::Probe,
        });
        write_framed(&mut a, &msg).await.unwrap();
        let got: AgentMsg = read_framed(&mut b).await.unwrap();
        assert!(matches!(
            got,
            AgentMsg::Call(AgentCall {
                version: AGENT_VERSION,
                op: AgentOp::Probe
            })
        ));
    }
}
