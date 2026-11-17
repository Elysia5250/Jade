//! SSRF-hardened `webfetch` tool.
//!
//! Blocks loopback, RFC1918 private networks, CGNAT (`100.64.0.0/10`),
//! link-local (`169.254.0.0/16` / `fe80::/10`), IPv6 ULA (`fc00::/7`),
//! IPv4-mapped IPv6 addresses, non-HTTP(S) schemes, and validates every
//! redirect hop before connecting.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::net::lookup_host;

const MAX_BYTES: usize = 512 * 1024;
const MAX_REDIRECTS: usize = 5;

pub(crate) async fn webfetch_tool(args: &Value) -> Result<String> {
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
        .user_agent(concat!("envoy/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let resp = {
        let mut hops = 0usize;
        loop {
            validate_url_and_resolve(&current).await?;
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
                current = current.join(loc).context("invalid redirect URL")?;
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
        out.push_str(&format!("\n\n[response body truncated at {} KiB]", MAX_BYTES / 1024));
    }
    Ok(out)
}

pub(crate) async fn validate_url_and_resolve(url: &reqwest::Url) -> Result<Vec<SocketAddr>> {
    match url.scheme() {
        "http" | "https" => {}
        other => bail!("unsupported URL scheme `{other}` (only http and https are allowed)"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URLs with embedded credentials are not allowed");
    }
    let host = url.host_str().context("URL missing host")?;
    let host_lower = host.to_ascii_lowercase();
    if host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower.ends_with(".local")
        || host_lower.ends_with(".internal")
    {
        bail!("refusing local/internal host `{host}`");
    }
    let port = url.port_or_known_default().context("unknown port")?;

    let bare_host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare_host.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            bail!("SSRF guard blocked non-public IP address `{ip}`");
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let addrs: Vec<SocketAddr> = lookup_host((bare_host, port))
        .await
        .with_context(|| format!("DNS lookup failed for `{host}`"))?
        .collect();
    if addrs.is_empty() {
        bail!("DNS lookup returned no addresses for `{host}`");
    }
    for addr in &addrs {
        if is_blocked_ip(addr.ip()) {
            bail!("SSRF guard blocked `{host}` resolving to non-public IP `{}`", addr.ip());
        }
    }
    Ok(addrs)
}

pub(crate) fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    let is_cgnat = o[0] == 100 && (o[1] & 0b1100_0000 == 64); // 100.64.0.0/10
    let is_bench = o[0] == 198 && (o[1] == 18 || o[1] == 19); // 198.18.0.0/15
    let is_reserved = o[0] >= 240; // 240.0.0.0/4
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
        || is_cgnat
        || is_bench
        || is_reserved
}

fn is_blocked_v6(v6: Ipv6Addr) -> bool {
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return is_blocked_v4(mapped);
    }
    let s = v6.segments();
    let is_ula = (s[0] & 0xfe00) == 0xfc00; // fc00::/7
    let is_link_local = (s[0] & 0xffc0) == 0xfe80; // fe80::/10
    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        || is_ula
        || is_link_local
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
                    "p" | "div" | "br" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                        | "section" | "article" | "blockquote" | "pre"
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
    async fn ssrf_blocks_private_loopback_cgnat_and_link_local() {
        for bad in [
            "http://127.0.0.1:8080/admin",
            "http://10.0.0.5/api",
            "http://172.16.1.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fc00::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://localhost:3000/",
            "file:///etc/passwd",
        ] {
            let url = reqwest::Url::parse(bad).unwrap();
            assert!(
                validate_url_and_resolve(&url).await.is_err(),
                "expected SSRF block for {bad}"
            );
        }
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
