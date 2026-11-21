//! Length-prefixed postcard framing over any async stream.
//!
//! Layout: `[u32 BE length][magic ENVY][u16 LE version][postcard body]`.
//! The length is rejected before any body allocation. The 6-byte header is
//! checked next, so a version or magic mismatch never decodes the body and
//! never retains it (the leftover bytes are discarded through a stack buffer).

use crate::{VersionMismatch, MAX_FRAME_BYTES, WIRE_HEADER_LEN, WIRE_MAGIC, WIRE_VERSION};
use anyhow::{anyhow, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const READ_CHUNK: usize = 64 * 1024;
const DISCARD_CHUNK: usize = 8 * 1024;

/// Byte length of `msg` once postcard-encoded.
pub fn payload_len<T: Serialize>(msg: &T) -> Result<usize> {
    Ok(postcard::to_stdvec(msg)?.len())
}

pub async fn write_framed<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = postcard::to_stdvec(msg)?;
    let frame_len = WIRE_HEADER_LEN + body.len();
    if frame_len > MAX_FRAME_BYTES {
        return Err(anyhow!("frame too large: {frame_len}"));
    }
    let mut frame = Vec::with_capacity(frame_len);
    frame.extend_from_slice(&WIRE_MAGIC);
    frame.extend_from_slice(&WIRE_VERSION.to_le_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&(frame.len() as u32).to_be_bytes()).await?;
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

/// Read one frame. An error here after a completed handshake means the peer
/// went away (detach), which callers treat as end-of-stream, not failure.
pub async fn read_framed<R, T>(r: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let len = read_frame_len(r).await?;
    let mut header = [0u8; WIRE_HEADER_LEN];
    r.read_exact(&mut header).await?;
    if let Err(err) = check_header(&header) {
        discard(r, len - WIRE_HEADER_LEN).await?;
        return Err(err);
    }
    let body = read_counted(r, len - WIRE_HEADER_LEN).await?;
    decode_body(&body)
}

async fn read_frame_len<R: AsyncRead + Unpin>(r: &mut R) -> Result<usize> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(anyhow!("frame length {len} exceeds max"));
    }
    if len < WIRE_HEADER_LEN {
        return Err(anyhow!(
            "frame length {len} is shorter than the wire header"
        ));
    }
    Ok(len)
}

fn check_header(header: &[u8; WIRE_HEADER_LEN]) -> Result<()> {
    if header[..WIRE_MAGIC.len()] != WIRE_MAGIC {
        return Err(VersionMismatch {
            local_version: WIRE_VERSION,
            peer_version: 0,
        }
        .into());
    }
    let peer = u16::from_le_bytes([header[4], header[5]]);
    if peer != WIRE_VERSION {
        return Err(VersionMismatch {
            local_version: WIRE_VERSION,
            peer_version: peer,
        }
        .into());
    }
    Ok(())
}

fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    let (msg, rest) = postcard::take_from_bytes::<T>(body)?;
    if !rest.is_empty() {
        return Err(anyhow!("frame has {} trailing bytes", rest.len()));
    }
    Ok(msg)
}

/// Fill `len` bytes incrementally up to the verified [`MAX_FRAME_BYTES`] ceiling.
async fn read_counted<R: AsyncRead + Unpin>(r: &mut R, len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len.min(READ_CHUNK));
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut remaining = len;
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        r.read_exact(&mut chunk[..want]).await?;
        out.extend_from_slice(&chunk[..want]);
        remaining -= want;
    }
    Ok(out)
}

/// Drop a rejected body without retaining it.
async fn discard<R: AsyncRead + Unpin>(r: &mut R, mut left: usize) -> Result<()> {
    let mut buf = [0u8; DISCARD_CHUNK];
    while left > 0 {
        let n = left.min(buf.len());
        r.read_exact(&mut buf[..n]).await?;
        left -= n;
    }
    Ok(())
}
