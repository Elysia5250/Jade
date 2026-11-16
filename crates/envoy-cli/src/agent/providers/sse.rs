//! Shared SSE framing.

pub(crate) struct SseEvent {
    pub(crate) event: String,
    pub(crate) data: String,
}

#[derive(Default)]
pub(crate) struct SseParser {
    buf: String,
    cur_event: String,
    cur_data: Vec<String>,
}

impl SseParser {
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find('\n') {
            let line = self.buf[..pos].trim_end_matches('\r').to_string();
            self.buf.drain(..=pos);
            if line.is_empty() {
                if !self.cur_data.is_empty() {
                    out.push(SseEvent {
                        event: std::mem::take(&mut self.cur_event),
                        data: std::mem::take(&mut self.cur_data).join("\n"),
                    });
                } else {
                    self.cur_event.clear();
                }
            } else if let Some(rest) = line.strip_prefix("event:") {
                self.cur_event = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                self.cur_data.push(rest.trim_start().to_string());
            }
        }
        out
    }
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

}
