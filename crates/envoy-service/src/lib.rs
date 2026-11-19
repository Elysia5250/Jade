//! Agent service & client over generic bidirectional streams.
//!
//! Zero dependency on any specific network engine: transports supply any
//! `AsyncRead + AsyncWrite + Unpin + Send` stream and implement
//! [`AgentDialer`]. The daemon's Unix socket and the conduit fabric bridge are
//! both just dialers.

pub mod client;
pub mod conduit;
pub mod remote;
pub mod server;
pub mod transport;

pub use client::{
    cancel, edit, jobs, list, logs, open, probe, read, status, submit, write, Conn, WriteStream,
};
pub use conduit::{
    default_control_socket, spawn_accept_pool, ConduitClient, ConduitDialer, Device, DevicesReply,
    Status, WsByteStream, DEFAULT_POOL,
};
pub use envoy_proto::{
    GrepHit, Hello, Job, JobId, JobResult, JobState, LogRecord, SandboxPolicy, SubmitRequest,
    TaskSpec,
};
pub use remote::{glob, grep, Remote};
pub use server::AgentServer;
pub use transport::{AgentDialer, AgentHost, AgentStream, BoxStream, StaticHost, StoreHost};

#[cfg(unix)]
pub use transport::{dialer_for, UnixDialer};
