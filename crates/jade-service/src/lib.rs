//! Agent service & client over generic bidirectional streams.
//!
//! Zero dependency on any specific network engine: transports supply any
//! `AsyncRead + AsyncWrite + Unpin + Send` stream and implement
//! [`AgentDialer`]. The daemon's Unix socket and the topaz fabric bridge are
//! both just dialers.

pub mod client;
mod local_direct;
mod policy;
pub mod remote;
pub mod server;
mod server_search;
pub mod topaz;
pub mod transport;

pub use client::{
    cancel, edit, jobs, list, logs, open, probe, read, status, submit, write, Conn, WriteStream,
};
pub use jade_core::Caller;
pub use jade_proto::{
    GrepHit, Hello, Job, JobId, JobResult, JobState, LogRecord, SandboxPolicy, SubmitRequest,
    TaskSpec,
};
pub use remote::{glob, grep, Remote};
pub use server::AgentServer;
pub use topaz::{
    default_control_socket, spawn_accept_pool, Device, DevicesReply, Status, TopazClient,
    TopazDialer, WsByteStream, DEFAULT_POOL,
};
pub use transport::{AgentDialer, AgentHost, AgentStream, BoxStream, StaticHost, StoreHost};

#[cfg(unix)]
pub use transport::{dialer_for, UnixDialer};
