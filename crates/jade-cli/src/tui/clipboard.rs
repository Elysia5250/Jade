//! Clipboard image/text reading and dropped image path detection for the TUI composer.

use base64::Engine as _;
use jade_agent::session::ImageAttachment;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];
const CLIPBOARD_CMD_TIMEOUT: Duration = Duration::from_secs(2);

fn run_cmd_timeout(bin: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout
            .take((MAX_IMAGE_BYTES + 1) as u64)
            .read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let buf = reader.join().ok()?;
                return status.success().then_some(buf);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

pub(crate) fn image_from_bytes(
    bytes: &[u8],
    hint_mime: Option<&str>,
    name: Option<String>,
) -> Option<(ImageAttachment, usize)> {
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let mime = sniff_image_mime(bytes).or_else(|| {
        hint_mime
            .filter(|m| IMAGE_MIMES.contains(m))
            .map(str::to_string)
    })?;
    let data_base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some((
        ImageAttachment {
            mime,
            data_base64,
            name,
        },
        bytes.len(),
    ))
}

fn sniff_image_mime(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png".into())
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg".into())
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif".into())
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp".into())
    } else {
        None
    }
}

pub(crate) fn try_load_image_path(raw: &str, cwd: &Path) -> Option<(ImageAttachment, usize)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\n') || trimmed.contains('\r') {
        return None;
    }
    let unquoted = strip_quotes(trimmed);
    let path_str = if let Some(uri) = unquoted.strip_prefix("file://") {
        uri.replace("%20", " ")
    } else {
        unquoted.to_string()
    };
    let mut path = expand_path(&path_str, cwd)?;
    if !path.is_file() && unquoted.len() == trimmed.len() && path_str.contains('\\') {
        let unescaped = unescape_posix_path(&path_str);
        if let Some(alt) = expand_path(&unescaped, cwd) {
            if alt.is_file() {
                path = alt;
            }
        }
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)?;
    let hint_mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    };
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_IMAGE_BYTES as u64 {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string);
    image_from_bytes(&bytes, Some(hint_mime), name)
}

fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2
        && ((s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn unescape_posix_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            } else {
                out.push('\\');
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn expand_path(raw: &str, cwd: &Path) -> Option<PathBuf> {
    if raw == "~" {
        return std::env::var_os("HOME").map(PathBuf::from);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest));
    }
    let p = Path::new(raw);
    Some(if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    })
}

pub(crate) fn read_clipboard_image() -> Option<(ImageAttachment, usize)> {
    #[cfg(target_os = "macos")]
    {
        try_macos_image()
    }
    #[cfg(not(target_os = "macos"))]
    {
        try_wl_paste_image().or_else(try_xclip_image)
    }
}

#[cfg(not(target_os = "macos"))]
fn try_wl_paste_image() -> Option<(ImageAttachment, usize)> {
    let list = run_cmd_timeout("wl-paste", &["--list-types"], CLIPBOARD_CMD_TIMEOUT)?;
    let types = String::from_utf8_lossy(&list);
    let mime = IMAGE_MIMES
        .iter()
        .copied()
        .find(|m| types.lines().any(|line| line.trim() == *m))?;
    let bytes = run_cmd_timeout(
        "wl-paste",
        &["--no-newline", "--type", mime],
        CLIPBOARD_CMD_TIMEOUT,
    )?;
    image_from_bytes(&bytes, Some(mime), Some("clipboard".into()))
}

#[cfg(not(target_os = "macos"))]
fn try_xclip_image() -> Option<(ImageAttachment, usize)> {
    let list = run_cmd_timeout(
        "xclip",
        &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        CLIPBOARD_CMD_TIMEOUT,
    )?;
    let targets = String::from_utf8_lossy(&list);
    let mime = IMAGE_MIMES
        .iter()
        .copied()
        .find(|m| targets.lines().any(|line| line.trim() == *m))?;
    let bytes = run_cmd_timeout(
        "xclip",
        &["-selection", "clipboard", "-t", mime, "-o"],
        CLIPBOARD_CMD_TIMEOUT,
    )?;
    image_from_bytes(&bytes, Some(mime), Some("clipboard".into()))
}

#[cfg(target_os = "macos")]
fn try_macos_image() -> Option<(ImageAttachment, usize)> {
    if let Some(bytes) = run_cmd_timeout("pngpaste", &["-"], CLIPBOARD_CMD_TIMEOUT) {
        if let Some(res) = image_from_bytes(&bytes, Some("image/png"), Some("clipboard".into())) {
            return Some(res);
        }
    }
    for (class_tag, mime) in [("PNGf", "image/png"), ("JPEG", "image/jpeg")] {
        let script = format!("set d to the clipboard as «class {class_tag}»");
        let Some(out) = run_cmd_timeout(
            "osascript",
            &["-e", &script, "-e", "return d"],
            CLIPBOARD_CMD_TIMEOUT,
        ) else {
            continue;
        };
        let hex_str = String::from_utf8_lossy(&out);
        let prefix = format!("«data {class_tag}");
        let Some(hex) = hex_str
            .trim()
            .strip_prefix(&prefix)
            .and_then(|s| s.strip_suffix('»'))
        else {
            continue;
        };
        if let Some(bytes) = decode_hex(hex) {
            if let Some(res) = image_from_bytes(&bytes, Some(mime), Some("clipboard".into())) {
                return Some(res);
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

pub(crate) fn read_clipboard_text() -> Option<String> {
    #[cfg(target_os = "macos")]
    let cmds: &[(&str, &[&str])] = &[("pbpaste", &[])];
    #[cfg(not(target_os = "macos"))]
    let cmds: &[(&str, &[&str])] = &[
        ("wl-paste", &["--no-newline"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ];
    for (bin, args) in cmds {
        if let Some(bytes) = run_cmd_timeout(bin, args, CLIPBOARD_CMD_TIMEOUT) {
            if !bytes.is_empty() {
                if let Ok(text) = String::from_utf8(bytes) {
                    return Some(text);
                }
            }
        }
    }
    None
}

pub(crate) fn format_image_label(img: &ImageAttachment, raw_bytes: Option<usize>) -> String {
    let approx = raw_bytes.unwrap_or_else(|| (img.data_base64.len() * 3) / 4);
    let kb = approx.div_ceil(1024).max(1);
    let ext = img.mime.strip_prefix("image/").unwrap_or("img");
    match &img.name {
        Some(name) if name != "clipboard" => format!("{name} ({kb} KB)"),
        _ => format!("{kb} KB {ext}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_png_and_rejects_oversized_images() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0u8; 32]);
        let (img, size) = image_from_bytes(&png, None, None).unwrap();
        assert_eq!(img.mime, "image/png");
        assert_eq!(size, 40);

        let too_big = vec![0u8; MAX_IMAGE_BYTES + 1];
        assert!(image_from_bytes(&too_big, Some("image/png"), None).is_none());
    }

    #[test]
    fn try_load_image_path_handles_file_uri_and_quotes() {
        let dir = std::env::temp_dir().join(format!("jade-clip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let img_path = dir.join("shot.png");
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[1, 2, 3, 4]);
        std::fs::write(&img_path, &png).unwrap();

        let uri = format!("'file://{}'", img_path.display());
        let (att, size) = try_load_image_path(&uri, &dir).unwrap();
        assert_eq!(att.mime, "image/png");
        assert_eq!(att.name.as_deref(), Some("shot.png"));
        assert_eq!(size, png.len());

        assert!(try_load_image_path("not an image.txt", &dir).is_none());

        let spaced = dir.join("Screenshot 2026 (1).png");
        std::fs::write(&spaced, &png).unwrap();
        let escaped = spaced
            .display()
            .to_string()
            .replace(' ', "\\ ")
            .replace('(', "\\(")
            .replace(')', "\\)");
        let (att2, size2) = try_load_image_path(&escaped, &dir).unwrap();
        assert_eq!(att2.mime, "image/png");
        assert_eq!(att2.name.as_deref(), Some("Screenshot 2026 (1).png"));
        assert_eq!(size2, png.len());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn run_cmd_timeout_kills_hanging_process() {
        let started = Instant::now();
        let res = run_cmd_timeout("sh", &["-c", "sleep 10"], Duration::from_millis(80));
        assert!(res.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout helper waited too long: {:?}",
            started.elapsed()
        );
    }
}
