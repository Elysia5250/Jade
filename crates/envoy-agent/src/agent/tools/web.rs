//! SSRF-hardened `webfetch` tool.
//!
//! Blocks loopback, RFC1918 private networks, CGNAT (`100.64.0.0/10`),
//! link-local (`169.254.0.0/16` / `fe80::/10`), IPv6 ULA (`fc00::/7`),
//! TUN Fake-IP (`198.18.0.0/15`), `0.0.0.0/8`, `192.0.0.0/24`, NAT64
//! (`64:ff9b::/96`), IPv4-mapped/compatible IPv6 addresses, non-HTTP(S) schemes,
//! bypasses host proxy settings, binds validated DNS resolutions atomically
//! through [`SafeDnsResolver`], and re-authorizes every redirect hop.

use crate::agent::asks::AskBroker;
use crate::agent::events::AgentEvent;
use crate::permissions::PermissionEngine;
use anyhow::{bail, Context, Result};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::lookup_host;
use tokio::sync::mpsc;

const MAX_BYTES: usize = 512 * 1024;
const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SafeDnsResolver {
    allow_loopback: bool,
}

impl SafeDnsResolver {
    pub(crate) fn new(allow_loopback: bool) -> Self {
        Self { allow_loopback }
    }
}

impl Resolve for SafeDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        let allow_loopback = self.allow_loopback;
        Box::pin(async move {
            let addrs = resolve_and_validate_host(&host, 0, allow_loopback)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

pub(crate) async fn webfetch_tool(
    args: &Value,
    permissions: &Arc<PermissionEngine>,
    asks: &Arc<AskBroker>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<String> {
    webfetch_inner(args, permissions, asks, tx, false).await
}

async fn webfetch_inner(
    args: &Value,
    permissions: &Arc<PermissionEngine>,
    asks: &Arc<AskBroker>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    allow_loopback: bool,
) -> Result<String> {
    let raw_url = args["url"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("missing `url`")?;
    let format = args["format"]
        .as_str()
        .unwrap_or("text")
        .to_ascii_lowercase();
    let timeout_s = args["timeout_s"].as_u64().unwrap_or(20).clamp(1, 60);

    let mut current = reqwest::Url::parse(raw_url).context("invalid URL")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_s))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(Arc::new(SafeDnsResolver::new(allow_loopback)))
        .user_agent(concat!("envoy/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let resp = {
        let mut hops = 0usize;
        loop {
            validate_http_url(&current, allow_loopback)?;
            let resp = client.get(current.clone()).send().await?;
            if resp.status().is_redirection() {
                if hops >= MAX_REDIRECTS {
                    bail!("too many redirects (max {MAX_REDIRECTS})");
                }
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .context("redirect missing Location header")?;
                let next = current.join(loc).context("invalid redirect URL")?;
                validate_http_url(&next, allow_loopback)?;
                permissions
                    .authorize_tool_call("webfetch", &json!({ "url": next.as_str() }), asks, tx)
                    .await?;
                current = next;
                hops += 1;
                continue;
            }
            break resp;
        }
    };

    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let mut bytes = Vec::new();
    let mut stream = resp;
    let mut truncated = false;
    while let Some(chunk) = stream.chunk().await? {
        let room = MAX_BYTES.saturating_sub(bytes.len());
        if room == 0 {
            truncated = true;
            break;
        }
        if chunk.len() <= room {
            bytes.extend_from_slice(&chunk);
        } else {
            bytes.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
    }

    let raw_text = String::from_utf8_lossy(&bytes).to_string();
    let rendered = if format == "html" || !content_type.to_ascii_lowercase().contains("html") {
        raw_text
    } else {
        html_to_text(&raw_text)
    };

    let mut out = format!("HTTP {status} ({content_type}) {}\n\n{rendered}", current);
    if truncated {
        out.push_str(&format!(
            "\n\n[response body truncated at {} KiB]",
            MAX_BYTES / 1024
        ));
    }
    Ok(out)
}

pub(crate) fn validate_http_url(url: &reqwest::Url, allow_loopback: bool) -> Result<()> {
    match url.scheme() {
        "http" | "https" => {}
        other => bail!("unsupported URL scheme `{other}` (only http and https are allowed)"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URLs with embedded credentials are not allowed");
    }
    let host = url.host_str().context("URL missing host")?;
    validate_hostname(host, allow_loopback)?;
    let _ = url.port_or_known_default().context("unknown port")?;
    Ok(())
}

fn validate_hostname(host: &str, allow_loopback: bool) -> Result<()> {
    let bare = host
        .trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']');
    if bare.is_empty() {
        bail!("URL missing host");
    }
    let lower = bare.to_ascii_lowercase();
    if lower.ends_with(".local") || lower.ends_with(".internal") {
        bail!("refusing local/internal host `{host}`");
    }
    if !allow_loopback && (lower == "localhost" || lower.ends_with(".localhost")) {
        bail!("refusing loopback host `{host}`");
    }
    if let Ok(ip) = bare.parse::<IpAddr>() {
        if is_blocked_ip_with_policy(ip, allow_loopback) {
            bail!("SSRF guard blocked non-public IP address `{ip}`");
        }
    }
    Ok(())
}

async fn resolve_and_validate_host(
    host: &str,
    port: u16,
    allow_loopback: bool,
) -> Result<Vec<SocketAddr>> {
    validate_hostname(host, allow_loopback)?;
    let bare = host
        .trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addrs: Vec<SocketAddr> = lookup_host((bare, port))
        .await
        .with_context(|| format!("DNS lookup failed for `{host}`"))?
        .collect();
    if addrs.is_empty() {
        bail!("DNS lookup returned no addresses for `{host}`");
    }
    for addr in &addrs {
        if is_blocked_ip_with_policy(addr.ip(), allow_loopback) {
            bail!(
                "SSRF guard blocked `{host}` resolving to non-public IP `{}`",
                addr.ip()
            );
        }
    }
    Ok(addrs)
}

#[allow(dead_code)]
pub(crate) async fn validate_url_and_resolve(url: &reqwest::Url) -> Result<Vec<SocketAddr>> {
    validate_http_url(url, false)?;
    let host = url.host_str().context("URL missing host")?;
    let port = url.port_or_known_default().context("unknown port")?;
    resolve_and_validate_host(host, port, false).await
}

#[allow(dead_code)]
pub(crate) fn is_blocked_ip(ip: IpAddr) -> bool {
    is_blocked_ip_with_policy(ip, false)
}

pub(crate) fn is_blocked_ip_with_policy(ip: IpAddr, allow_loopback: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4, allow_loopback),
        IpAddr::V6(v6) => is_blocked_v6(v6, allow_loopback),
    }
}

fn is_blocked_v4(v4: Ipv4Addr, allow_loopback: bool) -> bool {
    if v4.is_loopback() {
        return !allow_loopback;
    }
    let o = v4.octets();
    let is_this_network = o[0] == 0; // 0.0.0.0/8
    let is_cgnat = o[0] == 100 && (o[1] & 0b1100_0000 == 64); // 100.64.0.0/10
    let is_ietf_protocol = o[0] == 192 && o[1] == 0 && o[2] == 0; // 192.0.0.0/24
    let is_benchmark_fake_ip = o[0] == 198 && (o[1] & 0xfe) == 18; // 198.18.0.0/15
    let is_reserved = o[0] >= 240; // 240.0.0.0/4
    is_this_network
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
        || is_cgnat
        || is_ietf_protocol
        || is_benchmark_fake_ip
        || is_reserved
}

fn is_blocked_v6(v6: Ipv6Addr, allow_loopback: bool) -> bool {
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return is_blocked_v4(mapped, allow_loopback);
    }
    if v6.is_loopback() {
        return !allow_loopback;
    }
    let s = v6.segments();
    let is_ula = (s[0] & 0xfe00) == 0xfc00; // fc00::/7
    let is_link_local = (s[0] & 0xffc0) == 0xfe80; // fe80::/10
    let is_site_local = (s[0] & 0xffc0) == 0xfec0; // fec0::/10
    let is_nat64 = s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0]; // 64:ff9b::/96
    let is_nat64_local = s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0x0001; // 64:ff9b:1::/48
    let is_6to4 = s[0] == 0x2002; // 2002::/16
    let is_teredo = s[0] == 0x2001 && s[1] == 0x0000; // 2001:0::/32
    let is_doc = s[0] == 0x2001 && s[1] == 0x0db8; // 2001:db8::/32
    let is_v4_compat = s[0..6] == [0, 0, 0, 0, 0, 0]; // ::/96
    v6.is_unspecified()
        || v6.is_multicast()
        || is_ula
        || is_link_local
        || is_site_local
        || is_nat64
        || is_nat64_local
        || is_6to4
        || is_teredo
        || is_doc
        || is_v4_compat
}

pub(crate) fn html_to_text(html: &str) -> String {
    let mut cleaned = strip_block(html, "script");
    cleaned = strip_block(&cleaned, "style");
    cleaned = strip_block(&cleaned, "noscript");

    let mut out = String::with_capacity(cleaned.len() / 2);
    let mut in_tag = false;
    let mut tag_buf = String::new();
    for ch in cleaned.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let tag = tag_buf
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_start_matches('/')
                    .to_ascii_lowercase();
                if matches!(
                    tag.as_str(),
                    "p" | "div"
                        | "br"
                        | "li"
                        | "tr"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "section"
                        | "article"
                        | "blockquote"
                        | "pre"
                ) && !out.ends_with('\n')
                {
                    out.push('\n');
                }
                tag_buf.clear();
            } else {
                tag_buf.push(ch);
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            tag_buf.clear();
        } else {
            out.push(ch);
        }
    }

    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");

    let mut lines = Vec::new();
    let mut blank_run = 0usize;
    for line in decoded.lines() {
        let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !lines.is_empty() {
                lines.push(String::new());
            }
        } else {
            blank_run = 0;
            lines.push(collapsed);
        }
    }
    lines.join("\n").trim().to_string()
}

fn strip_block(input: &str, tag: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(input.len());
    let mut pos = 0usize;
    while let Some(rel_start) = lower[pos..].find(&open) {
        let start = pos + rel_start;
        out.push_str(&input[pos..start]);
        if let Some(rel_end) = lower[start..].find(&close) {
            pos = start + rel_end + close.len();
        } else {
            pos = input.len();
            break;
        }
    }
    out.push_str(&input[pos..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ssrf_blocks_private_loopback_cgnat_fake_ip_and_link_local() {
        for bad in [
            "http://127.0.0.1:8080/admin",
            "http://10.0.0.5/api",
            "http://172.16.1.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://198.18.0.1/",
            "http://198.19.255.254/",
            "http://0.0.0.1/",
            "http://192.0.0.1/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fec0::1]/",
            "http://[fc00::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[64:ff9b::127.0.0.1]/",
            "http://[64:ff9b:1::10.0.0.1]/",
            "http://[::127.0.0.1]/",
            "http://[2002:7f00:1::1]/",
            "http://localhost:3000/",
            "http://localhost./",
            "http://foo.local./",
            "http://metadata.google.internal./",
            "http://user:pass@example.com/",
            "file:///etc/passwd",
        ] {
            let url = reqwest::Url::parse(bad).unwrap();
            assert!(
                validate_url_and_resolve(&url).await.is_err(),
                "expected SSRF block for {bad}"
            );
        }
    }

    #[tokio::test]
    async fn webfetch_reauthorizes_redirects_and_blocks_ssrf_hops() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let dir = std::env::temp_dir().join(format!("envoy-webfetch-test-{}", std::process::id()));
        let data = dir.join("data");
        let work = dir.join("work");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        crate::permissions::rules::save_rule(
            &data,
            &work,
            crate::permissions::RuleScope::User,
            crate::permissions::Effect::Deny,
            "WebFetch(domain:denied.example.com)",
        )
        .unwrap();

        let eng = PermissionEngine::new(
            data,
            work,
            "s-wf".into(),
            "local".into(),
            crate::permissions::PermissionMode::Auto,
        );
        let asks = AskBroker::new_headless();
        let (tx, _rx) = mpsc::unbounded_channel();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let loc = if req.contains("GET /to-denied") {
                    "https://denied.example.com/secret"
                } else {
                    "http://169.254.169.254/latest/meta-data/"
                };
                let resp = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let denied_hop = webfetch_inner(
            &json!({ "url": format!("http://{addr}/to-denied") }),
            &eng,
            &asks,
            &tx,
            true,
        )
        .await
        .unwrap_err();
        assert!(
            denied_hop.to_string().contains("permission denied"),
            "{denied_hop:#}"
        );

        let ssrf_hop = webfetch_inner(
            &json!({ "url": format!("http://{addr}/to-meta") }),
            &eng,
            &asks,
            &tx,
            true,
        )
        .await
        .unwrap_err();
        assert!(ssrf_hop.to_string().contains("SSRF guard"), "{ssrf_hop:#}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn html_to_text_strips_scripts_and_tags() {
        let html = "<html><head><style>body{color:red}</style><script>alert(1)</script></head>\
                    <body><h1>Hello</h1><p>World &amp; <b>Envoy</b></p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("World & Envoy"));
        assert!(!text.contains("alert(1)"));
        assert!(!text.contains("color:red"));
    }
}
