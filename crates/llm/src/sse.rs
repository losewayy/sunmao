//! Hand-rolled Server-Sent Events parser.
//!
//! SSE is a line protocol: fields are `name: value` lines terminated by `\n`,
//! events are terminated by a blank line, `:`-prefixed lines are comments
//! (keepalives). Multiple `data:` lines in one event join with `\n`.
//!
//! The parser is fed arbitrary byte slices and yields complete events; chunk
//! boundaries are handled by buffering until `\n`.

#[derive(Debug, Clone, PartialEq)]
pub enum SseEvent {
    /// `data:` payload(s) joined; event type defaults to "message".
    Message { data: String },
    /// Explicitly typed event (`event: foo`).
    Event { event: String, data: String },
    /// `:` comment/keepalive line.
    Comment,
}

#[derive(Default)]
pub struct SseParser {
    /// Raw bytes not yet terminated by `\n`.
    buf: Vec<u8>,
    /// `data:` lines collected for the in-flight event.
    data: Vec<String>,
    /// `event:` override for the in-flight event.
    event_type: Option<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a network chunk; returns all events completed by it.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            // strip \n and optional \r
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(ev) = self.on_line(&line) {
                out.push(ev);
            }
        }
        out
    }

    /// Process one complete line; Some(event) when a blank line dispatched.
    fn on_line(&mut self, line: &[u8]) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line[0] == b':' {
            return Some(SseEvent::Comment);
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(i) => {
                let field = &line[..i];
                let mut value = &line[i + 1..];
                if value.first() == Some(&b' ') {
                    value = &value[1..];
                }
                (field, value)
            }
            // "bare field name" = field with empty value
            None => (&line[..], &[][..]),
        };
        match field {
            b"data" => self.data.push(String::from_utf8_lossy(value).into_owned()),
            b"event" => self.event_type = Some(String::from_utf8_lossy(value).into_owned()),
            // id: / retry: parsed but unused for chat streams
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() {
            self.event_type = None;
            return None;
        }
        let data = std::mem::take(&mut self.data).join("\n");
        let ev = match self.event_type.take() {
            None => SseEvent::Message { data },
            Some(event) => SseEvent::Event { event, data },
        };
        Some(ev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_data_event() {
        let mut p = SseParser::new();
        let evs = p.feed(b"data: hello\n\n");
        assert_eq!(
            evs,
            vec![SseEvent::Message {
                data: "hello".into()
            }]
        );
    }

    #[test]
    fn joins_multiline_data_and_typed_event() {
        let mut p = SseParser::new();
        let evs = p.feed(b"event: delta\ndata: line1\ndata: line2\n\n");
        assert_eq!(
            evs,
            vec![SseEvent::Event {
                event: "delta".into(),
                data: "line1\nline2".into()
            }]
        );
    }

    #[test]
    fn splits_across_chunk_boundaries() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: hel").is_empty());
        let evs = p.feed(b"lo\n\nda");
        assert_eq!(
            evs,
            vec![SseEvent::Message {
                data: "hello".into()
            }]
        );
        let evs = p.feed(b"ta: bye\n\n");
        assert_eq!(
            evs,
            vec![SseEvent::Message {
                data: "bye".into()
            }]
        );
    }

    #[test]
    fn comments_and_done_marker() {
        let mut p = SseParser::new();
        let evs = p.feed(b": keepalive\n\ndata: [DONE]\n\n");
        assert_eq!(
            evs,
            vec![
                SseEvent::Comment,
                SseEvent::Message {
                    data: "[DONE]".into()
                }
            ]
        );
    }

    #[test]
    fn strips_crlf() {
        let mut p = SseParser::new();
        let evs = p.feed(b"data: hi\r\n\r\n");
        assert_eq!(
            evs,
            vec![SseEvent::Message {
                data: "hi".into()
            }]
        );
    }
}
