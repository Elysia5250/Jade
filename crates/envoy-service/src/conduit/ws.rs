//! WebSocket framed as a transparent byte stream.

use futures::{Sink, Stream};
use std::io;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::WebSocketStream;

/// Buffered bytes before a WebSocket write applies backpressure.
const WS_WRITE_BUFFER: usize = 256 * 1024;

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
    close_sent: bool,
    pending_pong: Option<Vec<u8>>,
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
            close_sent: false,
            pending_pong: None,
        }
    }
}

fn ws_err(e: WsError) -> io::Error {
    match e {
        WsError::Io(e) => e,
        other => io::Error::other(other.to_string()),
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
        if let Err(e) = Pin::new(&mut self.inner).start_send(Message::Binary(chunk.clone().into()))
        {
            self.write = chunk;
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
        if let Poll::Ready(Err(e)) = this.poll_pong(cx) {
            return Poll::Ready(Err(e));
        } else if this.pending_pong.is_some() {
            return Poll::Pending;
        }
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
                    this.pending_pong = Some(payload.to_vec());
                    match this.poll_pong(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(())) => {}
                    }
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
        let before = this.write.len();
        this.write.extend_from_slice(buf);
        match this.poll_send_buffer(cx) {
            Poll::Ready(Err(e)) => {
                if this.write.len() >= before + buf.len() {
                    this.write.truncate(before);
                }
                Poll::Ready(Err(e))
            }
            Poll::Pending | Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.len())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_send_buffer(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.shutdown {
            return Poll::Ready(Ok(()));
        }
        match this.poll_send_buffer(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {}
        }
        if !this.close_sent {
            match Pin::new(&mut this.inner).poll_ready(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(ws_err(e))),
                Poll::Ready(Ok(())) => {
                    if let Err(e) = Pin::new(&mut this.inner).start_send(Message::Close(None)) {
                        return Poll::Ready(Err(ws_err(e)));
                    }
                    this.close_sent = true;
                }
            }
        }
        match Pin::new(&mut this.inner).poll_flush(cx).map_err(ws_err) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {
                this.shutdown = true;
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> WsByteStream<S> {
    fn poll_pong(&mut self, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let Some(payload) = self.pending_pong.clone() else {
            return Poll::Ready(Ok(()));
        };
        match Pin::new(&mut self.inner).poll_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(ws_err(e))),
            Poll::Ready(Ok(())) => {}
        }
        if let Err(e) = Pin::new(&mut self.inner).start_send(Message::Pong(payload.into())) {
            return Poll::Ready(Err(ws_err(e)));
        }
        self.pending_pong = None;
        Pin::new(&mut self.inner).poll_flush(cx).map_err(ws_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn poll_write_returns_the_underlying_error() {
        let (client, server) = tokio::io::duplex(64);
        let server_task = tokio::spawn(async move {
            let mut socket = tokio_tungstenite::accept_async(server).await.unwrap();
            let _ = socket.next().await;
            drop(socket);
        });
        let (mut socket, _) = tokio_tungstenite::client_async("ws://localhost/", client)
            .await
            .unwrap();
        socket
            .send(Message::Binary(b"hi".to_vec().into()))
            .await
            .unwrap();
        server_task.await.unwrap();
        let mut stream = WsByteStream::new(socket);
        let err = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            stream.write_all(b"payload-after-peer-drop").await
        })
        .await
        .expect("write hung")
        .expect_err("a broken websocket write must be Err, not Ok(len)");
        let _ = err;
    }
}
