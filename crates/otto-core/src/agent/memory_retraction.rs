//! Retraction notices: fixed-template lines that tell the model which memory
//! records it saw earlier in the conversation are no longer valid evidence.
//!
//! Selection intersects the persisted record references of the turns and
//! checkpoints still in the model view with the current lifecycle status of
//! those records (#591 §5 layer 2). It never uses retrieval and never replays
//! retired text. Rendering reads ids, revisions and the reason only: no record
//! text, no candidate reason and no model explanation reaches a notice (§9).
//! A withdrawn record does not imply its opposite; the preamble says so.

use std::collections::BTreeMap;

use super::memory::{RecordLifecycle, RecordStatus, quote};
use crate::session::MemoryRecordRef;

/// The header of the notice block. The block follows the recalled records in
/// the same request-local message.
pub const RETRACTION_CONTEXT_PREAMBLE: &str = "The following memory records appeared earlier in this \
    conversation and are no longer valid evidence. A withdrawn record does not imply its \
    opposite. Where a replacement is named, use the replacement.\n";

/// Why a referenced record is no longer valid evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetractionReason {
    NeedsReview,
    Superseded,
    Retracted,
    Expired,
    Forgotten,
    /// The record is active at a higher revision than the one the model saw.
    Updated,
}

impl RetractionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RetractionReason::NeedsReview => "needs_review",
            RetractionReason::Superseded => "superseded",
            RetractionReason::Retracted => "retracted",
            RetractionReason::Expired => "expired",
            RetractionReason::Forgotten => "forgotten",
            RetractionReason::Updated => "updated",
        }
    }
}

/// One notice. Every field is an identifier or an enum; there is no text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetractionNotice {
    pub id: String,
    /// The highest revision of the record the model saw.
    pub revision: Option<String>,
    /// The record's current revision when the store reported one.
    pub current_revision: Option<String>,
    pub reason: RetractionReason,
    /// The replacement record for `Superseded`.
    pub replacement_id: Option<String>,
}

fn revision_number(revision: Option<&str>) -> Option<u64> {
    revision.and_then(|value| value.parse().ok())
}

/// Picks the notices for `referenced` records given their current `lifecycle`,
/// one per record id, ordered by id. An `Active` record at the referenced
/// revision and an `Unknown` record select nothing; an `Active` record at a
/// higher revision selects `updated`.
pub fn select_retraction_notices(
    referenced: &[MemoryRecordRef],
    lifecycle: &[RecordLifecycle],
) -> Vec<RetractionNotice> {
    let mut highest_seen: BTreeMap<&str, Option<&str>> = BTreeMap::new();
    for reference in referenced {
        let seen = highest_seen.entry(reference.id.as_str()).or_insert(None);
        let candidate = reference.revision.as_deref();
        if seen.is_none()
            || revision_number(candidate)
                .is_some_and(|number| revision_number(*seen).is_none_or(|current| number > current))
        {
            *seen = candidate;
        }
    }
    let mut notices = Vec::new();
    for (id, seen_revision) in highest_seen {
        let Some(current) = lifecycle.iter().find(|entry| entry.id == id) else {
            continue;
        };
        let (reason, replacement_id) = match &current.status {
            RecordStatus::Unknown => continue,
            RecordStatus::Active => {
                let newer = match (
                    revision_number(seen_revision),
                    revision_number(current.revision.as_deref()),
                ) {
                    (Some(seen), Some(now)) => now > seen,
                    _ => false,
                };
                if !newer {
                    continue;
                }
                (RetractionReason::Updated, None)
            }
            RecordStatus::NeedsReview => (RetractionReason::NeedsReview, None),
            RecordStatus::Superseded { replacement_id } => {
                (RetractionReason::Superseded, Some(replacement_id.clone()))
            }
            RecordStatus::Retracted => (RetractionReason::Retracted, None),
            RecordStatus::Expired => (RetractionReason::Expired, None),
            RecordStatus::Forgotten => (RetractionReason::Forgotten, None),
        };
        notices.push(RetractionNotice {
            id: id.to_owned(),
            revision: seen_revision.map(str::to_owned),
            current_revision: current.revision.clone(),
            reason,
            replacement_id,
        });
    }
    notices
}

/// Renders the notice block: the preamble, then one
/// `<memory_retraction .../>` line per notice. Attribute values are JSON
/// quoted, so an id can never close the tag or add an attribute. Returns the
/// empty string for no notices.
pub fn render_retraction_notices(notices: &[RetractionNotice]) -> String {
    if notices.is_empty() {
        return String::new();
    }
    let mut rendered = String::from(RETRACTION_CONTEXT_PREAMBLE);
    for notice in notices {
        rendered.push_str("<memory_retraction id=");
        rendered.push_str(&quote(&notice.id));
        if let Some(revision) = &notice.revision {
            rendered.push_str(" revision=");
            rendered.push_str(&quote(revision));
        }
        if let Some(current) = &notice.current_revision {
            rendered.push_str(" current_revision=");
            rendered.push_str(&quote(current));
        }
        rendered.push_str(" reason=");
        rendered.push_str(&quote(notice.reason.as_str()));
        if let Some(replacement) = &notice.replacement_id {
            rendered.push_str(" replacement_id=");
            rendered.push_str(&quote(replacement));
        }
        rendered.push_str("/>\n");
    }
    rendered
}

/// The text of the request-local memory message: the recalled records, then
/// the notice block when there is one.
pub fn memory_request_text(memory_context: &str, retraction_context: &str) -> String {
    match (memory_context.is_empty(), retraction_context.is_empty()) {
        (true, true) => String::new(),
        (false, true) => memory_context.to_owned(),
        (true, false) => retraction_context.to_owned(),
        (false, false) => format!("{memory_context}\n{retraction_context}"),
    }
}

#[cfg(test)]
mod tests;

/// The sentence that precedes the summary lines in the notice block.
pub const SUMMARY_AFFECTED_PREAMBLE: &str = "The compaction summary in this context was written \
    from turns that used the records named below; its statements derived from them are no longer \
    valid evidence. A summary with unknown coverage cannot be traced to records.\n";

/// The heading of the retraction block in a compaction summary request.
pub const SUMMARY_INSTRUCTIONS_HEADING: &str = "Retracted memory records:";
/// The instruction that follows the notice lines in a compaction summary
/// request.
pub const SUMMARY_INSTRUCTIONS_RULE: &str = "Statements derived from these records are no longer \
    valid evidence. Do not carry them into the summary; where the previous summary states them, \
    record that they were withdrawn instead. A withdrawn record does not imply its opposite.";

/// A compaction summary in force that was written from turns that used a
/// record now retired, or whose memory coverage is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryNotice {
    /// The checkpoint id, or `remote` for a provider-managed compaction.
    pub checkpoint_id: String,
    /// The retired record and its reason; `None` for an unknown-coverage line.
    pub record: Option<(String, RetractionReason)>,
}

/// Picks the summary lines: one per retired record among the latest
/// checkpoint's `memory_records` that its `memory_retracted` does not cover;
/// one unknown-coverage line when the checkpoint has unknown coverage, or a
/// remote compaction is in force, and at least one record notice exists.
pub fn select_summary_notices(
    latest: Option<&crate::session::CompactionMetadata>,
    remote_in_force: bool,
    lifecycle: &[RecordLifecycle],
    record_notices: &[RetractionNotice],
) -> Vec<SummaryNotice> {
    let mut notices = Vec::new();
    if let Some(latest) = latest {
        let covered: std::collections::BTreeSet<&str> = latest
            .details
            .memory_retracted
            .iter()
            .map(|reference| reference.id.as_str())
            .collect();
        let affected: Vec<MemoryRecordRef> = latest
            .details
            .memory_records
            .iter()
            .filter(|reference| !covered.contains(reference.id.as_str()))
            .cloned()
            .collect();
        for notice in select_retraction_notices(&affected, lifecycle) {
            notices.push(SummaryNotice {
                checkpoint_id: latest.id.clone(),
                record: Some((notice.id, notice.reason)),
            });
        }
        if latest.details.memory_coverage.is_unknown() && !record_notices.is_empty() {
            notices.push(SummaryNotice {
                checkpoint_id: latest.id.clone(),
                record: None,
            });
        }
    }
    if remote_in_force && !record_notices.is_empty() {
        notices.push(SummaryNotice {
            checkpoint_id: "remote".into(),
            record: None,
        });
    }
    notices
}

/// Renders the summary lines, preceded by [`SUMMARY_AFFECTED_PREAMBLE`].
/// Returns the empty string for no notices.
pub fn render_summary_notices(notices: &[SummaryNotice]) -> String {
    if notices.is_empty() {
        return String::new();
    }
    let mut rendered = String::from(SUMMARY_AFFECTED_PREAMBLE);
    for notice in notices {
        rendered.push_str("<memory_summary_affected checkpoint=");
        rendered.push_str(&quote(&notice.checkpoint_id));
        match &notice.record {
            Some((id, reason)) => {
                rendered.push_str(" record=");
                rendered.push_str(&quote(id));
                rendered.push_str(" reason=");
                rendered.push_str(&quote(reason.as_str()));
            }
            None => rendered.push_str(" coverage=\"unknown\""),
        }
        rendered.push_str("/>\n");
    }
    rendered
}

/// The whole notice block of a request: the record notices, then the summary
/// lines. Returns the empty string when both are empty.
pub fn render_retraction_block(
    record_notices: &[RetractionNotice],
    summary_notices: &[SummaryNotice],
) -> String {
    let records = render_retraction_notices(record_notices);
    let summaries = render_summary_notices(summary_notices);
    match (records.is_empty(), summaries.is_empty()) {
        (true, true) => String::new(),
        (false, true) => records,
        (true, false) => summaries,
        (false, false) => format!("{records}{summaries}"),
    }
}

/// The block a compaction summary request carries so the summarizer drops
/// statements derived from retired records. Empty for no notices.
pub fn render_summary_instructions(notices: &[RetractionNotice]) -> String {
    if notices.is_empty() {
        return String::new();
    }
    let mut rendered = String::from(SUMMARY_INSTRUCTIONS_HEADING);
    rendered.push('\n');
    for notice in notices {
        rendered.push_str("<memory_retraction id=");
        rendered.push_str(&quote(&notice.id));
        if let Some(revision) = &notice.revision {
            rendered.push_str(" revision=");
            rendered.push_str(&quote(revision));
        }
        rendered.push_str(" reason=");
        rendered.push_str(&quote(notice.reason.as_str()));
        if let Some(replacement) = &notice.replacement_id {
            rendered.push_str(" replacement_id=");
            rendered.push_str(&quote(replacement));
        }
        rendered.push_str("/>\n");
    }
    rendered.push_str(SUMMARY_INSTRUCTIONS_RULE);
    rendered
}

/// The retired references a compaction applied: the previous checkpoint's
/// `memory_retracted` plus the references of `notices`, sorted and unique.
pub fn retracted_union(
    previous: Option<&crate::session::CompactionDetails>,
    notices: &[RetractionNotice],
) -> Vec<MemoryRecordRef> {
    let mut references: Vec<MemoryRecordRef> =
        previous.map_or_else(Vec::new, |previous| previous.memory_retracted.clone());
    references.extend(notices.iter().map(|notice| MemoryRecordRef {
        id: notice.id.clone(),
        revision: notice.revision.clone(),
    }));
    crate::session::normalize_memory_records(&references)
}
