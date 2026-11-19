//! Shared SSE framing.
//!
//! Bytes stay buffered until a newline so a multibyte UTF-8 character split
//! across TCP chunks is decoded as one line, not replaced with U+FFFD.

const MAX_EVENT_BYTES: usize = 1024 * 1024;

pub(crate) struct SseEvent {
    pub(crate) event: String,
    pub(crate) data: String,
}

#[derive(Default)]
pub(crate) struct SseParser {
    buf: Vec<u8>,
    pos: usize,
    cur_event: String,
    cur_data: Vec<String>,
    cur_bytes: usize,
    overflow: bool,
}

impl SseParser {
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        self.drain(false)
    }

    /// Emit a trailing event that never received a blank line.
    pub(crate) fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = self.drain(true);
        if self.overflow {
            self.reset_event();
            return out;
        }
        if let Some(ev) = self.emit() {
            out.push(ev);
        }
        out
    }

    fn drain(&mut self, eof: bool) -> Vec<SseEvent> {
        let mut out = Vec::new();
        loop {
            let rest = &self.buf[self.pos..];
            if rest.len() > MAX_EVENT_BYTES && !rest.contains(&b'\n') {
                self.buf.clear();
                self.pos = 0;
                self.overflow = true;
                self.reset_event();
                break;
            }
            let Some(rel) = rest.iter().position(|b| *b == b'\n') else {
                if eof && !rest.is_empty() {
                    let line = rest.to_vec();
                    self.pos = self.buf.len();
                    self.take_line(&line, &mut out);
                }
                break;
            };
            let end = self.pos + rel;
            let mut line = self.buf[self.pos..end].to_vec();
            self.pos = end + 1;
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.take_line(&line, &mut out);
        }
        self.compact();
        out
    }

    fn take_line(&mut self, line: &[u8], out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if self.overflow {
                self.reset_event();
                return;
            }
            if let Some(ev) = self.emit() {
                out.push(ev);
            }
            return;
        }
        if self.overflow
            || line.len() > MAX_EVENT_BYTES
            || self.cur_bytes.saturating_add(line.len()) > MAX_EVENT_BYTES
        {
            self.overflow = true;
            return;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            return;
        };
        if let Some(rest) = text.strip_prefix("event:") {
            self.cur_event = one_optional_space(rest).to_string();
        } else if let Some(rest) = text.strip_prefix("data:") {
            let payload = one_optional_space(rest);
            self.cur_bytes = self.cur_bytes.saturating_add(payload.len());
            self.cur_data.push(payload.to_string());
        }
    }

    fn emit(&mut self) -> Option<SseEvent> {
        if self.cur_data.is_empty() {
            self.cur_event.clear();
            self.cur_bytes = 0;
            return None;
        }
        let event = SseEvent {
            event: std::mem::take(&mut self.cur_event),
            data: std::mem::take(&mut self.cur_data).join("\n"),
        };
        self.cur_bytes = 0;
        Some(event)
    }

    fn reset_event(&mut self) {
        self.cur_event.clear();
        self.cur_data.clear();
        self.cur_bytes = 0;
        self.overflow = false;
    }

    fn compact(&mut self) {
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
            return;
        }
        if self.pos >= 8192 && self.pos * 2 >= self.buf.len() {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
    }
}

/// RFC 8895: ignore one optional space after the field colon. Do not trim.
fn one_optional_space(value: &str) -> &str {
    value.strip_prefix(' ').unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_handles_chunked_events() {
        let mut p = SseParser::default();
        assert!(p.feed(b"event: response.output_text.delta\n").is_empty());
        assert!(p.feed(b"data: {\"delta\":\"hel").is_empty());
        let evs = p.feed(b"lo\"}\n\ndata: [DONE]\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].event, "response.output_text.delta");
        assert_eq!(evs[0].data, "{\"delta\":\"hello\"}");
        assert_eq!(evs[1].data, "[DONE]");
    }

    #[test]
    fn sse_parser_reassembles_utf8_fed_one_byte_at_a_time() {
        let frame = "event: delta\ndata: 你好🙂 保留\n\n";
        let mut p = SseParser::default();
        let mut evs = Vec::new();
        for byte in frame.as_bytes() {
            evs.extend(p.feed(&[*byte]));
        }
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event, "delta");
        assert_eq!(evs[0].data, "你好🙂 保留");
        assert!(p.finish().is_empty());
    }

    #[test]
    fn finish_emits_a_trailing_event_without_a_blank_line() {
        let mut p = SseParser::default();
        assert!(p.feed("data: 尾帧\n".as_bytes()).is_empty());
        let evs = p.finish();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "尾帧");
        assert!(p.finish().is_empty());
    }

    #[test]
    fn data_field_strips_only_one_optional_space() {
        let mut p = SseParser::default();
        let evs = p.feed("data:  indented\n\n".as_bytes());
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, " indented");
        let evs = p.feed("data:nospace\n\n".as_bytes());
        assert_eq!(evs[0].data, "nospace");
    }

    #[test]
    fn oversized_line_is_discarded() {
        let mut p = SseParser::default();
        let big = vec![b'a'; MAX_EVENT_BYTES + 8];
        assert!(p.feed(&big).is_empty());
        let evs = p.feed(b"\n\ndata: ok\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "ok");
    }
}
