//! Inbound side: idle accept connections parked on conduit.

use super::ConduitClient;
use crate::server::AgentServer;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, info};

/// Workers and per-stream tasks spawned by [`spawn_accept_pool`].
pub struct AcceptPool {
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    stop: Arc<AtomicBool>,
}

impl AcceptPool {
    pub fn abort(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut tasks) = self.tasks.lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
    }
}

impl Drop for AcceptPool {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Keep `size` idle accept connections parked on conduit and serve whatever is
/// handed off. Each worker owns exactly one parked connection at a time, so the
/// pool size is the number of simultaneous idle accepts.
pub fn spawn_accept_pool(server: AgentServer, client: ConduitClient, size: usize) -> AcceptPool {
    let size = size.max(1);
    let tasks = Arc::new(Mutex::new(Vec::with_capacity(size)));
    let stop = Arc::new(AtomicBool::new(false));
    info!(
        socket = %client.socket().display(),
        pool = size,
        "conduit accept pool started"
    );
    for _ in 0..size {
        let server = server.clone();
        let client = client.clone();
        let tasks_for_streams = Arc::clone(&tasks);
        let worker_stop = Arc::clone(&stop);
        let worker = tokio::spawn(async move {
            accept_worker(server, client, tasks_for_streams, worker_stop).await;
        });
        push_task(&tasks, &stop, worker);
    }
    AcceptPool { tasks, stop }
}

fn push_task(tasks: &Mutex<Vec<JoinHandle<()>>>, stop: &AtomicBool, handle: JoinHandle<()>) {
    if stop.load(Ordering::SeqCst) {
        handle.abort();
        return;
    }
    let Ok(mut guard) = tasks.lock() else {
        handle.abort();
        return;
    };
    if stop.load(Ordering::SeqCst) {
        handle.abort();
        return;
    }
    guard.retain(|h| !h.is_finished());
    guard.push(handle);
}

async fn accept_worker(
    server: AgentServer,
    client: ConduitClient,
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    stop: Arc<AtomicBool>,
) {
    let mut backoff = Duration::from_millis(250);
    while !stop.load(Ordering::SeqCst) {
        match client.accept().await {
            Ok(accept) => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                backoff = Duration::from_millis(250);
                let peer = accept.peer_device_id.clone();
                debug!(peer = %peer, "conduit handed off an inbound stream");
                let server = server.clone();
                let stream = tokio::spawn(async move {
                    if let Err(e) = server
                        .serve_stream(envoy_core::Caller::Remote(peer.clone()), accept.stream)
                        .await
                    {
                        debug!(peer = %peer, error = %e, "inbound agent stream ended");
                    }
                });
                push_task(&tasks, &stop, stream);
            }
            Err(e) => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                // Conduit not running (or restarting) is normal: stay quiet and
                // keep the local daemon fully usable.
                debug!(error = %e, "no conduit accept connection; retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}
