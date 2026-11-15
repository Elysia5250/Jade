//! `envoy-bridge`: the assembly layer between `conduit` and `envoy`.
//!
//! Neither project depends on the other. This binary is the only place that
//! knows both:
//!
//! * it runs the conduit session engine and mounts [`EnvoyService`] on
//!   `AGENT_SERVICE_ID` (16), spawning one task per inbound stream because the
//!   engine loop is serial and must never block on a job;
//! * it listens on a local control socket where a client names a peer
//!   (`[u32 BE len][device name or id]`) and gets a conduit stream spliced to
//!   the peer's envoy service.
//!
//! That is the whole "which machine" story: the caller names a device, the
//! bridge resolves it against the authenticated device set, and envoy's own
//! allowlist sees the mTLS device id as `caller_id`.

use anyhow::{bail, Context, Result};
use clap::Parser;
use conduit_core::Store;
use conduit_fabric::{EngineDeps, EngineHandle, PeerInfo, PingService, Service, SharedStatus};
use conduit_net::{
    collect_local_endpoints, new_trust_store, Channel, CoordClient, QuicEndpoint, RelayAddrs,
};
use conduit_proto::auth::{evaluate, parse_pubkey, DeviceCert, DeviceSet};
use envoy_proto::codec::write_framed;
use envoy_proto::{Reply, AGENT_SERVICE_ID};
use envoy_service::{AgentHost, AgentServer, StoreHost};
use futures::future::BoxFuture;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{info, warn};

/// Longest accepted device target in the control preface.
const MAX_TARGET: usize = 256;

#[derive(Parser, Debug)]
#[command(
    name = "envoy-bridge",
    about = "Mount the envoy agent on a conduit device session and route local requests to peers"
)]
struct Args {
    /// envoy data directory (agent.json, jobs/).
    #[arg(long, env = "ENVOY_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Control socket; defaults to `<data-dir>/bridge.sock`.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// conduit data directory (device key, account, coord).
    #[arg(long, env = "CONDUIT_DATA_DIR")]
    conduit_data_dir: Option<PathBuf>,

    /// Only use the relay even when a direct path might work.
    #[arg(long)]
    force_relay: bool,

    /// Do not advertise or browse for LAN peers.
    #[arg(long)]
    no_mdns: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();

    let envoy_dir = args
        .data_dir
        .unwrap_or_else(envoy_core::AgentConfigStore::default_data_dir);
    let socket = args
        .socket
        .unwrap_or_else(|| envoy_dir.join("bridge.sock"));
    let conduit_dir = args
        .conduit_data_dir
        .unwrap_or_else(Store::default_data_dir);

    let store = Store::open(conduit_dir)?;
    let account = store.require_account()?.clone();
    let (coord_url, coord_pin) = store.require_coord()?;
    let coord = CoordClient::new(coord_url, coord_pin)?
        .with_identity(store.identity.clone());
    let my_id = store.identity.device_id.clone();

    let trust = new_trust_store();
    if let Ok(entries) = coord.list_devices().await {
        let set = evaluate(&account.user_pubkey, &entries);
        let mut t = trust.write().unwrap();
        for cert in &set.active {
            if cert.device_id != my_id {
                if let Some(pk) = parse_pubkey(&cert.device_pubkey) {
                    t.insert(pk, cert.device_id.clone());
                }
            }
        }
    } else {
        warn!("priming the trust store failed; the engine refresh will retry");
    }

    let (mut quic, mut mapped) = QuicEndpoint::bind(&store.identity, &trust, None)?;
    let mut endpoints = collect_local_endpoints(quic.local_addr.port(), None);
    let mut reg = coord.register(endpoints.clone()).await?;
    let with_observed =
        collect_local_endpoints(quic.local_addr.port(), Some(reg.observed_ip.as_str()));
    if with_observed != endpoints {
        endpoints = with_observed;
        reg = coord.register(endpoints.clone()).await?;
    }
    if mapped.is_none() {
        if let Ok(stun_addr) = resolve_stun(&reg.stun).await {
            drop(quic);
            let (q, m) = QuicEndpoint::bind(&store.identity, &trust, Some(stun_addr))?;
            quic = q;
            mapped = m;
            endpoints =
                collect_local_endpoints(quic.local_addr.port(), Some(reg.observed_ip.as_str()));
            if let Some(mapped) = mapped {
                let s = mapped.to_string();
                if !endpoints.contains(&s) {
                    endpoints.push(s);
                }
            }
            reg = coord.register(endpoints.clone()).await?;
        }
    }
    let relay = RelayAddrs {
        tcp: coord.relay_addr(&reg),
        ws: coord.relay_ws_url(&reg),
        coord_tls: coord.tls_config(),
    };
    info!(relay = %relay.tcp, observed = %reg.observed_ip, "registered with coord");

    let (signal_tx, signal_rx) = coord.signal().await?;
    let status = Arc::new(SharedStatus::new());

    let host: Arc<dyn AgentHost> = Arc::new(
        StoreHost::open(envoy_dir.clone()).context("opening the envoy policy store")?,
    );
    let server = Arc::new(AgentServer::with_device(
        envoy_dir.clone(),
        host,
        Some(my_id.clone()),
    ));

    let (handle, engine_fut) = conduit_fabric::build(
        EngineDeps {
            identity: store.identity.clone(),
            account_user_pubkey: account.user_pubkey.clone(),
            coord: coord.clone(),
            trust,
            quic,
            endpoints,
            relay,
            signal_tx,
            signal_rx,
            force_relay: args.force_relay,
            enable_mdns: !args.no_mdns,
        },
        |_handle| {
            vec![
                Arc::new(PingService::new(status.clone())) as Arc<dyn Service>,
                Arc::new(EnvoyService {
                    server: server.clone(),
                }) as Arc<dyn Service>,
            ]
        },
        status.clone(),
    );

    // The control socket is what `envoy --on <device>` dials.
    let listener = bind_socket(&socket).await?;
    info!(socket = %socket.display(), device = %my_id, "bridge listening");
    println!("envoy-bridge listening on {}", socket.display());
    println!("this device: {} ({my_id})", store.device_name());
    for cert in handle.device_set().active {
        if cert.device_id != my_id {
            println!("peer: {} ({})", cert.name, cert.device_id);
        }
    }

    let router = tokio::spawn(serve_router(listener, handle.clone()));

    tokio::select! {
        outcome = engine_fut => outcome?,
        _ = shutdown_signal() => info!("shutdown signal; exiting"),
    }
    router.abort();
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// The envoy service riding on a conduit session.
struct EnvoyService {
    server: Arc<AgentServer>,
}

impl Service for EnvoyService {
    fn id(&self) -> u16 {
        AGENT_SERVICE_ID
    }

    fn on_stream<'a>(&'a self, peer: &'a PeerInfo, channel: Channel) -> BoxFuture<'a, ()> {
        let server = self.server.clone();
        // mTLS already authenticated this device id; envoy uses it as caller_id.
        let caller = peer.device_id.clone();
        Box::pin(async move {
            // The engine loop is serial: a job watch can run for minutes, so
            // hand the stream off and return immediately.
            tokio::spawn(async move {
                if let Err(e) = server.serve_stream(&caller, channel).await {
                    info!(peer = %caller, error = %e, "agent stream ended");
                }
            });
        })
    }
}

async fn bind_socket(path: &PathBuf) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)
        .with_context(|| format!("binding the bridge socket {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(listener)
}

async fn serve_router(listener: UnixListener, handle: EngineHandle) {
    loop {
        match listener.accept().await {
            Ok((client, _)) => {
                let handle = handle.clone();
                tokio::spawn(async move {
                    if let Err(e) = route(client, &handle).await {
                        tracing::debug!(error = %e, "control connection ended");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Read the target preface, resolve it to a device, open an agent stream, splice.
async fn route(mut client: UnixStream, handle: &EngineHandle) -> Result<()> {
    let mut len_buf = [0u8; 4];
    client.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_TARGET {
        bail!("bad device target length {len}");
    }
    let mut buf = vec![0u8; len];
    client.read_exact(&mut buf).await?;
    let target = String::from_utf8(buf).context("device target is not utf-8")?;

    let set = handle.device_set();
    let device = match resolve(&set, &target) {
        Ok(device) => device,
        Err(reason) => {
            // Answer in the agent protocol so the caller sees a real error
            // instead of a bare EOF during the handshake.
            let _ = write_framed(&mut client, &Reply::Denied { reason }).await;
            return Ok(());
        }
    };

    match handle.open(&device, AGENT_SERVICE_ID).await {
        Ok(mut channel) => {
            info!(peer = %device, "routing an agent stream");
            client.flush().await.ok();
            tokio::io::copy_bidirectional(&mut client, &mut channel).await?;
            Ok(())
        }
        Err(e) => {
            let reason = format!("opening a stream to {target} ({device}): {e:#}");
            let _ = write_framed(&mut client, &Reply::Denied { reason }).await;
            Ok(())
        }
    }
}

/// Resolve a user-supplied target to an authenticated device id.
fn resolve(set: &DeviceSet, target: &str) -> std::result::Result<String, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("empty device target".into());
    }
    if set.active.iter().any(|c| c.device_id == target) {
        return Ok(target.to_string());
    }
    let wanted = target.to_lowercase();
    let hits: Vec<&DeviceCert> = set
        .active
        .iter()
        .filter(|c| c.name.to_lowercase() == wanted)
        .collect();
    let known = || {
        set.active
            .iter()
            .map(|c| format!("{} ({})", c.name, c.device_id))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match hits.len() {
        1 => Ok(hits[0].device_id.clone()),
        0 => Err(format!(
            "{target} is not an active device of this account; known: {}",
            known()
        )),
        n => Err(format!(
            "{target} matches {n} devices; use a device id instead; known: {}",
            known()
        )),
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

async fn resolve_stun(advertised: &str) -> Result<std::net::SocketAddr> {
    use std::net::ToSocketAddrs as _;
    let advertised = advertised.trim().to_string();
    if advertised.is_empty() {
        bail!("coord advertises no stun");
    }
    let for_error = advertised.clone();
    let mut addrs: Vec<std::net::SocketAddr> = tokio::task::spawn_blocking(move || {
        advertised
            .to_socket_addrs()
            .map(|it| it.filter(|a| a.is_ipv4()).collect())
            .unwrap_or_default()
    })
    .await?;
    addrs.truncate(1);
    addrs
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("stun address {for_error} did not resolve"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert(id: &str, name: &str) -> DeviceCert {
        DeviceCert {
            user_id: "u1".into(),
            user_pubkey: "aa".into(),
            device_id: id.into(),
            device_pubkey: "bb".into(),
            name: name.into(),
            issued_at: 0,
            issuer_pubkey: "cc".into(),
            sig: "dd".into(),
        }
    }

    fn set(certs: Vec<DeviceCert>) -> DeviceSet {
        DeviceSet {
            valid: certs.clone(),
            active: certs,
        }
    }

    #[test]
    fn resolves_exact_ids_and_unique_names() {
        let set = set(vec![cert("dev-aaa", "Laptop"), cert("dev-bbb", "Server")]);
        assert_eq!(resolve(&set, "dev-bbb").unwrap(), "dev-bbb");
        assert_eq!(resolve(&set, "laptop").unwrap(), "dev-aaa");
        assert_eq!(resolve(&set, "  SERVER  ").unwrap(), "dev-bbb");
    }

    #[test]
    fn unknown_targets_list_what_is_available() {
        let set = set(vec![cert("dev-aaa", "Laptop")]);
        let err = resolve(&set, "desktop").unwrap_err();
        assert!(err.contains("not an active device"), "{err}");
        assert!(err.contains("Laptop (dev-aaa)"), "{err}");
    }

    #[test]
    fn ambiguous_names_refuse_to_guess() {
        let set = set(vec![cert("dev-aaa", "Box"), cert("dev-bbb", "Box")]);
        let err = resolve(&set, "box").unwrap_err();
        assert!(err.contains("matches 2 devices"), "{err}");
    }

    #[test]
    fn empty_targets_are_rejected() {
        assert!(resolve(&set(vec![]), "   ").is_err());
    }
}
