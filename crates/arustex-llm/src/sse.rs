//! A minimal Server-Sent Events parser.

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental parser; feed arbitrary byte chunks and collect complete events.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    max_event_bytes: usize,
}

impl SseParser {
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            ..Default::default()
        }
    }

    /// Feeds a chunk. Returns an error when a single event grows beyond the limit.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, String> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if line.is_empty() {
                if !self.data.is_empty() || self.event.is_some() {
                    out.push(SseEvent {
                        event: self.event.take(),
                        data: std::mem::take(&mut self.data).join("\n"),
                    });
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line.as_str(), ""),
            };
            match field {
                "event" => self.event = Some(value.to_owned()),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
        let pending: usize = self.buffer.len() + self.data.iter().map(String::len).sum::<usize>();
        if self.max_event_bytes > 0 && pending > self.max_event_bytes {
            return Err(format!(
                "stream event exceeds {} bytes",
                self.max_event_bytes
            ));
        }
        Ok(out)
    }

    /// Flushes a trailing event that was not followed by a blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            let _ = self.feed(&rest);
            let _ = self.feed(b"\n");
        }
        if self.data.is_empty() && self.event.is_none() {
            None
        } else {
            Some(SseEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data).join("\n"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_chunks_comments_and_multiline_data() {
        let mut p = SseParser::new(1024);
        let mut events = p.feed(b": ping\nevent: a\ndata: {\"x\"").unwrap();
        assert!(events.is_empty());
        events.extend(p.feed(b":1}\r\n\r\ndata: one\ndata: two\n\n").unwrap());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("a"));
        assert_eq!(events[0].data, "{\"x\":1}");
        assert_eq!(events[1].data, "one\ntwo");
        p.feed(b"data: tail").unwrap();
        assert_eq!(p.finish().unwrap().data, "tail");
    }

    #[test]
    fn rejects_oversized_events() {
        let mut p = SseParser::new(8);
        assert!(p.feed(b"data: 0123456789").is_err());
    }
}
