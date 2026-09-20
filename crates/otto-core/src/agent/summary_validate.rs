//! Validation of the summary text a provider returned.
//!
//! A summary is untrusted model output that is about to become the head of the
//! transcript, so it is bounded in bytes, checked for the exact set of headings
//! the prompt asked for, and for the turn-prefix mode stripped of any heading
//! markers the model added anyway.
//!
//! Ownership: every function takes borrowed input and returns owned text.
//!
//! Errors: the `Err` string is the message the caller surfaces. The caller
//! wraps it in [`crate::agent::AgentError::InvalidCompactionSummary`].
//!

use crate::model::{BlockType, FinishReason, Message};

use super::summary::{
    SPLIT_TURN_SUMMARY_SEPARATOR, SUMMARY_MAXIMUM_BYTES, TURN_SUMMARY_MAXIMUM_BYTES,
};

/// The level-2 and level-3 headings a structured summary must contain, each
/// exactly once and in this order.
pub const REQUIRED_SUMMARY_HEADINGS: [&str; 9] = [
    "## Goal",
    "## Constraints & Preferences",
    "## Progress",
    "### Done",
    "### In Progress",
    "### Blocked",
    "## Key Decisions",
    "## Next Steps",
    "## Critical Context",
];

/// Validates a structured summary response: bounded text, no tool call, and
/// the nine required headings in order.
pub fn validate_structured_summary(message: &Message) -> Result<String, String> {
    let summary = validate_summary_message(message, SUMMARY_MAXIMUM_BYTES)?;
    validate_summary_headings(&summary)?;
    Ok(summary)
}

/// Validates a turn-prefix summary response and removes any heading markers
/// the model emitted, which the prompt forbade.
pub fn validate_turn_summary(message: &Message) -> Result<String, String> {
    let summary = validate_summary_message(message, TURN_SUMMARY_MAXIMUM_BYTES)?;
    Ok(sanitize_turn_summary_headings(&summary))
}

/// The checks both summary modes share.
pub fn validate_summary_message(message: &Message, maximum_bytes: usize) -> Result<String, String> {
    if message.finish_reason == Some(FinishReason::ToolCalls) {
        return Err("compaction summary response attempted a tool call".into());
    }
    let mut text = String::new();
    for block in &message.blocks {
        if block.block_type != BlockType::Text {
            return Err(
                "compaction summary response must contain text only and no tool calls".into(),
            );
        }
        text.push_str(&block.text);
    }
    let summary = text.trim();
    if summary.is_empty() {
        return Err("compaction summary response is empty".into());
    }
    if summary.len() > maximum_bytes {
        return Err(format!(
            "compaction summary response exceeds {maximum_bytes} bytes"
        ));
    }
    Ok(summary.to_owned())
}

/// Wraps text in the single-text-block message the validators consume.
fn text_message(text: &str) -> Message {
    Message {
        blocks: vec![crate::model::Block::text(text)],
        ..Message::default()
    }
}

/// Rejects a structured summary whose level-2 and level-3 headings are not
/// exactly [`REQUIRED_SUMMARY_HEADINGS`], in order. Headings inside a fenced
/// code block do not count, so a summary may quote them.
pub fn validate_summary_headings(summary: &str) -> Result<(), String> {
    let mut expected = 0;
    let mut fence = FenceScanner::default();
    for line in normalize_summary_line_endings(summary).split('\n') {
        if fence.consume(line) {
            continue;
        }
        if fence.is_open() || !is_level_two_or_three_heading(line) {
            continue;
        }
        if expected >= REQUIRED_SUMMARY_HEADINGS.len()
            || line != REQUIRED_SUMMARY_HEADINGS[expected]
        {
            return Err("compaction summary has an unexpected or out-of-order heading".into());
        }
        expected += 1;
    }
    if expected != REQUIRED_SUMMARY_HEADINGS.len() {
        return Err(format!(
            "compaction summary has {expected} of {} required headings",
            REQUIRED_SUMMARY_HEADINGS.len()
        ));
    }
    Ok(())
}

/// Removes the `##` and `###` markers from every heading outside a fenced code
/// block, leaving the heading text. A marker with no text after it is left
/// alone, because removing it would leave an empty line.
pub fn sanitize_turn_summary_headings(summary: &str) -> String {
    let mut lines: Vec<String> = summary.split('\n').map(str::to_owned).collect();
    let mut fence = FenceScanner::default();
    for line in &mut lines {
        let normalized = line.trim_end_matches('\r');
        if fence.consume(normalized) {
            continue;
        }
        if fence.is_open() || !is_level_two_or_three_heading(normalized) {
            continue;
        }
        *line = strip_turn_heading_marker(line);
    }
    lines.join("\n")
}

fn strip_turn_heading_marker(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut spaces = 0;
    while spaces < bytes.len() && spaces < 3 && bytes[spaces] == b' ' {
        spaces += 1;
    }
    let trimmed = &line[spaces..];
    let marker_length = if trimmed.starts_with("###") {
        3
    } else if trimmed.starts_with("##") {
        2
    } else {
        return line.to_owned();
    };
    let rest = trimmed[marker_length..].trim_start_matches([' ', '\t']);
    if rest.is_empty() {
        return line.to_owned();
    }
    rest.to_owned()
}

/// Tracks whether the scanner is inside a fenced code block.
#[derive(Default)]
pub(super) struct FenceScanner {
    character: u8,
    length: usize,
}

impl FenceScanner {
    pub(super) fn is_open(&self) -> bool {
        self.character != 0
    }

    /// Applies one line. Returns true when the line was a fence marker and so
    /// is not content.
    pub(super) fn consume(&mut self, line: &str) -> bool {
        let Some((marker, length, closing)) =
            summary_fence_marker(line, self.character, self.length)
        else {
            return false;
        };
        if !self.is_open() && !closing {
            self.character = marker;
            self.length = length;
        } else if self.character == marker && closing {
            self.character = 0;
            self.length = 0;
        }
        true
    }
}

/// Recognizes a Markdown fence line. Returns the fence character, its run
/// length, and whether it closes the currently open fence.
fn summary_fence_marker(line: &str, active: u8, active_length: usize) -> Option<(u8, usize, bool)> {
    let bytes = line.as_bytes();
    let mut spaces = 0;
    while spaces < bytes.len() && spaces < 4 && bytes[spaces] == b' ' {
        spaces += 1;
    }
    if spaces > 3 {
        return None;
    }
    let trimmed = &bytes[spaces..];
    if trimmed.len() < 3 || (trimmed[0] != b'`' && trimmed[0] != b'~') {
        return None;
    }
    let marker = trimmed[0];
    let mut length = 0;
    while length < trimmed.len() && trimmed[length] == marker {
        length += 1;
    }
    if length < 3 {
        return None;
    }
    let remainder = &trimmed[length..];
    if active == 0 {
        // An info string that contains a backtick is not a fence opener.
        if marker == b'`' && remainder.contains(&b'`') {
            return None;
        }
        return Some((marker, length, false));
    }
    if marker == active
        && length >= active_length
        && remainder.iter().all(|byte| *byte == b' ' || *byte == b'\t')
    {
        return Some((marker, length, true));
    }
    None
}

/// Collapses CRLF and lone CR to LF, so heading matching does not depend on
/// the line ending the model used.
pub fn normalize_summary_line_endings(summary: &str) -> String {
    summary.replace("\r\n", "\n").replace('\r', "\n")
}

fn is_level_two_or_three_heading(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut spaces = 0;
    while spaces < bytes.len() && spaces < 3 && bytes[spaces] == b' ' {
        spaces += 1;
    }
    let line = &line[spaces..];
    line == "##"
        || line.starts_with("## ")
        || line.starts_with("##\t")
        || line == "###"
        || line.starts_with("### ")
        || line.starts_with("###\t")
}

/// Joins a validated historical summary and a validated turn summary with the
/// split-turn separator, rejecting a combination that is over the bound.
pub fn combine_summary(historical: &str, turn: &str) -> Result<String, String> {
    let validated_historical = validate_structured_summary(&text_message(historical))?;
    let validated_turn = validate_turn_summary(&text_message(turn))?;
    let combined = validated_historical + SPLIT_TURN_SUMMARY_SEPARATOR + &validated_turn;
    if combined.len() > SUMMARY_MAXIMUM_BYTES {
        return Err(format!(
            "combined compaction summary exceeds {SUMMARY_MAXIMUM_BYTES} bytes"
        ));
    }
    Ok(combined)
}

#[cfg(test)]
mod tests {
    use crate::model::Block;

    use super::*;

    fn structured() -> String {
        REQUIRED_SUMMARY_HEADINGS
            .iter()
            .map(|heading| format!("{heading}\nbody\n"))
            .collect()
    }

    fn message(text: &str) -> Message {
        text_message(text)
    }

    #[test]
    fn a_complete_structured_summary_passes() {
        let summary = validate_structured_summary(&message(&structured())).expect("valid");
        assert!(summary.starts_with("## Goal"));
        assert!(summary.ends_with("body"), "trailing space is trimmed");
    }

    #[test]
    fn a_tool_call_finish_reason_is_rejected() {
        let mut candidate = message(&structured());
        candidate.finish_reason = Some(FinishReason::ToolCalls);
        assert_eq!(
            validate_structured_summary(&candidate).unwrap_err(),
            "compaction summary response attempted a tool call"
        );
    }

    #[test]
    fn a_non_text_block_is_rejected() {
        let candidate = Message {
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_name: "read".into(),
                tool_call_id: "call-1".into(),
                ..Block::default()
            }],
            ..Message::default()
        };
        assert_eq!(
            validate_structured_summary(&candidate).unwrap_err(),
            "compaction summary response must contain text only and no tool calls"
        );
    }

    #[test]
    fn an_empty_or_oversized_summary_is_rejected() {
        assert_eq!(
            validate_structured_summary(&message("   \n\t ")).unwrap_err(),
            "compaction summary response is empty"
        );
        let huge = "x".repeat(SUMMARY_MAXIMUM_BYTES + 1);
        assert_eq!(
            validate_structured_summary(&message(&huge)).unwrap_err(),
            format!("compaction summary response exceeds {SUMMARY_MAXIMUM_BYTES} bytes")
        );
        let huge_turn = "x".repeat(TURN_SUMMARY_MAXIMUM_BYTES + 1);
        assert_eq!(
            validate_turn_summary(&message(&huge_turn)).unwrap_err(),
            format!("compaction summary response exceeds {TURN_SUMMARY_MAXIMUM_BYTES} bytes")
        );
    }

    #[test]
    fn a_missing_heading_is_counted() {
        let partial: String = REQUIRED_SUMMARY_HEADINGS[..4]
            .iter()
            .map(|heading| format!("{heading}\n"))
            .collect();
        assert_eq!(
            validate_structured_summary(&message(&partial)).unwrap_err(),
            "compaction summary has 4 of 9 required headings"
        );
    }

    #[test]
    fn an_out_of_order_or_extra_heading_is_rejected() {
        let mut swapped = REQUIRED_SUMMARY_HEADINGS.to_vec();
        swapped.swap(0, 1);
        let text: String = swapped.iter().map(|line| format!("{line}\n")).collect();
        assert_eq!(
            validate_structured_summary(&message(&text)).unwrap_err(),
            "compaction summary has an unexpected or out-of-order heading"
        );

        let extra = structured() + "## Extra\n";
        assert_eq!(
            validate_structured_summary(&message(&extra)).unwrap_err(),
            "compaction summary has an unexpected or out-of-order heading"
        );
    }

    #[test]
    fn headings_inside_a_fenced_block_are_ignored() {
        let text = format!("```\n## Goal\n## Not A Heading\n```\n{}", structured());
        validate_structured_summary(&message(&text)).expect("the fenced headings do not count");
    }

    #[test]
    fn a_tilde_fence_and_a_longer_closing_run_are_handled() {
        let text = format!("~~~\n## Goal\n~~~~\n{}", structured());
        validate_structured_summary(&message(&text)).expect("the tilde fence closes");
    }

    #[test]
    fn a_backtick_info_string_containing_a_backtick_does_not_open_a_fence() {
        let text = format!("```a`b\n{}", structured());
        validate_structured_summary(&message(&text))
            .expect("the line is not a fence, so the headings after it count");
    }

    #[test]
    fn carriage_returns_do_not_hide_headings() {
        let text: String = REQUIRED_SUMMARY_HEADINGS
            .iter()
            .map(|heading| format!("{heading}\r\nbody\r\n"))
            .collect();
        validate_structured_summary(&message(&text)).expect("CRLF is normalized");
    }

    #[test]
    fn turn_summaries_lose_their_heading_markers() {
        let sanitized = validate_turn_summary(&message(
            "## Request\nbody\n   ### Deep\ntail\n##\n#### four\n",
        ))
        .expect("valid");
        assert_eq!(sanitized, "Request\nbody\nDeep\ntail\n##\n#### four");
    }

    #[test]
    fn turn_summary_headings_inside_a_fence_survive() {
        let sanitized =
            validate_turn_summary(&message("```\n## Kept\n```\n## Stripped")).expect("valid");
        assert_eq!(sanitized, "```\n## Kept\n```\nStripped");
    }

    #[test]
    fn combine_joins_with_the_split_turn_separator() {
        let combined = combine_summary(&structured(), "the turn so far").expect("valid");
        assert!(combined.contains(SPLIT_TURN_SUMMARY_SEPARATOR));
        assert!(combined.ends_with("the turn so far"));
    }

    #[test]
    fn combine_rejects_an_invalid_half() {
        assert_eq!(
            combine_summary("## Goal", "turn").unwrap_err(),
            "compaction summary has 1 of 9 required headings"
        );
        assert_eq!(
            combine_summary(&structured(), "   ").unwrap_err(),
            "compaction summary response is empty"
        );
    }

    #[test]
    fn combine_rejects_an_oversized_result() {
        let padding = "x".repeat(SUMMARY_MAXIMUM_BYTES - structured().len() - 16);
        let historical = structured() + &padding;
        assert_eq!(
            combine_summary(&historical, "turn").unwrap_err(),
            format!("combined compaction summary exceeds {SUMMARY_MAXIMUM_BYTES} bytes")
        );
    }
}
