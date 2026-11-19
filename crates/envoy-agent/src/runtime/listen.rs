//! Console socket: one request, then an optional event stream.

use super::protocol::{AgentReq, AgentResp, SeqEvent};
use super::server::{Live, Runtime};
use crate::agent::AgentEvent;
use anyhow::{bail, Result};
use envoy_proto::codec::{read_framed, write_framed};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
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
    let req: AgentReq =
        match tokio::time::timeout(Duration::from_secs(5), read_framed(&mut rd)).await {
            Ok(Ok(req)) => req,
            Ok(Err(e)) => bail!("reading agent request: {e}"),
            Err(_) => bail!("no agent request within 5s"),
        };

    match req {
        AgentReq::Ensure {
            id,
            cwd,
            target,
            history,
        } => {
            runtime.ensure_with_hooks(id, cwd, target, history).await;
            write_framed(&mut wr, &AgentResp::Ack).await?;
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
            stream_session(live, id, from, &mut wr).await?;
        }
        AgentReq::CloseSession { id } => match runtime.close_session(&id).await {
            Ok(_) => write_framed(&mut wr, &AgentResp::Ack).await?,
            Err(e) => write_framed(&mut wr, &AgentResp::Error(format!("{e:#}"))).await?,
        },
        AgentReq::Send { id, text, mode } => {
            runtime.send(&id, text, mode);
            write_framed(&mut wr, &AgentResp::Ack).await?;
        }
        AgentReq::Interrupt { id } => {
            runtime.interrupt(&id);
            write_framed(&mut wr, &AgentResp::Ack).await?;
        }
        AgentReq::Answer { id, ask_id, label } => {
            runtime.answer(&id, ask_id, label);
            write_framed(&mut wr, &AgentResp::Ack).await?;
        }
        AgentReq::Recall { id } => {
            write_framed(&mut wr, &AgentResp::Recalled(runtime.recall(&id))).await?;
        }
        AgentReq::Compact { id, focus } => match runtime.start_compact(&id, focus, false) {
            Ok(()) => write_framed(&mut wr, &AgentResp::Ack).await?,
            Err(e) => write_framed(&mut wr, &AgentResp::Error(format!("{e:#}"))).await?,
        },
        AgentReq::Handoff { id, focus } => match runtime.start_compact(&id, focus, true) {
            Ok(()) => write_framed(&mut wr, &AgentResp::Ack).await?,
            Err(e) => write_framed(&mut wr, &AgentResp::Error(format!("{e:#}"))).await?,
        },
        AgentReq::SetPermissionMode { id, mode } => {
            runtime.set_permission_mode(&id, mode);
            write_framed(&mut wr, &AgentResp::Ack).await?;
        }
        AgentReq::StopMonitor { id, monitor_id } => {
            match runtime.stop_monitor(&id, monitor_id).await {
                Ok(()) => write_framed(&mut wr, &AgentResp::Ack).await?,
                Err(e) => write_framed(&mut wr, &AgentResp::Error(format!("{e:#}"))).await?,
            }
        }
        AgentReq::ListCheckpoints { id } => match runtime.list_checkpoints(&id) {
            Ok(list) => write_framed(&mut wr, &AgentResp::Checkpoints(list)).await?,
            Err(e) => write_framed(&mut wr, &AgentResp::Error(format!("{e:#}"))).await?,
        },
        AgentReq::Rewind { id, checkpoint_id } => match runtime.rewind(&id, checkpoint_id).await {
            Ok(()) => write_framed(&mut wr, &AgentResp::Ack).await?,
            Err(e) => write_framed(&mut wr, &AgentResp::Error(format!("{e:#}"))).await?,
        },
        AgentReq::List => {
            write_framed(&mut wr, &AgentResp::Sessions(runtime.list())).await?;
        }
        AgentReq::Attach { id, from } => {
            let Some(live) = runtime.session(&id) else {
                write_framed(&mut wr, &AgentResp::Error(format!("no such session: {id}"))).await?;
                return Ok(());
            };
            stream_session(live, id, from, &mut wr).await?;
        }
    }
    Ok(())
}

async fn stream_session<W>(live: Arc<Mutex<Live>>, id: String, from: u64, wr: &mut W) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let (mut snapshot, running, seq_now, monitors_hub, mut rx) = {
        let mut state = live.lock().unwrap();
        debug!(session = %id, from, seq = state.seq, "attach");
        let from = if from > state.seq { 0 } else { from };
        let snapshot: Vec<SeqEvent> = state
            .events
            .iter()
            .filter(|e| e.seq > from)
            .cloned()
            .collect();
        let running = state.running;
        let seq_now = state.seq;
        let hub = state.monitors.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        state.subs.push(tx);
        (snapshot, running, seq_now, hub, rx)
    };
    let active_monitors = monitors_hub.active_list();
    if !active_monitors.is_empty() {
        snapshot.push(SeqEvent {
            seq: seq_now,
            event: AgentEvent::MonitorsSync {
                monitors: active_monitors,
            },
        });
    }
    write_framed(
        wr,
        &AgentResp::Snapshot {
            events: snapshot,
            running,
        },
    )
    .await?;
    while let Some(event) = rx.recv().await {
        if write_framed(wr, &AgentResp::Event(event)).await.is_err() {
            break;
        }
    }
    Ok(())
}
