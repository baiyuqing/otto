//! Timestamps, the session snapshot and the preview text of the last user message.

use super::*;

// ---------------------------------------------------------------------------
// timestamps
// ---------------------------------------------------------------------------

/// Parses the RFC 3339 timestamps stored sessions use, with nanosecond
/// precision.
pub fn parse_rfc3339(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value).ok().map(Into::into)
}

/// Formats as RFC 3339 with nanosecond precision: fractional seconds are
/// written only when nonzero, with trailing zeros removed.
pub fn format_rfc3339_nano(timestamp: DateTime<Utc>) -> String {
    let formatted = timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true);
    let Some((head, rest)) = formatted.split_once('.') else {
        return formatted;
    };
    let (fraction, suffix) = rest.split_at(rest.len() - 1);
    let trimmed = fraction.trim_end_matches('0');
    if trimmed.is_empty() {
        format!("{head}{suffix}")
    } else {
        format!("{head}.{trimmed}{suffix}")
    }
}

/// Formats a timestamp for persistence, rejecting the zero value and anything
/// that does not survive a format/parse round trip.
pub fn format_persisted_timestamp(
    timestamp: DateTime<Utc>,
    subject: &str,
) -> Result<String, PiError> {
    if timestamp == zero_time() {
        return Err(PiError::invalid(format!("{subject} timestamp is required")));
    }
    let formatted = format_rfc3339_nano(timestamp);
    match parse_rfc3339(&formatted) {
        Some(round_tripped)
            if round_tripped == timestamp && format_rfc3339_nano(round_tripped) == formatted =>
        {
            Ok(formatted)
        }
        _ => Err(PiError::invalid(format!(
            "{subject} timestamp is outside the RFC3339Nano range"
        ))),
    }
}

// ---------------------------------------------------------------------------
// snapshot and preview
// ---------------------------------------------------------------------------

/// Builds the display snapshot from a session's resolved state.
pub fn snapshot_from_state(
    aggregate_usage: Usage,
    aggregate_usage_present: bool,
    messages: &[Message],
    latest_compaction: Option<&super::super::session_types::CompactionMetadata>,
) -> Snapshot {
    let (context_input_tokens, present, pending) =
        latest_context_input_tokens(messages, latest_compaction);
    Snapshot {
        aggregate_usage,
        aggregate_usage_present,
        context_input_tokens,
        context_input_tokens_present: present,
        context_input_tokens_pending: pending,
    }
}

fn latest_context_input_tokens(
    messages: &[Message],
    latest_compaction: Option<&super::super::session_types::CompactionMetadata>,
) -> (i64, bool, bool) {
    let mut start = 0usize;
    if let Some(compaction) = latest_compaction {
        if compaction.first_post_checkpoint_message_id.is_empty() {
            return (0, false, true);
        }
        match messages
            .iter()
            .position(|message| message.id == compaction.first_post_checkpoint_message_id)
        {
            Some(index) => start = index,
            None => return (0, false, true),
        }
    }
    for message in messages[start..].iter().rev() {
        if message.role != Role::Assistant {
            continue;
        }
        return match message.usage {
            Some(usage) if usage.input_tokens > 0 => (usage.input_tokens, true, false),
            _ => (0, false, false),
        };
    }
    if latest_compaction.is_some() {
        return (0, false, true);
    }
    (0, false, false)
}

const MAX_SESSION_PREVIEW_RUNES: usize = 120;

/// The text of the most recent user message, or empty when there is none.
pub fn last_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(Message::text)
        .unwrap_or_default()
}

/// Collapses whitespace, escapes control characters and truncates to 120
/// display units, appending "..." when anything was dropped.
pub fn preview_text(text: &str) -> String {
    let mut builder = String::with_capacity(MAX_SESSION_PREVIEW_RUNES * 6 + 3);
    let mut used = 0usize;
    let mut wrote_content = false;
    let mut pending_space = false;
    let mut truncated = false;

    for character in text.chars() {
        if character.is_whitespace() {
            pending_space = wrote_content;
            continue;
        }
        let width = preview_rune_width(character);
        let needed = width + usize::from(pending_space);
        if used + needed > MAX_SESSION_PREVIEW_RUNES {
            truncated = true;
            break;
        }
        if pending_space {
            builder.push(' ');
            used += 1;
            pending_space = false;
        }
        write_preview_rune(&mut builder, character);
        used += width;
        wrote_content = true;
    }

    if !wrote_content {
        return String::new();
    }
    if truncated {
        builder.push_str("...");
    }
    builder
}

fn preview_rune_width(character: char) -> usize {
    let value = character as u32;
    if is_go_control(character) {
        if value < 0x100 {
            return 4;
        }
        return 2 + hex_width(value).max(4);
    }
    1
}

fn write_preview_rune(builder: &mut String, character: char) {
    let value = character as u32;
    if is_go_control(character) {
        if value < 0x100 {
            let _ = write!(builder, "\\x{value:02x}");
        } else {
            let width = hex_width(value).max(4);
            let _ = write!(builder, "\\u{value:0width$x}");
        }
        return;
    }
    builder.push(character);
}

/// The C0 and C1 control ranges only.
fn is_go_control(character: char) -> bool {
    let value = character as u32;
    value < 0x20 || (0x7f..=0x9f).contains(&value)
}

fn hex_width(mut value: u32) -> usize {
    let mut width = 1;
    while value >= 16 {
        value /= 16;
        width += 1;
    }
    width
}
