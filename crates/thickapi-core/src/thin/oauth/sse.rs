//! Minimal server-sent-events framing over a `reqwest` byte stream.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE parser: feed bytes, take complete events.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    /// No frame end starts before this offset; a long frame is scanned once, not per push.
    scanned: usize,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some((frame_len, sep_len)) = find_frame_end(&self.buf, self.scanned) {
            self.scanned = 0;
            let frame: Vec<u8> = self.buf.drain(..frame_len + sep_len).collect();
            let text = match frame.get(..frame_len) {
                Some(slice) => String::from_utf8_lossy(slice),
                None => String::from_utf8_lossy(&frame),
            };
            if let Some(event) = parse_frame(&text) {
                events.push(event);
            }
        }
        // The longest separator is four bytes; its first three may already be buffered.
        self.scanned = self.buf.len().saturating_sub(3);
        events
    }

    /// Bytes held waiting for a frame end; the caller caps it.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Flush a trailing frame without a terminating blank line (stream ended).
    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.buf.is_empty() {
            return None;
        }
        self.scanned = 0;
        let text = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
        parse_frame(&text)
    }
}

fn find_frame_end(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i < buf.len() {
        if buf.get(i) == Some(&b'\n') {
            if buf.get(i + 1) == Some(&b'\n') {
                return Some((i, 2));
            }
            if buf.get(i + 1) == Some(&b'\r') && buf.get(i + 2) == Some(&b'\n') {
                return Some((i, 3));
            }
        }
        if buf.get(i) == Some(&b'\r')
            && buf.get(i + 1) == Some(&b'\n')
            && buf.get(i + 2) == Some(&b'\r')
            && buf.get(i + 3) == Some(&b'\n')
        {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

fn parse_frame(text: &str) -> Option<SseEvent> {
    let mut event = SseEvent::default();
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => event.event = Some(value.to_string()),
            "data" => data_lines.push(value),
            _ => {}
        }
    }
    if data_lines.is_empty() && event.event.is_none() {
        return None;
    }
    event.data = data_lines.join("\n");
    Some(event)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn splits_frames_across_chunks() {
        let mut parser = SseParser::default();
        let a = parser.push(b"event: message_start\ndata: {\"a\":1}\n\nevent: pi");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].event.as_deref(), Some("message_start"));
        assert_eq!(a[0].data, "{\"a\":1}");
        let b = parser.push(b"ng\ndata: {}\r\n\r\n: comment\n\ndata: x\ndata: y\n\n");
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].event.as_deref(), Some("ping"));
        assert_eq!(b[1].data, "x\ny");
        assert!(parser.finish().is_none());
    }

    #[test]
    fn finish_flushes_partial() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"data: [DONE]").is_empty());
        assert_eq!(parser.finish().unwrap().data, "[DONE]");
    }
}
