//! The session messages that are not in the next request, as rows of the
//! `/context` panel: the section row, its items, search rows and the document
//! of one item.

use otto_core::agent::context_exclusion::{
    ExcludedItem, ExclusionReason, NOT_IN_REQUEST_TEXT_MAXIMUM_BYTES, NotInRequest,
};

use super::Document;
use super::context::excerpt;
use crate::tui::interaction::Row;
use crate::tui::layout::format_token_count;

/// The tag of the section row's id; see [`section_id`].
pub(super) const SECTION_TAG: &str = "excluded_section";
/// The tag of an item row's id; see [`item_id`]. The request items use the tags
/// `section` and `item`.
pub(super) const ITEM_TAG: &str = "excluded";

pub(super) fn section_id() -> String {
    serde_json::to_string(&(SECTION_TAG, 0_usize, None::<usize>)).unwrap()
}

pub(super) fn item_id(index: usize) -> String {
    serde_json::to_string(&(ITEM_TAG, index, None::<usize>)).unwrap()
}

/// Why the text is not in the request, in a sentence.
pub(super) fn reason_text(reason: ExclusionReason) -> &'static str {
    match reason {
        ExclusionReason::Summarized => "replaced by the compaction summary",
        ExclusionReason::UserMessageTruncated => {
            "user message cut at 2,000 characters in the summary"
        }
        ExclusionReason::UserMessageDropped => "user message dropped from the summary (16 KiB cap)",
        ExclusionReason::Projected => "changed or left out by the request projection",
    }
}

/// The reason as a few words, for a search row.
fn short_reason(reason: ExclusionReason) -> &'static str {
    match reason {
        ExclusionReason::Summarized => "summarized",
        ExclusionReason::UserMessageTruncated => "cut in summary",
        ExclusionReason::UserMessageDropped => "dropped from summary",
        ExclusionReason::Projected => "changed by projection",
    }
}

fn tokens(excluded: &NotInRequest) -> i64 {
    excluded.items.iter().fold(0_i64, |total, item| {
        total.saturating_add(item.tokens.max(0))
    })
}

/// The section row. It has no share of the request: the items are not in it.
pub(super) fn section_row(excluded: &NotInRequest) -> Row {
    let mut detail = String::from(
        "Session messages whose text is not in the next request. Compared with the next \
         request, not with the last sent one.",
    );
    if excluded.carried_user_messages > 0 {
        let count = excluded.carried_user_messages;
        detail.push_str(&format!(
            " {count} summarized user message{} not listed: their full text is in the \
             compaction summary, which the next request contains.",
            if count == 1 { " is" } else { "s are" }
        ));
    }
    Row {
        id: section_id(),
        cells: vec![
            format!("Not in next request ({})", excluded.items.len()),
            format_token_count(tokens(excluded)),
            "—".into(),
        ],
        detail,
    }
}

/// One row per item: label, tokens and reason.
pub(super) fn item_rows(excluded: &NotInRequest) -> Vec<Row> {
    excluded
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| Row {
            id: item_id(index),
            cells: vec![
                item.label.clone(),
                format_token_count(item.tokens),
                reason_text(item.reason).into(),
            ],
            detail: String::new(),
        })
        .collect()
}

/// The items whose label or text contains `needle`, an ASCII-lowercased
/// nonempty string, as rows of the request-order table: no position, no
/// tokens to the end.
pub(super) fn search_rows(excluded: &NotInRequest, needle: &str) -> Vec<Row> {
    excluded
        .items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let in_text = item
                .text
                .as_deref()
                .and_then(|text| text.to_ascii_lowercase().find(needle));
            if in_text.is_none() && !item.label.to_ascii_lowercase().contains(needle) {
                return None;
            }
            let detail = match item.text.as_deref() {
                Some(text) => excerpt(text, in_text),
                None => "The text was left out of the report for size.".into(),
            };
            Some(Row {
                id: item_id(index),
                cells: vec![
                    "—".into(),
                    format!(
                        "not in next request · {} · {}",
                        short_reason(item.reason),
                        item.label
                    ),
                    format_token_count(item.tokens),
                    "—".into(),
                ],
                detail,
            })
        })
        .collect()
}

/// The text of one item. Without text, the document says why.
pub(super) fn document(index: usize, item: &ExcludedItem) -> Document {
    Document {
        key: format!("context:{}", item_id(index)),
        title: format!(
            "{} · ~{} tokens · {}",
            item.label,
            format_token_count(item.tokens),
            reason_text(item.reason)
        ),
        text: item.text.clone().unwrap_or_else(|| {
            format!(
                "The text was left out of the report for size: the report holds at most {} MiB \
                 of text for these messages, newest first.",
                NOT_IN_REQUEST_TEXT_MAXIMUM_BYTES / (1024 * 1024)
            )
        }),
    }
}
