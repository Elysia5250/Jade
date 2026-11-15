//! Agent service & client over generic bidirectional streams.
//!
//! Zero dependency on any specific network engine: transports supply any
//! `AsyncRead + AsyncWrite + Unpin + Send` stream and implement
//! [`AgentDialer`]. The daemon's Unix socket and the conduit fabric bridge are
//! both just dialers.

pub mod client;
pub mod server;
pub mod transport;

pub use client::{
    cancel, jobs, list, logs, open, probe, read, status, submit, write, Conn, Remote, WriteStream,
};
pub use envoy_proto::{Hello, Job, JobId, JobResult, JobState, LogRecord, SubmitRequest, TaskSpec};
pub use server::AgentServer;
pub use transport::{
    AgentDialer, AgentHost, AgentStream, BoxStream, StaticHost, StoreHost,
};

#[cfg(unix)]
pub use transport::{dialer_for, RouteDialer, UnixDialer};
