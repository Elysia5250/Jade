//! Standalone agent service & client over generic bidirectional streams.
//!
//! Zero dependency on any specific network engine: callers or future transport
//! adapters supply any `AsyncRead + AsyncWrite + Unpin + Send` stream (and
//! optionally implement [`AgentDialer`] / [`AgentHost`]).

pub mod client;
pub mod server;
pub mod transport;

pub use client::{list, open_call, probe, pump, read, write, AgentClient, Hello, ReadStream, WriteStream};
pub use server::AgentServer;
pub use transport::{AgentDialer, AgentHost, AgentStream, BoxStream, StaticHost, StoreHost};
