//! Client helpers for connecting to the daemon control socket (`agent.sock`).

use super::hello::{protocol_mismatch_message, read_hello, write_hello, AGENT_PROTOCOL_VERSION};
use super::protocol::{AgentReq, AgentResp};
use super::socket_path;
use crate::session::ChatMessage;
use anyhow::{bail, Context, Result};
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
    write_hello(&mut stream, 0).await?;
    let server_hello = read_hello(&mut stream).await?;
    if server_hello.protocol_version != AGENT_PROTOCOL_VERSION {
        bail!(
            "{}",
            protocol_mismatch_message(server_hello.protocol_version, AGENT_PROTOCOL_VERSION)
        );
    }
    write_framed(&mut stream, req).await?;
    let resp: AgentResp = read_framed(&mut stream).await?;
    Ok(resp)
}

/// Fire-and-forget helper for send/interrupt/answer. `AgentResp::Error` is `Err`.
pub async fn tell(data_dir: &Path, req: AgentReq) -> Result<()> {
    match request(data_dir, &req).await {
        Ok(AgentResp::Error(reason)) => {
            debug!(error = %reason, "agent runtime rejected the request");
            bail!("{reason}");
        }
        Ok(_) => Ok(()),
        Err(err) => {
            debug!(error = %err, "agent runtime is unreachable");
            Err(err)
        }
    }
}

/// Last replay cursor and the daemon identity the console observed.
pub struct AttachOrigin {
    pub from: u64,
    pub incarnation_id: u64,
}

/// Attach (and create if needed) and forward the replay plus live events into
/// `tx`. `origin.incarnation_id` is `0` on the first attach. Sends an `Error`
/// when the stream ends so the console can reconnect.
pub async fn attach(
    data_dir: PathBuf,
    id: String,
    cwd: PathBuf,
    target: String,
    history: Vec<ChatMessage>,
    origin: AttachOrigin,
    tx: mpsc::UnboundedSender<AgentResp>,
) {
    let path = socket_path(&data_dir);
    let Ok(mut stream) = tokio::net::UnixStream::connect(&path).await else {
        let _ = tx.send(AgentResp::Error("agent runtime is not running".into()));
        return;
    };
    if let Err(err) = write_hello(&mut stream, origin.incarnation_id).await {
        let _ = tx.send(AgentResp::Error(format!("{err:#}")));
        return;
    }
    let server_hello = match read_hello(&mut stream).await {
        Ok(hello) => hello,
        Err(err) => {
            let _ = tx.send(AgentResp::Error(format!("{err:#}")));
            return;
        }
    };
    if server_hello.protocol_version != AGENT_PROTOCOL_VERSION {
        let _ = tx.send(AgentResp::Error(protocol_mismatch_message(
            server_hello.protocol_version,
            AGENT_PROTOCOL_VERSION,
        )));
        return;
    }
    let req = AgentReq::Resume {
        id,
        cwd,
        target,
        history,
        from: origin.from,
    };
    if write_framed(&mut stream, &req).await.is_err() {
        let _ = tx.send(AgentResp::Error(
            "agent runtime is not accepting requests".into(),
        ));
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
        Ok(Err(err)) => {
            debug!(error = %err, "agent attach failed");
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
            Err(err) => {
                debug!(error = %err, "agent event stream ended");
                let _ = tx.send(AgentResp::Error("agent event stream closed".into()));
                return;
            }
        }
    }
}
