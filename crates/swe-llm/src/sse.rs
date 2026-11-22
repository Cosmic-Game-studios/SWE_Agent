//! Incremental Server-Sent Events decoder.
//!
//! Feed it arbitrary byte chunks; it yields complete events. Handles `\n`,
//! `\r\n` and `\r` line endings, multi-line `data:` fields, comments and
//! UTF-8 sequences split across chunks.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// Value of the `event:` field, empty if absent.
    pub event: String,
    /// `data:` lines joined with `\n`.
    pub data: String,
}

#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    event: String,
    data: String,
    has_data: bool,
    /// A `\r` ended the previous chunk; a leading `\n` belongs to it.
    pending_cr: bool,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add bytes and return every event completed by them.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for &byte in chunk {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(&mut events),
                b'\r' => {
                    self.pending_cr = true;
                    self.end_line(&mut events);
                }
                other => self.buffer.push(other),
            }
        }
        events
    }

    /// Flush an event that was not terminated by a blank line before EOF.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let mut sink = Vec::new();
            self.end_line(&mut sink);
        }
        self.take_event()
    }

    fn end_line(&mut self, events: &mut Vec<SseEvent>) {
        let line = std::mem::take(&mut self.buffer);
        if line.is_empty() {
            if let Some(event) = self.take_event() {
                events.push(event);
            }
            return;
        }
        let line = String::from_utf8_lossy(&line);
        if line.starts_with(':') {
            return; // comment / keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {} // id, retry and unknown fields are irrelevant here
        }
    }

    fn take_event(&mut self) -> Option<SseEvent> {
        let event = std::mem::take(&mut self.event);
        let data = std::mem::take(&mut self.data);
        let had_data = std::mem::replace(&mut self.has_data, false);
        if !had_data && event.is_empty() {
            return None;
        }
        Some(SseEvent { event, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::new();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(decoder.push(chunk));
        }
        out.extend(decoder.finish());
        out
    }

    #[test]
    fn parses_named_events_and_plain_data() {
        let events = decode_all(&[b"event: ping\ndata: {\"a\":1}\n\ndata: [DONE]\n\n"]);
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: "ping".into(),
                    data: "{\"a\":1}".into()
                },
                SseEvent {
                    event: "".into(),
                    data: "[DONE]".into()
                },
            ]
        );
    }

    #[test]
    fn joins_multi_line_data_and_skips_comments() {
        let events = decode_all(&[b": OPENROUTER PROCESSING\n\ndata: one\ndata: two\n\n"]);
        assert_eq!(
            events,
            vec![SseEvent {
                event: "".into(),
                data: "one\ntwo".into()
            }]
        );
    }

    #[test]
    fn survives_arbitrary_chunk_boundaries() {
        let wire = "event: delta\r\ndata: {\"text\":\"h\u{e9}llo \u{1F600}\"}\r\n\r\ndata: tail\r\n\r\n".as_bytes();
        let whole = decode_all(&[wire]);
        assert_eq!(whole.len(), 2);
        for split in 1..wire.len() {
            let (a, b) = wire.split_at(split);
            assert_eq!(decode_all(&[a, b]), whole, "split at byte {split}");
        }
        let bytewise: Vec<&[u8]> = wire.chunks(1).collect();
        assert_eq!(decode_all(&bytewise), whole);
    }

    #[test]
    fn flushes_unterminated_final_event() {
        let events = decode_all(&[b"data: last"]);
        assert_eq!(
            events,
            vec![SseEvent {
                event: "".into(),
                data: "last".into()
            }]
        );
    }

    #[test]
    fn data_without_space_and_empty_data_lines() {
        let events = decode_all(&[b"data:x\n\ndata:\ndata: y\n\n"]);
        assert_eq!(events[0].data, "x");
        assert_eq!(events[1].data, "\ny");
    }
}
