//! Server-Sent Events framing.
//!
//! One frame is `id: <seq>\nevent: <type>\ndata: <json>\n\n`, written by
//! `writeSSEFrame` in `internal/server/server.go` and read by `parseFrames`
//! in `ui/src/sse.ts`.

use serde::{Deserialize, Serialize};

/// One decoded frame. `id` is absent when the frame carried no `id:` line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub id: Option<i64>,
    pub event: String,
    pub data: String,
}

/// What [`parse_frames`] returns: the complete frames in `buf` and the
/// trailing bytes that are not yet a whole frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedFrames {
    pub frames: Vec<Frame>,
    pub rest: String,
}

/// Writes one frame exactly as the Go server does.
pub fn format_frame(seq: i64, event: &str, data: &str) -> String {
    format!("id: {seq}\nevent: {event}\ndata: {data}\n\n")
}

/// Splits `buf` on the blank line between frames. A block with no `event:`
/// line is dropped, matching `parseFrame`'s `return frame.event ? frame : null`.
pub fn parse_frames(buf: &str) -> ParsedFrames {
    let mut frames = Vec::new();
    let mut start = 0usize;
    while let Some(offset) = buf[start..].find("\n\n") {
        let end = start + offset;
        if let Some(frame) = parse_frame(&buf[start..end]) {
            frames.push(frame);
        }
        start = end + 2;
    }
    ParsedFrames {
        frames,
        rest: buf[start..].to_string(),
    }
}

fn parse_frame(block: &str) -> Option<Frame> {
    let mut id = None;
    let mut event = String::new();
    let mut data: Vec<&str> = Vec::new();
    for line in block.split('\n') {
        if let Some(rest) = line.strip_prefix("id: ") {
            // `Number("x")` is NaN in the TypeScript reader; Rust has no NaN
            // for an integer field, so an unparsable id reads as absent.
            id = rest.trim().parse::<i64>().ok();
        } else if let Some(rest) = line.strip_prefix("event: ") {
            event = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("data: ") {
            data.push(rest);
        }
    }
    if event.is_empty() {
        return None;
    }
    Some(Frame {
        id,
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_complete_frames_and_keeps_a_partial_one() {
        let parsed =
            parse_frames("id: 0\nevent: text_delta\ndata: {\"a\":1}\n\nid: 1\nevent: agent_fin");
        assert_eq!(
            parsed.frames,
            vec![Frame {
                id: Some(0),
                event: "text_delta".into(),
                data: "{\"a\":1}".into(),
            }]
        );
        assert_eq!(parsed.rest, "id: 1\nevent: agent_fin");
    }

    #[test]
    fn drops_a_block_without_an_event_line() {
        let parsed = parse_frames("id: 7\ndata: {}\n\n");
        assert!(parsed.frames.is_empty());
        assert_eq!(parsed.rest, "");
    }

    #[test]
    fn joins_repeated_data_lines_with_newlines() {
        let parsed = parse_frames("event: e\ndata: one\ndata: two\n\n");
        assert_eq!(parsed.frames[0].data, "one\ntwo");
        assert_eq!(parsed.frames[0].id, None);
    }

    #[test]
    fn format_frame_matches_the_go_writer() {
        assert_eq!(
            format_frame(3, "text_delta", "{\"text\":\"hi\"}"),
            "id: 3\nevent: text_delta\ndata: {\"text\":\"hi\"}\n\n"
        );
    }
}
