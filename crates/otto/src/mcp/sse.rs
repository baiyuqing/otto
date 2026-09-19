//! WHATWG Server-Sent Events (SSE) format parser.
//!
//! Per spec: lines end with `\n`, `\r\n` or `\r`; `data:` lines accumulate joined by `\n`;
//! `event:` sets the name; `id:` and `retry:` are ignored; comments (lines starting with `:`)
//! are ignored; an empty line dispatches when data is non-empty. UTF-8 BOM at start is skipped.

/// One SSE event as parsed from the stream.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    /// The event name (from `event:` lines), if present.
    pub event: Option<String>,
    /// The accumulated data (from `data:` lines, joined by `\n`).
    pub data: String,
}

/// Parses SSE format from byte chunks. Accumulates partial lines and emits events on empty lines.
pub struct SseParser {
    max_event_bytes: usize,
    buffer: Vec<u8>,
    event_name: Option<String>,
    event_data: Vec<String>,
    event_bytes: usize,
}

impl SseParser {
    /// Create a new parser with the given maximum event size in bytes.
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            buffer: Vec::new(),
            event_name: None,
            event_data: Vec::new(),
            event_bytes: 0,
        }
    }

    /// Feed a chunk of bytes and return any complete events parsed so far.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, String> {
        self.buffer.extend_from_slice(chunk);

        // Skip UTF-8 BOM at the start
        if self.buffer.starts_with(&[0xEF, 0xBB, 0xBF]) {
            self.buffer.drain(0..3);
        }

        let mut events = Vec::new();

        loop {
            // Find the next line ending
            let line_end = self.buffer.iter().position(|&b| b == b'\n' || b == b'\r');

            let consumed = match line_end {
                Some(pos) if self.buffer[pos] == b'\r' && pos + 1 == self.buffer.len() => {
                    // A lone `\r` at the very end of the buffered bytes is
                    // ambiguous: the next `feed` call may deliver the `\n`
                    // that completes a `\r\n` terminator split across a
                    // chunk boundary. Hold it back rather than dispatching
                    // early on a line that has not actually ended yet.
                    break;
                }
                Some(pos) => {
                    let is_cr = self.buffer[pos] == b'\r';
                    let line_len = pos;
                    let end_len = if is_cr {
                        if pos + 1 < self.buffer.len() && self.buffer[pos + 1] == b'\n' {
                            2
                        } else {
                            1
                        }
                    } else {
                        1
                    };

                    // Convert line to UTF-8
                    let line = std::str::from_utf8(&self.buffer[..line_len])
                        .map_err(|_| "invalid utf-8 in sse event".to_string())?;

                    // Process the line
                    if line.is_empty() {
                        // Empty line: dispatch event if data is non-empty
                        if !self.event_data.is_empty() {
                            let data = self.event_data.join("\n");
                            events.push(SseEvent {
                                event: self.event_name.take(),
                                data,
                            });
                            self.event_data.clear();
                            self.event_bytes = 0;
                        }
                    } else if !line.starts_with(':') {
                        // Parse field: value pairs (skip comments)
                        let (field, value) = if let Some(colon_pos) = line.find(':') {
                            let field = &line[..colon_pos];
                            let rest = &line[colon_pos + 1..];
                            // Strip leading space after colon if present
                            let value = rest.strip_prefix(' ').unwrap_or(rest);
                            (field, value)
                        } else {
                            (line, "")
                        };

                        match field {
                            "event" => {
                                self.event_name = Some(value.to_string());
                            }
                            "data" => {
                                let bytes_added =
                                    value.len() + if self.event_data.is_empty() { 0 } else { 1 }; // +1 for \n separator
                                if self.event_bytes + bytes_added > self.max_event_bytes {
                                    return Err(format!(
                                        "sse event exceeds {} bytes",
                                        self.max_event_bytes
                                    ));
                                }
                                self.event_bytes += bytes_added;
                                self.event_data.push(value.to_string());
                            }
                            "id" | "retry" => {
                                // Ignored per spec
                            }
                            _ => {
                                // Unknown fields are ignored
                            }
                        }
                    }

                    line_len + end_len
                }
                None => {
                    // No line terminator anywhere in the buffered bytes. A
                    // stream that never sends one would otherwise grow this
                    // buffer without bound, since the per-event size check
                    // below only runs once a `data:` line is fully parsed.
                    if self.buffer.len() > self.max_event_bytes {
                        return Err(format!("sse event exceeds {} bytes", self.max_event_bytes));
                    }
                    break;
                }
            };

            self.buffer.drain(..consumed);
        }

        Ok(events)
    }

    /// Finish parsing: dispatch any trailing event without a final blank line.
    pub fn finish(mut self) -> Result<Option<SseEvent>, String> {
        // A trailing lone `\r` held back by `feed` (in case a `\n` was still
        // to come) is now known final: the stream ended, so it terminates
        // whatever line preceded it.
        if self.buffer.last() == Some(&b'\r') {
            self.buffer.pop();
        }

        // Check for remaining data
        if !self.buffer.is_empty() {
            let line = std::str::from_utf8(&self.buffer)
                .map_err(|_| "invalid utf-8 in sse event".to_string())?;

            if !line.is_empty() && !line.starts_with(':') {
                // Process last line
                if let Some(colon_pos) = line.find(':') {
                    let field = &line[..colon_pos];
                    let rest = &line[colon_pos + 1..];
                    let value = rest.strip_prefix(' ').unwrap_or(rest);

                    if field == "event" {
                        self.event_name = Some(value.to_string());
                    } else if field == "data" {
                        let bytes_added =
                            value.len() + if self.event_data.is_empty() { 0 } else { 1 };
                        if self.event_bytes + bytes_added > self.max_event_bytes {
                            return Err(format!(
                                "sse event exceeds {} bytes",
                                self.max_event_bytes
                            ));
                        }
                        self.event_data.push(value.to_string());
                    }
                } else if !line.starts_with(':') {
                    // Treat as data without explicit field
                    self.event_data.push(line.to_string());
                }
            }
        }

        // Dispatch trailing event if data is non-empty
        if !self.event_data.is_empty() {
            let data = self.event_data.join("\n");
            Ok(Some(SseEvent {
                event: self.event_name,
                data,
            }))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_event_with_crlf() {
        let mut parser = SseParser::new(10000);
        let events = parser.feed(b"data: hello\r\n\r\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
        assert_eq!(events[0].event, None);
    }

    #[test]
    fn test_single_event_with_lf() {
        let mut parser = SseParser::new(10000);
        let events = parser.feed(b"data: world\n\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "world");
    }

    #[test]
    fn test_single_event_with_cr() {
        let mut parser = SseParser::new(10000);
        // The trailing `\r` of the blank line is ambiguous within `feed`
        // (a `\n` could still follow in the next chunk), so it is held
        // back and only resolved at `finish`.
        let events = parser.feed(b"data: test\r\r").unwrap();
        assert_eq!(events.len(), 0);
        let result = parser.finish().unwrap();
        assert_eq!(result.unwrap().data, "test");
    }

    #[test]
    fn test_crlf_split_across_chunks_does_not_dispatch_early() {
        // Regression test: a `\r\n` line terminator split across two `feed`
        // calls must not be mistaken for a separate blank line that
        // dispatches the event before all `data:` lines have arrived.
        let mut parser = SseParser::new(10000);
        let events1 = parser.feed(b"data: {\"a\":1\r").unwrap();
        assert_eq!(events1.len(), 0);
        let events2 = parser.feed(b"\ndata: ,\"b\":2}\r\n\r\n").unwrap();
        assert_eq!(events2.len(), 1);
        assert_eq!(events2[0].data, "{\"a\":1\n,\"b\":2}");
    }

    #[test]
    fn test_trailing_lone_cr_at_finish_still_dispatches() {
        let mut parser = SseParser::new(10000);
        let events = parser.feed(b"data: final\r").unwrap();
        assert_eq!(events.len(), 0);
        let result = parser.finish().unwrap();
        assert_eq!(result.unwrap().data, "final");
    }

    #[test]
    fn test_no_terminator_over_cap_is_rejected() {
        let mut parser = SseParser::new(10);
        let result = parser.feed(&[b'x'; 11]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds"));
    }

    #[test]
    fn test_multiline_data() {
        let mut parser = SseParser::new(10000);
        let events = parser.feed(b"data: line1\ndata: line2\n\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn test_event_name() {
        let mut parser = SseParser::new(10000);
        let events = parser.feed(b"event: custom\ndata: payload\n\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, Some("custom".to_string()));
        assert_eq!(events[0].data, "payload");
    }

    #[test]
    fn test_comments_ignored() {
        let mut parser = SseParser::new(10000);
        let events = parser.feed(b": this is a comment\ndata: real\n\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn test_id_and_retry_ignored() {
        let mut parser = SseParser::new(10000);
        let events = parser.feed(b"id: 123\nretry: 5000\ndata: msg\n\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "msg");
    }

    #[test]
    fn test_space_after_colon_stripped() {
        let mut parser = SseParser::new(10000);
        let events = parser.feed(b"data:  spaced\n\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, " spaced");
    }

    #[test]
    fn test_no_space_after_colon() {
        let mut parser = SseParser::new(10000);
        let events = parser.feed(b"data:nospace\n\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "nospace");
    }

    #[test]
    fn test_empty_event_not_dispatched() {
        let mut parser = SseParser::new(10000);
        let events = parser.feed(b"\n\n").unwrap();
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_partial_line_buffered() {
        let mut parser = SseParser::new(10000);
        let events1 = parser.feed(b"data: hel").unwrap();
        assert_eq!(events1.len(), 0);
        let events2 = parser.feed(b"lo\n\n").unwrap();
        assert_eq!(events2.len(), 1);
        assert_eq!(events2[0].data, "hello");
    }

    #[test]
    fn test_line_split_across_chunks() {
        let mut parser = SseParser::new(10000);
        parser.feed(b"data: test").unwrap();
        parser.feed(b"ing\n").unwrap();
        let events = parser.feed(b"\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "testing");
    }

    #[test]
    fn test_bom_skipped() {
        let mut parser = SseParser::new(10000);
        // UTF-8 BOM (EF BB BF) followed by event
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice(b"data: bom\n\n");
        let events = parser.feed(&data).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "bom");
    }

    #[test]
    fn test_size_limit_exceeded() {
        let mut parser = SseParser::new(10);
        let result = parser.feed(b"data: this is a very long message\n\n");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds"));
    }

    #[test]
    fn test_size_limit_multiline() {
        let mut parser = SseParser::new(10);
        parser.feed(b"data: hello\n").unwrap();
        let result = parser.feed(b"data: world\n\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_utf8() {
        let mut parser = SseParser::new(10000);
        let result = parser.feed(b"data: \xFF\xFE\n\n");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("utf-8"));
    }

    #[test]
    fn test_finish_without_blank_line() {
        let mut parser = SseParser::new(10000);
        parser.feed(b"data: final").unwrap();
        let result = parser.finish().unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().data, "final");
    }

    #[test]
    fn test_finish_no_trailing_event() {
        let mut parser = SseParser::new(10000);
        parser.feed(b"data: event1\n\n").unwrap();
        let result = parser.finish().unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_multiple_events() {
        let mut parser = SseParser::new(10000);
        let events = parser
            .feed(b"data: one\n\nevent: two\ndata: msg2\n\ndata: three\n\n")
            .unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].data, "one");
        assert_eq!(events[1].data, "msg2");
        assert_eq!(events[1].event, Some("two".to_string()));
        assert_eq!(events[2].data, "three");
    }

    #[test]
    fn test_chunk_split_mid_line() {
        let mut parser = SseParser::new(10000);
        parser.feed(b"data: te").unwrap();
        parser.feed(b"st\nevent: name\ndata: va").unwrap();
        let events = parser.feed(b"lue\n\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, Some("name".to_string()));
        assert_eq!(events[0].data, "test\nvalue");
    }

    #[test]
    fn test_unknown_field_ignored() {
        let mut parser = SseParser::new(10000);
        let events = parser.feed(b"unknown: field\ndata: real\n\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn test_empty_data_field() {
        let mut parser = SseParser::new(10000);
        let events = parser.feed(b"data:\ndata: value\n\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "\nvalue");
    }
}
