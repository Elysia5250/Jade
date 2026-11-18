//! Inbound side: idle accept connections parked on conduit.

use super::ConduitClient;
use crate::server::AgentServer;
use std::time::Duration;
use tracing::{debug, info};

/// Keep `size` idle accept connections parked on conduit and serve whatever is
/// handed off. Each worker owns exactly one parked connection at a time, so the
/// pool size is the number of simultaneous idle accepts.
pub fn spawn_accept_pool(
    server: AgentServer,
    client: ConduitClient,
    size: usize,
) -> tokio::task::JoinHandle<()> {
    let size = size.max(1);
    tokio::spawn(async move {
        info!(
            socket = %client.socket().display(),
            pool = size,
            "conduit accept pool started"
        );
        let mut workers = Vec::with_capacity(size);
        for _ in 0..size {
            let server = server.clone();
            let client = client.clone();
            workers.push(tokio::spawn(async move {
                accept_worker(server, client).await;
            }));
        }
        for worker in workers {
            let _ = worker.await;
        }
    })
}

async fn accept_worker(server: AgentServer, client: ConduitClient) {
    let mut backoff = Duration::from_millis(250);
    loop {
        match client.accept().await {
            Ok(accept) => {
                backoff = Duration::from_millis(250);
                let peer = accept.peer_device_id.clone();
                debug!(peer = %peer, "conduit handed off an inbound stream");
                let server = server.clone();
                tokio::spawn(async move {
                    if let Err(e) = server.serve_stream(&peer, accept.stream).await {
                        debug!(peer = %peer, error = %e, "inbound agent stream ended");
                    }
                });
            }
            Err(e) => {
                // Conduit not running (or restarting) is normal: stay quiet and
                // keep the local daemon fully usable.
                debug!(error = %e, "no conduit accept connection; retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}
