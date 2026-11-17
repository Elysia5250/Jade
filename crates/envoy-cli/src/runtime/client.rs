//! Client helpers for connecting to the daemon control socket (`agent.sock`).

use super::protocol::{AgentReq, AgentResp};
use super::socket_path;
use crate::session::ChatMessage;
use anyhow::{Context, Result};
use envoy_proto::codec::{read_framed, write_framed};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tracing::debug;

/// Open one request/response exchange with the daemon.
pub async fn request(data_dir: &Path, req: &AgentReq) -> Result<AgentResp> {
    let mut stream = tokio::net::UnixStream::connect(socket_path(data_dir))
        .await
        .with_context(|| {
            format!(
                "connecting to the agent runtime at {} (is `envoy serve` up?)",
                socket_path(data_dir).display()
            )
        })?;
    write_framed(&mut stream, req).await?;
    let resp: AgentResp = read_framed(&mut stream).await?;
    Ok(resp)
}

/// Fire-and-forget helper for send/interrupt/answer.
pub async fn tell(data_dir: &Path, req: AgentReq) -> Result<()> {
    request(data_dir, &req).await.map(|_| ()).inspect_err(|e| {
        debug!(error = %e, "agent runtime is unreachable");
    })
}

/// Attach (and create if needed) and forward the replay plus live events into
/// `tx`. Sends an `Error` when the stream ends so the console can reconnect.
pub async fn attach(
    data_dir: PathBuf,
    id: String,
    cwd: PathBuf,
    target: String,
    history: Vec<ChatMessage>,
    from: u64,
    tx: mpsc::UnboundedSender<AgentResp>,
) {
    let path = socket_path(&data_dir);
    let Ok(mut stream) = tokio::net::UnixStream::connect(&path).await else {
        let _ = tx.send(AgentResp::Error("agent runtime is not running".into()));
        return;
    };
    let req = AgentReq::Resume {
        id,
        cwd,
        target,
        history,
        from,
    };
    if write_framed(&mut stream, &req).await.is_err() {
        let _ = tx.send(AgentResp::Error("agent runtime is not accepting requests".into()));
        return;
    }
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_framed::<_, AgentResp>(&mut stream),
    )
    .await
    {
        Ok(Ok(resp)) => {
            if tx.send(resp).is_err() {
                return;
            }
        }
        Ok(Err(e)) => {
            debug!(error = %e, "agent attach failed");
            let _ = tx.send(AgentResp::Error("agent event stream closed".into()));
            return;
        }
        Err(_) => {
            let _ = tx.send(AgentResp::Error(
                "agent runtime did not answer within 5s".into(),
            ));
            return;
        }
    }
    loop {
        match read_framed::<_, AgentResp>(&mut stream).await {
            Ok(resp) => {
                if tx.send(resp).is_err() {
                    return;
                }
            }
            Err(e) => {
                debug!(error = %e, "agent event stream ended");
                let _ = tx.send(AgentResp::Error("agent event stream closed".into()));
                return;
            }
        }
    }
}
