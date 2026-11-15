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
//! Every stream opens with one [`ClientMsg::Call`]; its `version` doubles as the
//! handshake. The agent answers [`Reply::Hello`] (or `Denied` / `Failed`) and
//! then the replies for that one request. Closing the stream detaches a log
//! follower — it never cancels a job.

pub mod codec;

use serde::{Deserialize, Serialize};

/// Default mux service identifier when carried over a multiplexed transport.
pub const AGENT_SERVICE_ID: u16 = 16;

/// Protocol generation. Bumped only for incompatible changes.
pub const AGENT_VERSION: u8 = 2;
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
    Queued {
        since_ms: u64,
    },
    Running {
        since_ms: u64,
    },
    Done {
        at_ms: u64,
        result: JobResult,
    },
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
    JobStatus { id: JobId },
    /// Read (and optionally follow) a job's output log.
    JobLog {
        id: JobId,
        offset: u64,
        follow: bool,
    },
    JobCancel { id: JobId },
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
        }
    }
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
    Denied { reason: String },
    Failed { error: String },
    /// A submitted job was accepted; `job.state` may already be running.
    Accepted { job: Job },
    Jobs { jobs: Vec<Job> },
    Job { job: Job },
    /// A batch of output records. `next` is the cursor for the next call;
    /// `finished` is set once the job is done and nothing further will arrive.
    Log {
        records: Vec<LogRecord>,
        next: u64,
        log_bytes: u64,
        finished: Option<i32>,
    },
    /// Precedes `Read` payload: whole-file size and permission bits.
    Meta { size: u64, mode: u32 },
    Data { data: Vec<u8> },
    Listing { entries: Vec<Entry> },
    /// Terminal event for Probe/Read/List/Cancel/Write. `code` is 0 on success.
    Done { code: i32, ms: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{read_framed, write_framed};

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
            }),
        };
        let bytes = postcard::to_stdvec(&call).unwrap();
        let back: Call = postcard::from_bytes(&bytes).unwrap();
        match back.op {
            Op::Submit(req) => {
                assert_eq!(req.spec.display(), "echo hi");
                assert_eq!(req.timeout_s, 7);
            }
            _ => panic!("wrong op"),
        }
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
}
