//! Memory record references carried by a compaction checkpoint.
//!
//! A checkpoint holds the union of the references of the turns it summarizes
//! and of the previous checkpoint. The function is pure: it reads no session.

use std::collections::BTreeSet;

use crate::model::{Message, Role};
use crate::session::{
    CompactionDetails, MemoryRecordRef, MemoryReferenceCoverage, TurnMemoryReferences,
    normalize_memory_records,
};

/// Returns the sorted, unique references of the previous checkpoint and of
/// every summarized user turn that has a reference record, and the coverage.
///
/// Coverage is `Complete` only when the previous checkpoint (if any) is
/// `Complete` and every summarized `Role::User` message has a reference
/// record. Messages of other roles never need one.
pub(super) fn summarized_memory_references<'a>(
    previous: Option<&CompactionDetails>,
    summarized_messages: impl IntoIterator<Item = &'a Message>,
    references: &[TurnMemoryReferences],
) -> (Vec<MemoryRecordRef>, MemoryReferenceCoverage) {
    let summarized_user_ids: BTreeSet<&str> = summarized_messages
        .into_iter()
        .filter(|message| message.role == Role::User)
        .map(|message| message.id.as_str())
        .collect();
    let mut records: Vec<MemoryRecordRef> =
        previous.map_or_else(Vec::new, |previous| previous.memory_records.clone());
    let mut covered_user_ids: BTreeSet<&str> = BTreeSet::new();
    for reference in references {
        if summarized_user_ids.contains(reference.user_message_id.as_str()) {
            covered_user_ids.insert(reference.user_message_id.as_str());
            records.extend(reference.records.iter().cloned());
        }
    }
    let previous_complete = previous
        .is_none_or(|previous| previous.memory_coverage == MemoryReferenceCoverage::Complete);
    let coverage = if previous_complete && covered_user_ids.len() == summarized_user_ids.len() {
        MemoryReferenceCoverage::Complete
    } else {
        MemoryReferenceCoverage::Unknown
    };
    (normalize_memory_records(&records), coverage)
}

#[cfg(test)]
mod tests;
