//! `agent.sock` hello exchanged before every [`super::protocol::AgentReq`].

pub(crate) use super::protocol::{AgentHello, AGENT_PROTOCOL_VERSION};
use anyhow::Result;
use envoy_proto::codec::{read_framed, write_framed};
use tokio::io::{AsyncRead, AsyncWrite};

pub(crate) fn bin_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub(crate) fn protocol_mismatch_message(peer: u32, local: u32) -> String {
    format!(
        "agent protocol v{peer} does not match this binary (v{local}). Stop the old daemon with `envoy daemon stop` and start this binary again"
    )
}

pub(crate) async fn write_hello<W>(w: &mut W, incarnation_id: u64) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let hello = AgentHello {
        protocol_version: AGENT_PROTOCOL_VERSION,
        incarnation_id,
        bin_version: bin_version(),
    };
    write_framed(w, &hello).await
}

pub(crate) async fn read_hello<R>(r: &mut R) -> Result<AgentHello>
where
    R: AsyncRead + Unpin,
{
    read_framed(r).await
}
