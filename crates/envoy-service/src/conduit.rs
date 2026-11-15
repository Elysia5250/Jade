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

use crate::server::AgentServer;
use crate::transport::{AgentDialer, BoxStream};
use anyhow::{bail, Context, Result};
use futures::future::BoxFuture;
use futures::{Sink, SinkExt, Stream, StreamExt};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{client_async, WebSocketStream};
use tracing::{debug, info};

/// The service id envoy registers with conduit.
pub const AGENT_SERVICE_ID: u16 = envoy_proto::AGENT_SERVICE_ID;
/// Name reported in `/v1/services/16/accept?name=...`.
pub const SERVICE_NAME: &str = "envoy";
/// Largest accepted control-socket response body.
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
/// Buffered bytes before a WebSocket write applies backpressure.
const WS_WRITE_BUFFER: usize = 256 * 1024;
/// Idle accept connections `envoy serve` keeps parked.
pub const DEFAULT_POOL: usize = 3;

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
                let reason = serde_json::from_str::<serde_json::Value>(&raw)
                    .ok()
                    .and_then(|v| v.get("error")?.as_str().map(str::to_string))
                    .unwrap_or_else(|| raw.trim().to_string());
                let reason = if reason.is_empty() {
                    status.to_string()
                } else {
                    format!("{} ({})", reason, status.as_u16())
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
    pub async fn accept(&self) -> Result<Accept> {
        let path = format!(
            "/v1/services/{}/accept?name={}",
            AGENT_SERVICE_ID, SERVICE_NAME
        );
        let mut ws = self.upgrade(&path).await?;
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    let meta: Handoff = serde_json::from_str(text.as_str())
                        .context("parsing the conduit handoff frame")?;
                    if meta.event != "accepted" {
                        bail!("conduit sent an unexpected handoff event: {}", meta.event);
                    }
                    if meta.peer_device_id.is_empty() {
                        bail!("conduit handed off a peer without a device id");
                    }
                    return Ok(Accept {
                        peer_device_id: meta.peer_device_id,
                        stream: BoxStream::new(WsByteStream::new(ws)),
                    });
                }
                Some(Ok(Message::Binary(_))) => {
                    bail!("conduit sent binary data before the handoff metadata")
                }
                Some(Ok(Message::Ping(payload))) => {
                    ws.send(Message::Pong(payload)).await.ok();
                }
                Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                Some(Ok(Message::Close(_))) | None => {
                    bail!("conduit closed the accept connection before a handoff")
                }
                Some(Err(e)) => return Err(e).context("waiting for a conduit handoff"),
            }
        }
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
        loop {
            let n = stream.read(&mut buf).await?;
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

    /// Reverse-map a TUN peer address to a device id (`/v1/devices`).
    ///
    /// The contract requires this before trusting a TCP peer on the virtual
    /// network: the address is only meaningful after it has been mapped back to
    /// a device and checked against the allowlist.
    pub async fn device_for_ip(&self, ip: std::net::IpAddr) -> Result<String> {
        let reply = self.devices().await?;
        let wanted = ip.to_string();
        reply
            .devices
            .into_iter()
            .find(|d| d.virtual_ip.as_deref() == Some(wanted.as_str()))
            .map(|d| d.device_id)
            .with_context(|| format!("no conduit device owns the virtual ip {ip}"))
    }
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

// ---------------------------------------------------------------- WebSocket

/// A WebSocket carrying a transparent byte stream.
///
/// Binary frames are payload; text frames are ignored; ping is answered; close
/// is EOF. Writes are buffered up to [`WS_WRITE_BUFFER`] so a slow peer applies
/// backpressure instead of growing memory.
pub struct WsByteStream<S> {
    inner: WebSocketStream<S>,
    read: Vec<u8>,
    read_pos: usize,
    write: Vec<u8>,
    closed: bool,
    shutdown: bool,
}

impl<S> WsByteStream<S> {
    pub fn new(inner: WebSocketStream<S>) -> Self {
        Self {
            inner,
            read: Vec::new(),
            read_pos: 0,
            write: Vec::new(),
            closed: false,
            shutdown: false,
        }
    }
}

fn ws_err(e: WsError) -> io::Error {
    match e {
        WsError::Io(e) => e,
        other => io::Error::new(io::ErrorKind::Other, other.to_string()),
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> WsByteStream<S> {
    /// Push the write buffer into the socket and flush it.
    fn poll_send_buffer(&mut self, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        if self.write.is_empty() {
            return Pin::new(&mut self.inner).poll_flush(cx).map_err(ws_err);
        }
        match Pin::new(&mut self.inner).poll_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(ws_err(e))),
            Poll::Ready(Ok(())) => {}
        }
        let chunk = std::mem::take(&mut self.write);
        if let Err(e) = Pin::new(&mut self.inner).start_send(Message::Binary(chunk.into())) {
            return Poll::Ready(Err(ws_err(e)));
        }
        match Pin::new(&mut self.inner).poll_flush(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(ws_err(e))),
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WsByteStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.read_pos < this.read.len() {
                let n = (this.read.len() - this.read_pos).min(buf.remaining());
                buf.put_slice(&this.read[this.read_pos..this.read_pos + n]);
                this.read_pos += n;
                return Poll::Ready(Ok(()));
            }
            if this.closed {
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(Message::Binary(data)))) => {
                    this.read = data.to_vec();
                    this.read_pos = 0;
                }
                Poll::Ready(Some(Ok(Message::Text(_))))
                | Poll::Ready(Some(Ok(Message::Pong(_))))
                | Poll::Ready(Some(Ok(Message::Frame(_)))) => {}
                Poll::Ready(Some(Ok(Message::Ping(payload)))) => {
                    // tungstenite queues the pong but does not flush it for us.
                    let _ = Pin::new(&mut this.inner).start_send(Message::Pong(payload));
                    let _ = Pin::new(&mut this.inner).poll_flush(cx);
                }
                Poll::Ready(Some(Ok(Message::Close(_)))) | Poll::Ready(None) => {
                    this.closed = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(ws_err(e))),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsByteStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.shutdown || this.closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "websocket stream closed",
            )));
        }
        if this.write.len() >= WS_WRITE_BUFFER {
            match this.poll_send_buffer(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {}
            }
        }
        this.write.extend_from_slice(buf);
        // Best effort: a pending socket is fine because the bytes are buffered
        // and the cap above is what applies backpressure.
        let _ = this.poll_send_buffer(cx);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_send_buffer(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.shutdown {
            return Poll::Ready(Ok(()));
        }
        if let Poll::Pending = this.poll_send_buffer(cx) {
            return Poll::Pending;
        }
        let _ = Pin::new(&mut this.inner).start_send(Message::Close(None));
        let flushed = Pin::new(&mut this.inner).poll_flush(cx).map_err(ws_err);
        if flushed.is_ready() {
            this.shutdown = true;
        }
        flushed
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

fn decode_chunked(data: &[u8]) -> Result<Vec<u8>> {
    let mut rest = data;
    let mut out = Vec::new();
    loop {
        let Some(end) = find(rest, b"\r\n") else {
            bail!("truncated chunk header");
        };
        let line = std::str::from_utf8(&rest[..end])?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("0").trim(), 16)
            .context("bad chunk size")?;
        rest = &rest[end + 2..];
        if size == 0 {
            break;
        }
        if rest.len() < size + 2 {
            bail!("truncated chunk body");
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
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
    fn encodes_targets_for_the_path() {
        assert_eq!(encode_component("laptop"), "laptop");
        assert_eq!(encode_component("100.64.0.2"), "100.64.0.2");
        assert_eq!(encode_component("my laptop"), "my%20laptop");
    }
}
