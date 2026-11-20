//! Console socket: hello, one request, then an optional event stream.

use super::hello::{protocol_mismatch_message, read_hello, write_hello, AGENT_PROTOCOL_VERSION};
use super::protocol::{AgentReq, AgentResp};
use super::server::Runtime;
use super::snapshot::stream_session;
use anyhow::{bail, Result};
use envoy_proto::codec::{read_framed, write_framed};
use envoy_proto::VersionMismatch;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::debug;

/// Serve the console control socket until cancelled.
pub async fn serve(runtime: Arc<Runtime>, listener: tokio::net::UnixListener) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                debug!(error = %e, "agent socket accept failed");
                continue;
            }
        };
        #[cfg(unix)]
        {
            let same_user = stream
                .peer_cred()
                .map(|cred| cred.uid() == unsafe { libc::geteuid() })
                .unwrap_or(false);
            if !same_user {
                tracing::warn!("rejected agent socket connection from another uid");
                continue;
            }
        }
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, runtime).await {
                debug!(error = %e, "agent session connection ended");
            }
        });
    }
}

async fn handle<S>(stream: S, runtime: Arc<Runtime>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let client_hello = match tokio::time::timeout(Duration::from_secs(5), read_hello(&mut rd)).await
    {
        Ok(Ok(hello)) => hello,
        Ok(Err(err)) => {
            if err.downcast_ref::<VersionMismatch>().is_some() {
                let _ = write_framed(
                    &mut wr,
                    &AgentResp::Error(format!(
                        "{err:#}. Stop the old daemon with `envoy daemon stop` and start this binary again"
                    )),
                )
                .await;
                return Ok(());
            }
            bail!("reading agent hello: {err:#}");
        }
        Err(_) => bail!("no agent hello within 5s"),
    };
    write_hello(&mut wr, runtime.incarnation_id()).await?;
    if client_hello.protocol_version != AGENT_PROTOCOL_VERSION {
        write_framed(
            &mut wr,
            &AgentResp::Error(protocol_mismatch_message(
                client_hello.protocol_version,
                AGENT_PROTOCOL_VERSION,
            )),
        )
        .await?;
        return Ok(());
    }
    let req: AgentReq =
        match tokio::time::timeout(Duration::from_secs(5), read_framed(&mut rd)).await {
            Ok(Ok(req)) => req,
            Ok(Err(err)) => bail!("reading agent request: {err:#}"),
            Err(_) => bail!("no agent request within 5s"),
        };
    let reset =
        client_hello.incarnation_id != 0 && client_hello.incarnation_id != runtime.incarnation_id();
    dispatch(&runtime, req, reset, &mut wr).await
}

async fn dispatch<W>(runtime: &Arc<Runtime>, req: AgentReq, reset: bool, wr: &mut W) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if session_req(&req) {
        return dispatch_session(runtime, req, reset, wr).await;
    }
    if queue_req(&req) {
        return dispatch_queue(runtime, req, wr).await;
    }
    dispatch_edit(runtime, req, wr).await
}

fn session_req(req: &AgentReq) -> bool {
    matches!(
        req,
        AgentReq::Ensure { .. }
            | AgentReq::Resume { .. }
            | AgentReq::CloseSession { .. }
            | AgentReq::Attach { .. }
    )
}

fn queue_req(req: &AgentReq) -> bool {
    matches!(
        req,
        AgentReq::Send { .. }
            | AgentReq::Interrupt { .. }
            | AgentReq::Answer { .. }
            | AgentReq::Recall { .. }
            | AgentReq::SetPermissionMode { .. }
    )
}

async fn dispatch_session<W>(
    runtime: &Arc<Runtime>,
    req: AgentReq,
    reset: bool,
    wr: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    match req {
        AgentReq::Ensure {
            id,
            cwd,
            target,
            history,
        } => {
            runtime.ensure_with_hooks(id, cwd, target, history).await;
            ack(wr).await?;
        }
        AgentReq::Resume {
            id,
            cwd,
            target,
            history,
            from,
        } => {
            let live = runtime
                .ensure_with_hooks(id.clone(), cwd, target, history)
                .await;
            stream_session(runtime, live, id, from, reset, wr).await?;
        }
        AgentReq::CloseSession { id } => match runtime.close_session(&id).await {
            Ok(_) => ack(wr).await?,
            Err(err) => fail(wr, format!("{err:#}")).await?,
        },
        AgentReq::Attach { id, from } => {
            let Some(live) = runtime.session(&id) else {
                fail(wr, no_session(&id)).await?;
                return Ok(());
            };
            stream_session(runtime, live, id, from, reset, wr).await?;
        }
        other => bail!("unhandled session request: {other:?}"),
    }
    Ok(())
}

async fn dispatch_queue<W>(runtime: &Arc<Runtime>, req: AgentReq, wr: &mut W) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    match req {
        AgentReq::Send { id, text, mode } => {
            if runtime.session(&id).is_none() {
                fail(wr, no_session(&id)).await?;
            } else {
                runtime.send(&id, text, mode);
                ack(wr).await?;
            }
        }
        AgentReq::Interrupt { id } => {
            if runtime.session(&id).is_none() {
                fail(wr, no_session(&id)).await?;
            } else {
                runtime.interrupt(&id);
                ack(wr).await?;
            }
        }
        AgentReq::Answer { id, ask_id, label } => {
            if runtime.session(&id).is_none() {
                fail(wr, no_session(&id)).await?;
            } else {
                runtime.answer(&id, ask_id, label);
                ack(wr).await?;
            }
        }
        AgentReq::Recall { id } => {
            if runtime.session(&id).is_none() {
                fail(wr, no_session(&id)).await?;
            } else {
                write_framed(wr, &AgentResp::Recalled(runtime.recall(&id))).await?;
            }
        }
        AgentReq::SetPermissionMode { id, mode } => {
            if runtime.session(&id).is_none() {
                fail(wr, no_session(&id)).await?;
            } else {
                runtime.set_permission_mode(&id, mode);
                ack(wr).await?;
            }
        }
        other => bail!("unhandled queue request: {other:?}"),
    }
    Ok(())
}

async fn dispatch_edit<W>(runtime: &Arc<Runtime>, req: AgentReq, wr: &mut W) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    match req {
        AgentReq::Compact { id, focus } => match runtime.start_compact(&id, focus, false) {
            Ok(()) => ack(wr).await?,
            Err(err) => fail(wr, format!("{err:#}")).await?,
        },
        AgentReq::Handoff { id, focus } => match runtime.start_compact(&id, focus, true) {
            Ok(()) => ack(wr).await?,
            Err(err) => fail(wr, format!("{err:#}")).await?,
        },
        AgentReq::StopMonitor { id, monitor_id } => {
            match runtime.stop_monitor(&id, monitor_id).await {
                Ok(()) => ack(wr).await?,
                Err(err) => fail(wr, format!("{err:#}")).await?,
            }
        }
        AgentReq::ListCheckpoints { id } => match runtime.list_checkpoints(&id) {
            Ok(list) => write_framed(wr, &AgentResp::Checkpoints(list)).await?,
            Err(err) => fail(wr, format!("{err:#}")).await?,
        },
        AgentReq::Rewind { id, checkpoint_id } => match runtime.rewind(&id, checkpoint_id).await {
            Ok(()) => ack(wr).await?,
            Err(err) => fail(wr, format!("{err:#}")).await?,
        },
        AgentReq::List => {
            write_framed(wr, &AgentResp::Sessions(runtime.list())).await?;
        }
        AgentReq::SetCwd { id, cwd } => match runtime.set_cwd(&id, cwd) {
            Ok(()) => ack(wr).await?,
            Err(err) => fail(wr, format!("{err:#}")).await?,
        },
        AgentReq::SetTarget { id, target } => match runtime.set_target(&id, target) {
            Ok(()) => ack(wr).await?,
            Err(err) => fail(wr, format!("{err:#}")).await?,
        },
        AgentReq::Clear { id } => match runtime.clear_context(&id) {
            Ok(()) => ack(wr).await?,
            Err(err) => fail(wr, format!("{err:#}")).await?,
        },
        AgentReq::ProbeMcp { cwd } => {
            let lines = runtime.probe_mcp(cwd).await;
            write_framed(wr, &AgentResp::McpProbe(lines)).await?;
        }
        AgentReq::RefreshModels => match runtime.refresh_models().await {
            Ok((provider_id, models)) => {
                write_framed(
                    wr,
                    &AgentResp::ModelsRefreshed {
                        provider_id,
                        models,
                    },
                )
                .await?;
            }
            Err(err) => fail(wr, format!("{err:#}")).await?,
        },
        other => bail!("unhandled edit request: {other:?}"),
    }
    Ok(())
}

fn no_session(id: &str) -> String {
    format!("no such session: {id}")
}

async fn ack<W: AsyncWrite + Unpin>(wr: &mut W) -> Result<()> {
    write_framed(wr, &AgentResp::Ack).await
}

async fn fail<W: AsyncWrite + Unpin>(wr: &mut W, message: String) -> Result<()> {
    write_framed(wr, &AgentResp::Error(message)).await
}
