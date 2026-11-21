//! Runtime adapter for `conduit run`'s control socket.
//!
//! `conduit` is a separate VPN daemon that owns the device keys, the QUIC/relay
//! sessions and (optionally) the TUN device. It exposes them on a Unix socket
//! as plain HTTP/1.1 + JSON and WebSocket. This module is a *client* of that
//! socket — no conduit crate, no second engine, no shared data directory.
//!
//! Two directions:
//!
//! * **Outbound** — `GET /v1/services/16/open/{target}` upgrades to a
//!   WebSocket and hands back a transparent byte stream to the peer's agent.
//! * **Inbound** — [`spawn_accept_pool`] keeps a few idle
//!   `GET /v1/services/16/accept?name=envoy` connections. When a peer opens a
//!   stream, conduit sends one text frame naming the mTLS-authenticated
//!   `peer_device_id`, then switches that connection to binary passthrough.
//!
//! Failure modes are carried as HTTP status + `{"error": "..."}`, so CLI errors
//! name the real reason (unknown device, ambiguous name, peer offline).

use crate::transport::{AgentDialer, BoxStream};
use anyhow::{bail, Context, Result};
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{client_async, WebSocketStream};
use tracing::debug;

mod accept;
mod ws;
pub use accept::{spawn_accept_pool, AcceptPool};
pub use ws::WsByteStream;

/// The service id envoy registers with conduit.
pub const AGENT_SERVICE_ID: u16 = envoy_proto::AGENT_SERVICE_ID;
/// Name reported in `/v1/services/16/accept?name=...`.
pub const SERVICE_NAME: &str = "envoy";
/// Largest accepted control-socket response body.
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
/// Idle accept connections `envoy serve` keeps parked.
pub const DEFAULT_POOL: usize = 4;
/// How often a parked accept connection is pinged.
pub const KEEPALIVE: Duration = Duration::from_secs(25);

/// Resolve the control socket: explicit path, `CONDUIT_CONTROL_SOCK`,
/// `$CONDUIT_DATA_DIR/control.sock`, else `~/.conduit/control.sock`.
pub fn default_control_socket() -> PathBuf {
    if let Some(path) = std::env::var_os("CONDUIT_CONTROL_SOCK") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Some(dir) = std::env::var_os("CONDUIT_DATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("control.sock");
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".conduit").join("control.sock")
}

/// A device as listed by `GET /v1/devices`.
#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    pub device_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub dns_name: Option<String>,
    #[serde(default)]
    pub virtual_ip: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub me: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DevicesReply {
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Tun {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub interface: Option<String>,
    #[serde(default)]
    pub cidr: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PeerService {
    pub service_id: u16,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Peer {
    pub device_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub dns_name: Option<String>,
    #[serde(default)]
    pub virtual_ip: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub rtt_ms: Option<u64>,
    /// Services the peer advertises over conduit's discovery channel.
    #[serde(default)]
    pub services: Vec<PeerService>,
}

impl Peer {
    pub fn has_service(&self, service_id: u16) -> bool {
        self.services.iter().any(|s| s.service_id == service_id)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServiceInfo {
    #[serde(default)]
    pub service_id: u16,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub listeners: usize,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Status {
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub dns_name: Option<String>,
    #[serde(default)]
    pub virtual_ip: Option<String>,
    #[serde(default)]
    pub tun: Tun,
    #[serde(default)]
    pub connection: Option<String>,
    #[serde(default)]
    pub peers: Vec<Peer>,
    #[serde(default)]
    pub services: Vec<ServiceInfo>,
}

/// One handed-off inbound stream, with the caller conduit authenticated.
pub struct Accept {
    pub peer_device_id: String,
    pub stream: BoxStream,
}

/// Client for the conduit control socket.
#[derive(Debug, Clone)]
pub struct ConduitClient {
    socket: PathBuf,
}

impl ConduitClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn from_env() -> Self {
        Self::new(default_control_socket())
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    async fn connect(&self) -> Result<UnixStream> {
        UnixStream::connect(&self.socket).await.with_context(|| {
            format!(
                "connecting to conduit at {} (is `conduit run` up?)",
                self.socket.display()
            )
        })
    }

    /// Upgrade one path to a WebSocket over the control socket.
    async fn upgrade(&self, path: &str) -> Result<WebSocketStream<UnixStream>> {
        let stream = self.connect().await?;
        let request = format!("ws://localhost{path}").into_client_request()?;
        match client_async(request, stream).await {
            Ok((ws, response)) => {
                if let Some(peer) = response.headers().get("x-conduit-peer-id") {
                    debug!(
                        peer = %peer.to_str().unwrap_or("?"),
                        path,
                        "conduit handed over a peer"
                    );
                }
                Ok(ws)
            }
            Err(WsError::Http(response)) => {
                let status = response.status();
                let raw = response
                    .body()
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(b).to_string())
                    .unwrap_or_default();
                let parsed = serde_json::from_str::<serde_json::Value>(&raw).ok();
                let reason = parsed
                    .as_ref()
                    .and_then(|v| v.get("error")?.as_str().map(str::to_string))
                    .unwrap_or_else(|| raw.trim().to_string());
                // `code` is the stable branch key; the text is for humans.
                let code = parsed
                    .as_ref()
                    .and_then(|v| v.get("code")?.as_str().map(str::to_string))
                    .unwrap_or_else(|| "unknown".to_string());
                let reason = if reason.is_empty() {
                    format!("{code} (HTTP {})", status.as_u16())
                } else {
                    format!("{reason} [{code}, HTTP {}]", status.as_u16())
                };
                bail!("conduit refused {path}: {reason}")
            }
            Err(e) => Err(e).with_context(|| format!("upgrading {path} on the conduit socket")),
        }
    }

    /// Open a stream to `target` (`device_id`, `dns_name`, `virtual_ip`, or a
    /// unique device name — conduit resolves it).
    pub async fn open(&self, target: &str) -> Result<BoxStream> {
        let path = format!(
            "/v1/services/{}/open/{}",
            AGENT_SERVICE_ID,
            encode_component(target)
        );
        let ws = self.upgrade(&path).await?;
        Ok(BoxStream::new(WsByteStream::new(ws)))
    }

    /// Park one accept connection and wait for conduit to hand off a peer.
    ///
    /// A parked connection is idle for a long time, so this pings every
    /// [`KEEPALIVE`] and treats a failed send as "conduit is gone": the caller
    /// reconnects instead of holding a half-open slot conduit still counts.
    pub async fn accept(&self) -> Result<Accept> {
        let path = format!(
            "/v1/services/{}/accept?name={}",
            AGENT_SERVICE_ID, SERVICE_NAME
        );
        let ws = self.upgrade(&path).await?;
        let (mut tx, mut rx) = ws.split();
        let mut ping = tokio::time::interval(KEEPALIVE);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await; // the first tick fires immediately

        loop {
            tokio::select! {
                msg = rx.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        let meta: Handoff = serde_json::from_str(text.as_str())
                            .context("parsing the conduit handoff frame")?;
                        if meta.event != "accepted" {
                            bail!("conduit sent an unexpected handoff event: {}", meta.event);
                        }
                        if meta.peer_device_id.is_empty() {
                            bail!("conduit handed off a peer without a device id");
                        }
                        let ws = tx.reunite(rx).map_err(|_| {
                            anyhow::anyhow!("reuniting the parked accept websocket")
                        })?;
                        return Ok(Accept {
                            peer_device_id: meta.peer_device_id,
                            stream: BoxStream::new(WsByteStream::new(ws)),
                        });
                    }
                    Some(Ok(Message::Binary(_))) => {
                        bail!("conduit sent binary data before the handoff metadata")
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        tx.send(Message::Pong(payload)).await.ok();
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) | None => {
                        bail!("conduit closed the accept connection before a handoff")
                    }
                    Some(Err(e)) => return Err(e).context("waiting for a conduit handoff"),
                },
                _ = ping.tick() => {
                    if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                        bail!("conduit stopped answering on the control socket");
                    }
                }
            }
        }
    }

    /// Subscribe to `GET /v1/events`.
    ///
    /// Returns the initial snapshot plus a watch channel fed by every later
    /// push (peer online/offline, RTT, virtual ip, and `service 16` listener
    /// counts). The channel closes when conduit goes away.
    pub async fn watch(&self) -> Result<tokio::sync::watch::Receiver<Option<Status>>> {
        let mut ws = self.upgrade("/v1/events").await?;
        let initial = loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Some(status) = parse_status_event(text.as_str()) {
                        break status;
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    ws.send(Message::Pong(payload)).await.ok();
                }
                Some(Ok(Message::Binary(_)))
                | Some(Ok(Message::Pong(_)))
                | Some(Ok(Message::Frame(_))) => {}
                Some(Ok(Message::Close(_))) | None => {
                    bail!("conduit closed /v1/events before the first snapshot")
                }
                Some(Err(e)) => return Err(e).context("reading the first /v1/events frame"),
            }
        };

        let (tx, rx) = tokio::sync::watch::channel(Some(initial));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    frame = ws.next() => match frame {
                        Some(Ok(Message::Text(text))) => {
                            if let Some(status) = parse_status_event(text.as_str()) {
                                if tx.send(Some(status)).is_err() {
                                    break;
                                }
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            ws.send(Message::Pong(payload)).await.ok();
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            debug!(error = %e, "/v1/events stream ended");
                            break;
                        }
                        None => break,
                    },
                }
            }
            let _ = tx.send(None);
        });
        Ok(rx)
    }

    /// `GET /v1/devices`.
    pub async fn devices(&self) -> Result<DevicesReply> {
        self.get_json("/v1/devices").await
    }

    /// `GET /v1/status`.
    pub async fn status(&self) -> Result<Status> {
        self.get_json("/v1/status").await
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let mut stream = self.connect().await?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;

        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("conduit response timed out for {path}");
            }
            let step = remaining.min(Duration::from_secs(10));
            let n = tokio::time::timeout(step, stream.read(&mut buf))
                .await
                .map_err(|_| anyhow::anyhow!("conduit read timed out for {path}"))??;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            if raw.len() > MAX_RESPONSE {
                bail!("conduit response exceeded {MAX_RESPONSE} bytes");
            }
        }
        let response = parse_http_response(&raw)?;
        if response.status != 200 {
            bail!(
                "conduit returned {} for {path}: {}",
                response.status,
                String::from_utf8_lossy(&response.body).trim()
            );
        }
        serde_json::from_slice(&response.body)
            .with_context(|| format!("parsing the JSON from {path}"))
    }
}

fn parse_status_event(text: &str) -> Option<Status> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("type")?.as_str()? != "status" {
        return None;
    }
    serde_json::from_value(value.get("status")?.clone()).ok()
}

#[derive(Debug, Deserialize)]
struct Handoff {
    #[serde(default)]
    event: String,
    #[serde(default)]
    peer_device_id: String,
}

/// `AgentDialer` backed by the conduit control socket.
///
/// The daemon name is passed through to conduit, which resolves it against the
/// account's active devices and guarantees the peer's identity with mTLS.
#[derive(Debug, Clone)]
pub struct ConduitDialer {
    client: ConduitClient,
}

impl ConduitDialer {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            client: ConduitClient::new(socket),
        }
    }

    pub fn from_env() -> Self {
        Self {
            client: ConduitClient::from_env(),
        }
    }

    pub fn client(&self) -> &ConduitClient {
        &self.client
    }
}

impl AgentDialer for ConduitDialer {
    fn open_stream<'a>(
        &'a self,
        peer: &'a str,
        _service_id: u16,
    ) -> BoxFuture<'a, Result<BoxStream>> {
        Box::pin(async move { self.client.open(peer).await })
    }
}

// --------------------------------------------------------------------- HTTP

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn parse_http_response(raw: &[u8]) -> Result<HttpResponse> {
    let head_end = find(raw, b"\r\n\r\n").context("malformed HTTP response from conduit")?;
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .with_context(|| format!("bad HTTP status line: {status_line}"))?;

    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_ascii_lowercase();
        if name == "transfer-encoding" && value.contains("chunked") {
            chunked = true;
        } else if name == "content-length" {
            content_length = value.parse::<usize>().ok();
        }
    }

    let mut body = raw[head_end + 4..].to_vec();
    if chunked {
        body = decode_chunked(&body)?;
    } else if let Some(len) = content_length {
        body.truncate(len.min(body.len()));
    }
    Ok(HttpResponse { status, body })
}

const MAX_CHUNKED_BODY: usize = 16 * 1024 * 1024;
const MAX_CHUNK_HEX_DIGITS: usize = 8;

fn decode_chunked(data: &[u8]) -> Result<Vec<u8>> {
    let mut rest = data;
    let mut out = Vec::new();
    loop {
        let Some(end) = find(rest, b"\r\n") else {
            bail!("truncated chunk header");
        };
        let line = std::str::from_utf8(&rest[..end])?;
        let token = line.split(';').next().unwrap_or("0").trim();
        if token.is_empty()
            || token.len() > MAX_CHUNK_HEX_DIGITS
            || !token.bytes().all(|b| b.is_ascii_hexdigit())
        {
            bail!("bad chunk size");
        }
        let size = usize::from_str_radix(token, 16).context("bad chunk size")?;
        let header_end = end.checked_add(2).context("chunk header overflow")?;
        if rest.len() < header_end {
            bail!("truncated chunk header");
        }
        rest = &rest[header_end..];
        if size == 0 {
            break;
        }
        let body_end = size.checked_add(2).context("chunk length overflow")?;
        if rest.len() < body_end {
            bail!("truncated chunk body");
        }
        let new_len = out
            .len()
            .checked_add(size)
            .context("chunk length overflow")?;
        if new_len > MAX_CHUNKED_BODY {
            bail!("chunked body exceeded {MAX_CHUNKED_BODY} bytes");
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[body_end..];
    }
    Ok(out)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_control_socket_like_the_contract_says() {
        // The env-dependent path is covered by `from_env` at runtime; this pins
        // the documented fallback shape.
        let path = default_control_socket();
        assert!(path.ends_with("control.sock"), "{path:?}");
    }

    #[test]
    fn parses_a_content_length_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        let response = parse_http_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"{}");
    }

    #[test]
    fn parses_a_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        let response = parse_http_response(raw).unwrap();
        assert_eq!(response.body, b"{}");
    }

    #[test]
    fn huge_chunk_length_is_an_explicit_error() {
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\n0\r\n\r\n";
        let Err(err) = parse_http_response(raw) else {
            panic!("oversized chunk length was accepted");
        };
        let text = format!("{err:#}");
        assert!(
            text.contains("chunk") || text.contains("bad chunk"),
            "{text}"
        );
    }

    #[test]
    fn parses_status_events_and_ignores_other_types() {
        let text = r#"{"type":"status","status":{"device_id":"a","device_name":"workstation","tun":{"enabled":true},"peers":[{"device_id":"b","name":"laptop"}],"services":[{"service_id":16,"name":"envoy","online":true,"listeners":3}]}}"#;
        let status = parse_status_event(text).expect("status frame");
        assert_eq!(status.device_id, "a");
        assert_eq!(status.peers[0].name, "laptop");
        assert_eq!(status.services[0].listeners, 3);
        assert!(parse_status_event(r#"{"type":"other"}"#).is_none());
        assert!(parse_status_event("not json").is_none());
    }

    #[test]
    fn encodes_targets_for_the_path() {
        assert_eq!(encode_component("laptop"), "laptop");
        assert_eq!(encode_component("100.64.0.2"), "100.64.0.2");
        assert_eq!(encode_component("my laptop"), "my%20laptop");
    }
}
