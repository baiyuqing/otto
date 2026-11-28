//! The session messages whose text is not in the next provider request.
//!
//! [`not_in_request`] compares the messages on the active session path with
//! the message items of the projected next request
//! ([`super::context_report::ContextReport::sections`]). A message is in the
//! request when an item carries the same rendered text. The comparison is by
//! content, so it holds for both checkpoint forms (anchor entry and retained
//! tail), for context edits and for the historical Bash output projection,
//! without reimplementing any of them.
//!
//! The comparison is against the projected next request, not against the last
//! request that was sent: the session state at an earlier send is not
//! recorded. Memory records left out by the recall budget, and tool output
//! that was sent only within its own turn, are not session messages and are
//! not compared. A compaction summary that a later checkpoint replaced is not
//! listed either.
//!
//! Ownership: input is borrowed, results are owned. The functions perform no
//! I/O. Errors: a session entry that cannot be converted to messages is a
//! [`PiError`].

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::model::{Message, Role};
use crate::session::compaction::compaction_aware_path;
use crate::session::context::pi_entry_to_context_messages;
use crate::session::pi::PiEntry;
use crate::session::{PiError, active_context_path, index_context_entries};

use super::context_estimate::estimate_message;
use super::context_report::{ContextSection, SectionKind, message_label, message_text};
use super::summary_details::{
    USER_MESSAGE_MAXIMUM_RUNES, USER_MESSAGE_TRUNCATION_MARKER, compaction_user_messages,
};

/// The most text, in bytes, that [`NotInRequest::items`] carries together.
/// Older items give up their text first.
pub const NOT_IN_REQUEST_TEXT_MAXIMUM_BYTES: usize = 8 * 1024 * 1024;

/// The session messages on the active path that the next request does not
/// contain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotInRequest {
    /// In session order.
    pub items: Vec<ExcludedItem>,
    /// Summarized user messages whose full text is in the summary's
    /// `<user-messages>` block. They are in the request and not listed.
    pub carried_user_messages: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedItem {
    /// `session #<1-based ordinal among the path's messages> <role>`, plus the
    /// tool name for a tool call or result.
    pub label: String,
    /// The same estimate the request items carry.
    pub tokens: i64,
    pub reason: ExclusionReason,
    /// `None` when the text was left out of the report for size, see
    /// [`NOT_IN_REQUEST_TEXT_MAXIMUM_BYTES`].
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionReason {
    /// A compaction checkpoint replaced the message with its summary.
    Summarized,
    /// A user message the summary's `<user-messages>` block carries cut after
    /// [`USER_MESSAGE_MAXIMUM_RUNES`] characters.
    UserMessageTruncated,
    /// A user message the summary's `<user-messages>` block does not carry,
    /// because the block is limited to
    /// [`super::summary_details::USER_MESSAGES_MAXIMUM_BYTES`].
    UserMessageDropped,
    /// The message is on the context path, but the request projection changed
    /// or left out its text: a context edit, the historical Bash output
    /// summary, or an opaque provider-held checkpoint.
    Projected,
}

/// [`not_in_request`] for the whole entry list of a session file, with the
/// leaf the store resolves: the last entry.
pub fn not_in_request_for_entries(
    entries: &[PiEntry],
    sections: &[ContextSection],
) -> Result<NotInRequest, PiError> {
    let (index, _) = index_context_entries(entries)?;
    let leaf = entries.last().map(|entry| entry.id.as_str());
    let path = active_context_path(entries, leaf.unwrap_or_default(), &index)?;
    not_in_request(&path, sections)
}

/// A message of the active path and whether the compaction-aware path keeps
/// its entry.
struct Candidate {
    message: Message,
    text: String,
    ordinal: usize,
    kept: bool,
}

/// The messages of `path`, the active path in order, whose text no message
/// item of `sections` has.
///
/// Two passes match the messages of the entries the checkpoint keeps before
/// the others, so an identical older text cannot take the match of a kept
/// message. Of several messages with the same text, only as many as the
/// request has items count as in the request, taken in session order; which
/// of equal messages the projection removed is not recorded.
pub fn not_in_request(
    path: &[PiEntry],
    sections: &[ContextSection],
) -> Result<NotInRequest, PiError> {
    let mut request: HashMap<&str, usize> = HashMap::new();
    for section in sections {
        if matches!(
            section.kind,
            SectionKind::Messages | SectionKind::CompactionSummary
        ) {
            for item in &section.items {
                *request.entry(item.text.as_str()).or_default() += 1;
            }
        }
    }

    let retained = compaction_aware_path(path)?;
    let kept_ids: HashSet<&str> = retained
        .iter()
        .filter(|entry| entry.type_name != "compaction")
        .map(|entry| entry.id.as_str())
        .collect();

    let mut candidates = Vec::new();
    let mut ordinal = 0;
    for entry in path.iter().filter(|entry| entry.type_name != "compaction") {
        let kept = kept_ids.contains(entry.id.as_str());
        // A summarized entry was valid when it was appended; one that no
        // longer converts contributes nothing instead of failing the report.
        let messages = if kept {
            pi_entry_to_context_messages(entry)?
        } else {
            pi_entry_to_context_messages(entry).unwrap_or_default()
        };
        for message in messages {
            ordinal += 1;
            let text = message_text(&message);
            if text.trim().is_empty() {
                continue;
            }
            candidates.push(Candidate {
                message,
                text,
                ordinal,
                kept,
            });
        }
    }

    let mut excluded = vec![false; candidates.len()];
    for pass_kept in [true, false] {
        for (candidate, excluded) in candidates.iter().zip(&mut excluded) {
            if candidate.kept != pass_kept {
                continue;
            }
            match request.get_mut(candidate.text.as_str()) {
                Some(count) if *count > 0 => *count -= 1,
                _ => *excluded = true,
            }
        }
    }

    let mut carried = latest_carried_user_messages(path);
    let mut carried_user_messages = 0_u32;
    let mut items = Vec::new();
    for (candidate, _) in candidates.into_iter().zip(excluded).filter(|(_, out)| *out) {
        let reason = if candidate.kept {
            ExclusionReason::Projected
        } else {
            match classify_summarized(&candidate.message, &mut carried) {
                Some(reason) => reason,
                None => {
                    carried_user_messages = carried_user_messages.saturating_add(1);
                    continue;
                }
            }
        };
        items.push(ExcludedItem {
            label: format!(
                "session {}",
                message_label(candidate.ordinal - 1, &candidate.message)
            ),
            tokens: estimate_message(&candidate.message),
            reason,
            text: Some(candidate.text),
        });
    }
    cap_text(&mut items);
    Ok(NotInRequest {
        items,
        carried_user_messages,
    })
}

/// The lines of the newest checkpoint's `<user-messages>` block, with how
/// often each occurs.
fn latest_carried_user_messages(path: &[PiEntry]) -> HashMap<String, usize> {
    let mut lines = HashMap::new();
    let summary = path
        .iter()
        .rev()
        .find(|entry| entry.type_name == "compaction")
        .and_then(|entry| entry.compaction.as_deref())
        .map(|compaction| compaction.summary.as_str());
    for line in summary.map(compaction_user_messages).unwrap_or_default() {
        *lines.entry(line).or_default() += 1;
    }
    lines
}

/// The reason a message before the checkpoint boundary is not in the request,
/// or `None` when it is a user message whose full text the summary carries.
/// A matching line is used up, so equal messages are matched one to one.
fn classify_summarized(
    message: &Message,
    carried: &mut HashMap<String, usize>,
) -> Option<ExclusionReason> {
    let text = message.text();
    if message.role != Role::User || text.trim().is_empty() {
        return Some(ExclusionReason::Summarized);
    }
    if take_line(carried, &text) {
        return None;
    }
    if let Some((end, _)) = text.char_indices().nth(USER_MESSAGE_MAXIMUM_RUNES) {
        let cut = format!("{}\n{USER_MESSAGE_TRUNCATION_MARKER}", &text[..end]);
        if take_line(carried, &cut) {
            return Some(ExclusionReason::UserMessageTruncated);
        }
    }
    Some(ExclusionReason::UserMessageDropped)
}

fn take_line(lines: &mut HashMap<String, usize>, line: &str) -> bool {
    match lines.get_mut(line) {
        Some(count) if *count > 0 => {
            *count -= 1;
            true
        }
        _ => false,
    }
}

/// Keeps the text of the newest items while the total fits in
/// [`NOT_IN_REQUEST_TEXT_MAXIMUM_BYTES`]. The first item that does not fit and
/// every older one lose their text.
fn cap_text(items: &mut [ExcludedItem]) {
    let mut remaining = NOT_IN_REQUEST_TEXT_MAXIMUM_BYTES;
    let mut fits = true;
    for item in items.iter_mut().rev() {
        let length = item.text.as_deref().map_or(0, str::len);
        if fits && length <= remaining {
            remaining -= length;
        } else {
            fits = false;
            item.text = None;
        }
    }
}

#[cfg(test)]
mod tests;
