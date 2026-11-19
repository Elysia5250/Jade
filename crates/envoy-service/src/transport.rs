//! Transport and host boundary traits for `envoy-service`.
//!
//! `envoy-service` does not know how peer streams are dialed or verified: a
//! transport adapter implements [`AgentDialer`] (client side) and hands an
//! [`AgentStream`] to `AgentServer::serve_stream` (server side). Any policy
//! source implements [`AgentHost`].

use anyhow::{Context, Result};
use envoy_core::{AgentConfig, AgentConfigStore};
use futures::future::BoxFuture;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
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
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for BoxStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

/// A transport adapter: opens an agent stream to `peer` for `service_id`.
pub trait AgentDialer: Send + Sync + 'static {
    fn open_stream<'a>(
        &'a self,
        peer: &'a str,
        service_id: u16,
    ) -> BoxFuture<'a, Result<BoxStream>>;
}

impl AgentDialer for Box<dyn AgentDialer> {
    fn open_stream<'a>(
        &'a self,
        peer: &'a str,
        service_id: u16,
    ) -> BoxFuture<'a, Result<BoxStream>> {
        self.as_ref().open_stream(peer, service_id)
    }
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

/// Dials the local daemon's control socket, which serves the agent protocol
/// directly for same-user callers.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct UnixDialer {
    pub path: PathBuf,
}

#[cfg(unix)]
impl UnixDialer {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[cfg(unix)]
impl AgentDialer for UnixDialer {
    fn open_stream<'a>(
        &'a self,
        _peer: &'a str,
        _service_id: u16,
    ) -> BoxFuture<'a, Result<BoxStream>> {
        Box::pin(async move {
            let stream = tokio::net::UnixStream::connect(&self.path)
                .await
                .with_context(|| {
                    format!(
                        "connecting to the daemon at {} (is it running? try `envoy daemon start`)",
                        self.path.display()
                    )
                })?;
            Ok(BoxStream::new(stream))
        })
    }
}

/// Build a dialer for a target name: `local` goes to this machine's daemon
/// socket, any other name goes to `conduit run`'s control socket.
///
/// `conduit_socket` overrides `$CONDUIT_CONTROL_SOCK` / `~/.conduit/control.sock`.
#[cfg(unix)]
pub fn dialer_for(
    data_dir: &std::path::Path,
    target: &str,
    conduit_socket: Option<PathBuf>,
) -> Box<dyn AgentDialer> {
    if target == "local" || target.is_empty() {
        Box::new(UnixDialer::new(data_dir.join("envoy.sock")))
    } else {
        let socket = conduit_socket.unwrap_or_else(crate::conduit::default_control_socket);
        Box::new(crate::conduit::ConduitDialer::new(socket))
    }
}
