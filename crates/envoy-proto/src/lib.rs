//! Agent wire protocol, job model, and length-prefixed framing.
//!
//! The protocol is split along the interaction/execution seam:
//!
//! * **Execution** lives in the daemon. A [`Job`] is submitted once, runs to
//!   completion regardless of who is watching, and keeps its output on disk.
//! * **Interaction** is a set of short request/reply exchanges, each on its own
//!   stream: `Probe`, `Submit`, `Jobs`, `JobStatus`, `JobLog`, `JobCancel`, and
//!   the synchronous file operations.
//!
//! Every stream opens with one [`ClientMsg::Call`]. A fixed `ENVY` header carries
//! the protocol version before the postcard body, so a mismatch answers
//! [`Reply::VersionMismatch`] instead of a decode failure. The agent then answers
//! [`Reply::Hello`] (or `Denied` / `Failed`) and the replies for that one request.
//! Closing the stream detaches a log follower — it never cancels a job.

pub mod codec;

use serde::{Deserialize, Serialize};

/// Default mux service identifier when carried over a multiplexed transport.
pub const AGENT_SERVICE_ID: u16 = 16;

/// Protocol generation. Bumped only for incompatible changes.
/// The framed header uses the same number as [`WIRE_VERSION`].
pub const AGENT_VERSION: u8 = 4;
/// Fixed header version. Checked before the postcard body is decoded.
pub const WIRE_VERSION: u16 = 4;
/// Magic that opens every framed payload (`ENVY`).
pub const WIRE_MAGIC: [u8; 4] = *b"ENVY";
/// `WIRE_MAGIC` plus a little-endian [`WIRE_VERSION`].
pub const WIRE_HEADER_LEN: usize = 6;

const _: () = assert!(WIRE_HEADER_LEN == 6);
const _: () = assert!(WIRE_VERSION == AGENT_VERSION as u16);

/// Raised by [`codec::read_framed`] when the fixed header is not this build.
#[derive(Debug, Clone)]
pub struct VersionMismatch {
    /// Version this process speaks ([`WIRE_VERSION`]).
    pub local_version: u16,
    /// Version in the peer header. `0` means the magic was not [`WIRE_MAGIC`].
    pub peer_version: u16,
}

impl std::fmt::Display for VersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.peer_version == 0 {
            write!(
                f,
                "wire protocol mismatch: this process speaks v{}, and the peer frame has no ENVY header. Stop the old daemon with `envoy daemon stop` and start this binary",
                self.local_version
            )
        } else {
            write!(
                f,
                "wire protocol mismatch: this process speaks v{}, peer speaks v{}. Stop the old daemon with `envoy daemon stop` and start this binary",
                self.local_version, self.peer_version
            )
        }
    }
}

impl std::error::Error for VersionMismatch {}
/// Payload ceiling for one output/log chunk.
pub const AGENT_CHUNK: usize = 32 * 1024;
/// Maximum length of a single framed message (8 MiB).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// A command that outlives this is killed; callers may lower it per call.
pub const DEFAULT_EXEC_TIMEOUT: u64 = 600;
/// Hard ceiling on a per-call timeout.
pub const MAX_EXEC_TIMEOUT: u64 = 3600;
/// Largest `Read` a single call returns; callers page with `offset`.
pub const READ_LIMIT: u64 = 4 * 1024 * 1024;
/// The group every job lands in unless the caller names another.
pub const DEFAULT_GROUP: &str = "default";
/// Caller id used for same-user local connections (daemon socket).
pub const LOCAL_CALLER: &str = "local";

/// Stable identifier of a submitted job.
pub type JobId = u64;

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskSpec {
    /// `sh -c <command>`: shell syntax, pipes, globs.
    Shell { command: String },
    /// `argv` verbatim — no shell, no quoting surprises.
    Argv { argv: Vec<String> },
}

impl TaskSpec {
    /// Human form, also used in audit logs.
    pub fn display(&self) -> String {
        match self {
            TaskSpec::Shell { command } => command.clone(),
            TaskSpec::Argv { argv } => argv.join(" "),
        }
    }

    /// Program plus arguments to spawn.
    pub fn program(&self) -> Vec<String> {
        match self {
            TaskSpec::Shell { command } => {
                vec!["sh".to_string(), "-c".to_string(), command.clone()]
            }
            TaskSpec::Argv { argv } => argv.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            TaskSpec::Shell { command } => command.trim().is_empty(),
            TaskSpec::Argv { argv } => argv.is_empty(),
        }
    }
}

/// OS-level execution sandbox requested for a submitted job.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SandboxPolicy {
    /// No OS sandbox wrapper.
    #[default]
    Off,
    /// Read-only filesystem view (plus `/tmp`), optional network.
    ReadOnly { cwd: String, allow_net: bool },
    /// Workspace-writable sandbox (root read-only, `cwd` and `extra_rw` writable,
    /// protected dirs read-only/hidden), network disabled by default.
    WorkspaceWrite {
        cwd: String,
        extra_rw: Vec<String>,
        allow_net: bool,
    },
    /// Unsandboxed full host access (explicitly approved).
    DangerFullAccess,
}

/// Everything needed to enqueue a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitRequest {
    pub spec: TaskSpec,
    pub cwd: Option<String>,
    /// Extra environment on top of the daemon's minimal base environment.
    pub env: Vec<(String, String)>,
    pub label: Option<String>,
    /// Jobs in one group share a concurrency limit.
    pub group: String,
    /// Wall-clock cap; 0 means [`DEFAULT_EXEC_TIMEOUT`].
    pub timeout_s: u64,
    /// OS sandbox policy for this job.
    pub sandbox: SandboxPolicy,
}

/// Kind of a directory entry.
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

/// How a job ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobResult {
    /// Exited with 0.
    Success,
    /// Exited with a non-zero status.
    Failed { code: i32 },
    /// The program never started.
    FailedToSpawn { error: String },
    /// Cancelled by a caller.
    Killed,
    /// Exceeded its wall-clock cap and was killed.
    TimedOut,
    /// Infrastructure error (log file, daemon restart, ...).
    Errored { error: String },
}

impl JobResult {
    pub fn ok(&self) -> bool {
        matches!(self, JobResult::Success)
    }

    /// Process exit code, mirroring the shell convention for non-exit outcomes.
    pub fn code(&self) -> i32 {
        match self {
            JobResult::Success => 0,
            JobResult::Failed { code } => *code,
            JobResult::FailedToSpawn { .. } | JobResult::Errored { .. } => 127,
            JobResult::Killed => 137,
            JobResult::TimedOut => 124,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            JobResult::Success => "success".into(),
            JobResult::Failed { code } => format!("exit {code}"),
            JobResult::FailedToSpawn { error } => format!("failed to spawn: {error}"),
            JobResult::Killed => "killed".into(),
            JobResult::TimedOut => "timed out".into(),
            JobResult::Errored { error } => format!("error: {error}"),
        }
    }
}

/// Lifecycle of a job. `Queued` waits for a free slot in its group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Queued { since_ms: u64 },
    Running { since_ms: u64 },
    Done { at_ms: u64, result: JobResult },
}

impl JobState {
    pub fn label(&self) -> &'static str {
        match self {
            JobState::Queued { .. } => "queued",
            JobState::Running { .. } => "running",
            JobState::Done { .. } => "done",
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(self, JobState::Done { .. })
    }

    pub fn result(&self) -> Option<&JobResult> {
        match self {
            JobState::Done { result, .. } => Some(result),
            _ => None,
        }
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.result().map(JobResult::code)
    }
}

/// Public snapshot of a job. Environment is deliberately absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    /// Authenticated caller that submitted it (`local` for this device).
    pub owner: String,
    pub spec: TaskSpec,
    /// Working directory, already resolved.
    pub cwd: String,
    pub label: Option<String>,
    pub group: String,
    pub created_ms: u64,
    pub state: JobState,
    /// Process-group leader while running.
    pub pid: Option<u32>,
    /// Bytes of output written so far.
    pub log_bytes: u64,
}

impl Job {
    pub fn is_done(&self) -> bool {
        self.state.is_done()
    }

    pub fn command(&self) -> String {
        self.spec.display()
    }
}

/// One interleaved chunk of a job's output log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    /// Byte offset of this record inside the log file (resume cursor).
    pub seq: u64,
    /// 0 = stdout, 1 = stderr.
    pub fd: u8,
    pub data: Vec<u8>,
}

/// Facts a peer reports in the handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub version: u8,
    pub os: String,
    pub arch: String,
    pub hostname: String,
    /// The conduit device id, when this stream arrived over the fabric.
    pub device: Option<String>,
}

/// The one request a stream carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Op {
    /// Handshake only.
    Probe,
    /// Enqueue a job; answered immediately, execution is detached.
    Submit(SubmitRequest),
    /// All known jobs, oldest first.
    Jobs,
    JobStatus {
        id: JobId,
    },
    /// Read (and optionally follow) a job's output log.
    JobLog {
        id: JobId,
        offset: u64,
        follow: bool,
    },
    JobCancel {
        id: JobId,
    },
    /// Read up to `limit` bytes at `offset` (`limit` 0 = [`READ_LIMIT`]).
    Read {
        path: String,
        offset: u64,
        limit: u64,
    },
    /// Write a file: `Data` frames follow the call, `End` commits it.
    Write {
        path: String,
        mode: Option<u32>,
    },
    /// One directory level, sorted, no filtering.
    List {
        path: String,
    },
    /// Replace an exact snippet in a text file, atomically.
    Edit {
        path: String,
        old: String,
        new: String,
        /// Replace every occurrence; otherwise a non-unique `old` is an error.
        replace_all: bool,
    },
    /// Recursively match file paths under `root`.
    Glob {
        root: String,
        pattern: String,
        max_results: u32,
    },
    /// Recursively search text files under `root` for `pattern`.
    Grep {
        root: String,
        pattern: String,
        include: Option<String>,
        max_results: u32,
    },
}

impl Op {
    pub fn name(&self) -> &'static str {
        match self {
            Op::Probe => "probe",
            Op::Submit(_) => "submit",
            Op::Jobs => "jobs",
            Op::JobStatus { .. } => "job-status",
            Op::JobLog { .. } => "job-log",
            Op::JobCancel { .. } => "job-cancel",
            Op::Read { .. } => "read",
            Op::Write { .. } => "write",
            Op::List { .. } => "list",
            Op::Edit { .. } => "edit",
            Op::Glob { .. } => "glob",
            Op::Grep { .. } => "grep",
        }
    }
}

/// One matching line returned by [`Op::Grep`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepHit {
    pub path: String,
    pub line: u32,
    pub text: String,
}

/// The first frame a client sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub version: u8,
    pub op: Op,
}

/// Client -> agent frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    Call(Call),
    /// Payload of a `Write` call.
    Data(Vec<u8>),
    /// Commit a `Write` (rename `.part`); EOF without `End` cancels it.
    End,
}

/// Agent -> client frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Reply {
    Hello(Hello),
    /// The target machine refused: agent disabled or caller not allowlisted.
    Denied {
        reason: String,
    },
    Failed {
        error: String,
    },
    /// A submitted job was accepted; `job.state` may already be running.
    Accepted {
        job: Job,
    },
    Jobs {
        jobs: Vec<Job>,
    },
    Job {
        job: Job,
    },
    /// A batch of output records. `next` is the cursor for the next call;
    /// `finished` is set once the job is done and nothing further will arrive.
    Log {
        records: Vec<LogRecord>,
        next: u64,
        log_bytes: u64,
        finished: Option<i32>,
    },
    /// Precedes `Read` payload: whole-file size and permission bits.
    Meta {
        size: u64,
        mode: u32,
    },
    Data {
        data: Vec<u8>,
    },
    Listing {
        entries: Vec<Entry>,
    },
    /// The file was rewritten in place; `replacements` is how many times the
    /// snippet was replaced.
    Edited {
        replacements: usize,
    },
    /// Matching relative paths from `Op::Glob`.
    GlobMatches {
        paths: Vec<String>,
    },
    /// Matching text hits from `Op::Grep`.
    GrepMatches {
        matches: Vec<GrepHit>,
    },
    /// Terminal event for Probe/Read/List/Cancel/Write/Glob/Grep. `code` is 0 on success.
    Done {
        code: i32,
        ms: u64,
    },
    /// The peer header or `Call::version` is not [`AGENT_VERSION`].
    /// `client_version` is 0 when the frame magic was not `ENVY`.
    VersionMismatch {
        server_version: u16,
        client_version: u16,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{read_framed, write_framed};
    use tokio::io::AsyncWriteExt;

    #[test]
    fn call_survives_a_postcard_roundtrip() {
        let call = Call {
            version: AGENT_VERSION,
            op: Op::Submit(SubmitRequest {
                spec: TaskSpec::Shell {
                    command: "echo hi".into(),
                },
                cwd: Some("/tmp".into()),
                env: vec![("NO_COLOR".into(), "1".into())],
                label: Some("smoke".into()),
                group: DEFAULT_GROUP.into(),
                timeout_s: 7,
                sandbox: SandboxPolicy::WorkspaceWrite {
                    cwd: "/tmp".into(),
                    extra_rw: vec!["/var/tmp".into()],
                    allow_net: false,
                },
            }),
        };
        let bytes = postcard::to_stdvec(&call).unwrap();
        let back: Call = postcard::from_bytes(&bytes).unwrap();
        match back.op {
            Op::Submit(req) => {
                assert_eq!(req.spec.display(), "echo hi");
                assert_eq!(req.timeout_s, 7);
                assert!(matches!(
                    req.sandbox,
                    SandboxPolicy::WorkspaceWrite {
                        allow_net: false,
                        ..
                    }
                ));
            }
            _ => panic!("wrong op"),
        }
    }

    #[test]
    fn glob_and_grep_ops_and_replies_roundtrip_postcard() {
        let glob_call = Call {
            version: AGENT_VERSION,
            op: Op::Glob {
                root: "/tmp/proj".into(),
                pattern: "src/**/*.rs".into(),
                max_results: 50,
            },
        };
        let bytes = postcard::to_stdvec(&glob_call).unwrap();
        let back: Call = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.op.name(), "glob");

        let grep_reply = Reply::GrepMatches {
            matches: vec![GrepHit {
                path: "src/lib.rs".into(),
                line: 12,
                text: "pub fn run()".into(),
            }],
        };
        let rbytes = postcard::to_stdvec(&grep_reply).unwrap();
        let rback: Reply = postcard::from_bytes(&rbytes).unwrap();
        assert!(matches!(rback, Reply::GrepMatches { matches } if matches.len() == 1));
    }

    #[test]
    fn job_survives_a_postcard_roundtrip() {
        let job = Job {
            id: 9,
            owner: "dev-a".into(),
            spec: TaskSpec::Argv {
                argv: vec!["ls".into(), "-l".into()],
            },
            cwd: "/tmp".into(),
            label: None,
            group: DEFAULT_GROUP.into(),
            created_ms: 1,
            state: JobState::Done {
                at_ms: 2,
                result: JobResult::Failed { code: 3 },
            },
            pid: None,
            log_bytes: 42,
        };
        let bytes = postcard::to_stdvec(&job).unwrap();
        let back: Job = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.state.exit_code(), Some(3));
        assert_eq!(back.command(), "ls -l");
    }

    #[test]
    fn results_map_to_shell_style_codes() {
        assert_eq!(JobResult::Success.code(), 0);
        assert_eq!(JobResult::TimedOut.code(), 124);
        assert_eq!(JobResult::Killed.code(), 137);
        assert!(JobResult::Failed { code: 2 }.code() == 2);
    }

    #[tokio::test]
    async fn codec_roundtrip_over_duplex() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let msg = ClientMsg::Call(Call {
            version: AGENT_VERSION,
            op: Op::Probe,
        });
        write_framed(&mut a, &msg).await.unwrap();
        let got: ClientMsg = read_framed(&mut b).await.unwrap();
        assert!(matches!(
            got,
            ClientMsg::Call(Call {
                version: AGENT_VERSION,
                op: Op::Probe
            })
        ));
    }

    #[tokio::test]
    async fn oversized_length_is_rejected_before_the_body() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let len = ((MAX_FRAME_BYTES as u32) + 1).to_be_bytes();
        writer.write_all(&len).await.unwrap();
        let read = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            read_framed::<_, ClientMsg>(&mut reader),
        );
        let err = read
            .await
            .expect("length check waited for a body")
            .unwrap_err();
        assert!(err.to_string().contains("exceeds max"), "{err}");
    }

    #[tokio::test]
    async fn version_mismatch_is_reported_before_decode() {
        let (mut writer, mut reader) = tokio::io::duplex(256);
        let mut payload = Vec::new();
        payload.extend_from_slice(&WIRE_MAGIC);
        payload.extend_from_slice(&99u16.to_le_bytes());
        payload.extend_from_slice(b"not-postcard");
        writer
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await
            .unwrap();
        writer.write_all(&payload).await.unwrap();
        let err = read_framed::<_, ClientMsg>(&mut reader).await.unwrap_err();
        let Some(mismatch) = err.downcast_ref::<VersionMismatch>() else {
            panic!("expected VersionMismatch, got {err}");
        };
        assert_eq!(mismatch.local_version, WIRE_VERSION);
        assert_eq!(mismatch.peer_version, 99);
        let text = err.to_string();
        assert!(text.contains("v99"), "{text}");
        assert!(text.contains("envoy daemon stop"), "{text}");
    }

    #[tokio::test]
    async fn trailing_bytes_are_rejected() {
        let body = postcard::to_stdvec(&ClientMsg::End).unwrap();
        let mut payload = Vec::new();
        payload.extend_from_slice(&WIRE_MAGIC);
        payload.extend_from_slice(&WIRE_VERSION.to_le_bytes());
        payload.extend_from_slice(&body);
        payload.push(0xAB);
        let (mut writer, mut reader) = tokio::io::duplex(256);
        writer
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await
            .unwrap();
        writer.write_all(&payload).await.unwrap();
        let err = read_framed::<_, ClientMsg>(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("trailing"), "{err}");
    }
}
