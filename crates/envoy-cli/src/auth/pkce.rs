//! PKCE, URL encoding, and the ChatGPT account id inside an id token.
use anyhow::{anyhow, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> Result<Pkce> {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut raw = [0u8; 43];
    getrandom::fill(&mut raw).map_err(|e| anyhow!("rng failed: {e}"))?;
    let verifier: String = raw
        .iter()
        .map(|&b| CHARSET[(b as usize) % CHARSET.len()] as char)
        .collect();
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    Ok(Pkce {
        verifier,
        challenge,
    })
}

pub fn random_state() -> Result<String> {
    let mut raw = [0u8; 24];
    getrandom::fill(&mut raw).map_err(|e| anyhow!("rng failed: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(raw))
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn extract_query_param(query: &str, key: &str) -> Option<String> {
    let q = query.split('?').nth(1).unwrap_or(query);
    let q = q.split('#').next().unwrap_or(q);
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn extract_chatgpt_account_id(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let val: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    if let Some(id) = val.get("chatgpt_account_id").and_then(|v| v.as_str()) {
        return Some(id.to_string());
    }
    if let Some(id) = val
        .get("https://api.openai.com/auth")
        .and_then(|o| o.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
    {
        return Some(id.to_string());
    }
    val.get("organizations")
        .and_then(|o| o.as_array())
        .and_then(|a| a.first())
        .and_then(|o| o.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

pub fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
