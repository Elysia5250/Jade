//! Transport and host boundary traits for `envoy-service`.
//!
//! `envoy-service` does not know how peer streams are dialed or verified;
//! any transport adapter implements [`AgentDialer`] / passes an [`AgentStream`]
//! and any policy host implements [`AgentHost`].

use envoy_core::{AgentConfig, AgentConfigStore};
use anyhow::Result;
use futures::future::BoxFuture;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Marker trait for bidirectional byte streams carried by an external transport.
pub trait AgentStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> AgentStream for T {}

/// Boxed bidirectional stream returned by [`AgentDialer`].
pub struct BoxStream(pub Box<dyn AgentStream>);

impl BoxStream {
    pub fn new<S: AgentStream>(stream: S) -> Self {
        Self(Box::new(stream))
    }
}

impl AsyncRead for BoxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for BoxStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

/// Future network/IPC adapters implement this to open agent streams to a peer.
pub trait AgentDialer: Send + Sync + 'static {
    fn open_stream<'a>(&'a self, peer: &'a str, service_id: u16) -> BoxFuture<'a, Result<BoxStream>>;
}

/// Supplies live [`AgentConfig`] to [`crate::AgentServer`].
pub trait AgentHost: Send + Sync + 'static {
    fn config(&self) -> AgentConfig;
}

/// Host backed by an in-memory [`AgentConfig`] (handy for tests and embedding).
#[derive(Clone, Default)]
pub struct StaticHost {
    pub config: AgentConfig,
}

impl StaticHost {
    pub fn new(config: AgentConfig) -> Self {
        Self { config }
    }
}

impl AgentHost for StaticHost {
    fn config(&self) -> AgentConfig {
        self.config.clone()
    }
}

/// Host backed by `agent.json` in a data directory; reloads on each request so
/// CLI `enable` / `allow` / `deny` take effect without restarting the server.
#[derive(Clone)]
pub struct StoreHost {
    store: Arc<Mutex<AgentConfigStore>>,
}

impl StoreHost {
    pub fn open(root: PathBuf) -> Result<Self> {
        let store = AgentConfigStore::open(root)?;
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
        })
    }

    pub fn from_store(store: Arc<Mutex<AgentConfigStore>>) -> Self {
        Self { store }
    }
}

impl AgentHost for StoreHost {
    fn config(&self) -> AgentConfig {
        let mut guard = self.store.lock().unwrap();
        let _ = guard.reload();
        guard.config.clone()
    }
}
