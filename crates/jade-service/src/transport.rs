//! Transport and host boundary traits for `jade-service`.
//!
//! `jade-service` does not know how peer streams are dialed or verified: a
//! transport adapter implements [`AgentDialer`] (client side) and hands an
//! [`AgentStream`] to `AgentServer::serve_stream` (server side). Any policy
//! source implements [`AgentHost`].

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use jade_core::{AgentConfig, AgentConfigStore};
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

/// File identity used to skip redundant `agent.json` reads when unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
    ino: u64,
}

fn file_stamp(path: &std::path::Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    };
    #[cfg(not(unix))]
    let ino = 0;
    Some(FileStamp {
        modified: meta.modified().ok(),
        len: meta.len(),
        ino,
    })
}

/// Host backed by `agent.json` in a data directory; reloads when `agent.json`
/// changes on disk so CLI `enable` / `allow` / `deny` take effect immediately
/// without re-reading and parsing JSON on every unchanged RPC.
#[derive(Clone)]
pub struct StoreHost {
    store: Arc<Mutex<AgentConfigStore>>,
    stamp: Arc<Mutex<Option<Option<FileStamp>>>>,
}

impl StoreHost {
    pub fn open(root: PathBuf) -> Result<Self> {
        let initial = file_stamp(&root.join("agent.json"));
        let store = AgentConfigStore::open(root)?;
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            stamp: Arc::new(Mutex::new(Some(initial))),
        })
    }

    pub fn from_store(store: Arc<Mutex<AgentConfigStore>>) -> Self {
        let initial = {
            let guard = store.lock().unwrap();
            file_stamp(&guard.root.join("agent.json"))
        };
        Self {
            store,
            stamp: Arc::new(Mutex::new(Some(initial))),
        }
    }
}

impl AgentHost for StoreHost {
    fn config(&self) -> AgentConfig {
        let mut guard = self.store.lock().unwrap();
        let current = file_stamp(&guard.root.join("agent.json"));
        let mut stamp_guard = self.stamp.lock().unwrap();
        if *stamp_guard != Some(current) {
            if guard.reload().is_ok() {
                *stamp_guard = Some(current);
            }
        }
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
                        "connecting to the daemon at {} (is it running? try `jade daemon start`)",
                        self.path.display()
                    )
                })?;
            Ok(BoxStream::new(stream))
        })
    }
}

/// Build a dialer for a target name: `local` goes to this machine's daemon
/// socket, any other name goes to `topaz run`'s control socket.
///
/// `topaz_socket` overrides `$TOPAZ_CONTROL_SOCK` / `~/.topaz/control.sock`.
#[cfg(unix)]
pub fn dialer_for(
    data_dir: &std::path::Path,
    target: &str,
    topaz_socket: Option<PathBuf>,
) -> Box<dyn AgentDialer> {
    if target == "local" || target.is_empty() {
        Box::new(UnixDialer::new(data_dir.join("jade.sock")))
    } else {
        let socket = topaz_socket.unwrap_or_else(crate::topaz::default_control_socket);
        Box::new(crate::topaz::TopazDialer::new(socket))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_host_caches_unchanged_config_and_reloads_on_external_write() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jade-store-host-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();

        let host = StoreHost::open(dir.clone()).unwrap();
        assert!(!host.config().enabled);
        assert!(host.config().allow.is_empty());

        // External CLI write updates agent.json via atomic rename.
        let mut ext = AgentConfigStore::open(dir.clone()).unwrap();
        ext.set_enabled(true).unwrap();
        ext.allow("dev-peer".into()).unwrap();

        let reloaded = host.config();
        assert!(reloaded.enabled);
        assert_eq!(reloaded.allow, vec!["dev-peer".to_string()]);

        // Deleting agent.json resets back to defaults on next read.
        std::fs::remove_file(dir.join("agent.json")).unwrap();
        let reset = host.config();
        assert!(!reset.enabled);
        assert!(reset.allow.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
