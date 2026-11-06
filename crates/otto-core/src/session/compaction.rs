//! Compaction checkpoints: validation, boundary resolution, and the
//! compaction-aware view of an active path.
//!
//! The store methods that append a checkpoint live with the native store;
//! everything here is filesystem-free and builds for `wasm32-unknown-unknown`.
//!
//! Ownership: input is borrowed, results are owned. Concurrency: no shared
//! state. Errors: every failure is a [`PiError`].

use std::collections::{BTreeSet, HashSet};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;

use crate::model::Usage;

use super::context::{
    active_context_path, format_persisted_timestamp, index_context_entries, model_usage_to_pi,
    optional_pi_usage_to_model,
};
use super::memory_recall::{
    MEMORY_RECALL_MAXIMUM_RECORDS, MemoryRecordRef, MemoryReferenceCoverage,
    normalize_memory_records,
};
use super::pi::{PiCompaction, PiEntry, PiUsage};
use super::session_types::{CompactionCheckpoint, CompactionDetails, CompactionMetadata};
use super::{PiError, PiErrorKind};

/// Largest summary a checkpoint may carry, in bytes.
pub const COMPACTION_SUMMARY_MAXIMUM_BYTES: usize = 128 * 1024;
/// Largest number of read plus modified paths a checkpoint may carry.
pub const COMPACTION_DETAILS_MAXIMUM_PATHS: usize = 1_024;
/// Largest total path length a checkpoint may carry, in bytes.
pub const COMPACTION_DETAILS_MAXIMUM_BYTES: usize = 64 * 1024;

/// Rejects a checkpoint that cannot be persisted. The checks run in a fixed
/// order so the first failure reported is stable.
pub fn validate_compaction_checkpoint(checkpoint: &CompactionCheckpoint) -> Result<(), PiError> {
    if checkpoint.summary.trim().is_empty() {
        return Err(PiError::invalid(
            "compaction summary must be nonempty UTF-8",
        ));
    }
    if checkpoint.summary.len() > COMPACTION_SUMMARY_MAXIMUM_BYTES {
        return Err(PiError::size(
            PiErrorKind::EntryTooLarge,
            COMPACTION_SUMMARY_MAXIMUM_BYTES,
        ));
    }
    if checkpoint.first_kept_entry_id.trim().is_empty() {
        return Err(PiError::invalid(
            "compaction first-kept entry id is required",
        ));
    }
    if checkpoint.tokens_before < 0 {
        return Err(PiError::invalid(
            "compaction tokens before must be nonnegative",
        ));
    }
    format_persisted_timestamp(checkpoint.created_at, "compaction")?;
    if let Some(usage) = checkpoint.usage.as_ref() {
        usage
            .validate()
            .map_err(|error| PiError::invalid(error.0))?;
    }
    validate_compaction_details(&checkpoint.details)
}

/// Rejects file details that are too many, too long, malformed, or repeated.
pub fn validate_compaction_details(details: &CompactionDetails) -> Result<(), PiError> {
    if details.omitted_read_files < 0 || details.omitted_modified_files < 0 {
        return Err(PiError::invalid(
            "compaction omitted file counts must be nonnegative",
        ));
    }
    if details.read_files.len() > COMPACTION_DETAILS_MAXIMUM_PATHS
        || details.modified_files.len()
            > COMPACTION_DETAILS_MAXIMUM_PATHS.saturating_sub(details.read_files.len())
    {
        return Err(PiError::new(
            PiErrorKind::EntryTooLarge,
            format!("compaction file details maximum is {COMPACTION_DETAILS_MAXIMUM_PATHS} paths"),
        ));
    }
    let mut seen = HashSet::new();
    let mut path_bytes = 0usize;
    for path in details.read_files.iter().chain(&details.modified_files) {
        if !valid_compaction_detail_path(path) {
            return Err(PiError::invalid("compaction file detail path is invalid"));
        }
        if !seen.insert(path.as_str()) {
            return Err(PiError::invalid(
                "compaction file detail paths must be unique and disjoint",
            ));
        }
        if path.len() > COMPACTION_DETAILS_MAXIMUM_BYTES.saturating_sub(path_bytes) {
            return Err(PiError::size(
                PiErrorKind::EntryTooLarge,
                COMPACTION_DETAILS_MAXIMUM_BYTES,
            ));
        }
        path_bytes += path.len();
    }
    validate_compaction_memory_records(&details.memory_records)?;
    validate_compaction_memory_records(&details.memory_retracted)
}

/// Rejects memory references that are too many, malformed, or repeated.
fn validate_compaction_memory_records(records: &[MemoryRecordRef]) -> Result<(), PiError> {
    if records.len() > MEMORY_RECALL_MAXIMUM_RECORDS {
        return Err(PiError::new(
            PiErrorKind::EntryTooLarge,
            format!(
                "compaction memory records maximum is {MEMORY_RECALL_MAXIMUM_RECORDS} references"
            ),
        ));
    }
    let mut seen = HashSet::new();
    for record in records {
        if !record.is_valid() {
            return Err(PiError::invalid(
                "compaction memory record reference is invalid",
            ));
        }
        if !seen.insert(record) {
            return Err(PiError::invalid(
                "compaction memory record references must be unique",
            ));
        }
    }
    Ok(())
}

/// True for a nonempty path that is already in cleaned form and carries no
/// control characters. `&str` is UTF-8 by construction, so no encoding check is
/// needed.
pub fn valid_compaction_detail_path(path: &str) -> bool {
    if path.is_empty() || clean_path(path) != path {
        return false;
    }
    !path
        .chars()
        .any(|character| character <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&character))
}

/// Used only to decide whether a path is already clean, so the exact result
/// matters.
fn clean_path(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let bytes = path.as_bytes();
    let rooted = bytes[0] == b'/';
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let (mut read, mut dotdot) = (0usize, 0usize);
    if rooted {
        out.push(b'/');
        read = 1;
        dotdot = 1;
    }
    while read < bytes.len() {
        let rest = &bytes[read..];
        // An empty element or a bare `.` element is dropped, exactly as
        // Go's `filepath.Clean` drops them.
        if rest[0] == b'/' || (rest[0] == b'.' && (rest.len() == 1 || rest[1] == b'/')) {
            read += 1;
        } else if rest[0] == b'.'
            && rest.len() > 1
            && rest[1] == b'.'
            && (rest.len() == 2 || rest[2] == b'/')
        {
            read += 2;
            if out.len() > dotdot {
                out.pop();
                while out.len() > dotdot && out[out.len() - 1] != b'/' {
                    out.pop();
                }
            } else if !rooted {
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(b"..");
                dotdot = out.len();
            }
        } else {
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            while read < bytes.len() && bytes[read] != b'/' {
                out.push(bytes[read]);
                read += 1;
            }
        }
    }
    if out.is_empty() {
        out.push(b'.');
    }
    String::from_utf8(out).expect("cleaning keeps the input's UTF-8 boundaries")
}

/// Converts checkpoint usage to the wire shape. Unlike
/// [`model_usage_to_pi`], absent usage stays absent instead of becoming an
/// all-zero object.
pub fn compaction_usage_to_pi(usage: Option<&Usage>) -> Result<Option<PiUsage>, PiError> {
    match usage {
        None => Ok(None),
        Some(usage) => model_usage_to_pi(Some(usage)),
    }
}

/// True when the checkpoint carried a `retainedTail` key, even an empty one.
pub fn compaction_has_retained_tail(compaction: &PiCompaction) -> bool {
    compaction.retained_tail.is_some()
}

/// Clamps a persisted token count into the nonnegative range.
pub fn safe_context_token_count(tokens: i64) -> i64 {
    tokens.max(0)
}

/// True when the details carry anything worth persisting.
pub fn compaction_details_present(details: &CompactionDetails) -> bool {
    !details.read_files.is_empty()
        || !details.modified_files.is_empty()
        || details.omitted_read_files != 0
        || details.omitted_modified_files != 0
        || !details.memory_records.is_empty()
        || !details.memory_retracted.is_empty()
        || !details.memory_coverage.is_unknown()
}

/// True for the entry types that count as real context on the active path.
pub fn is_real_compaction_context_entry(entry: &PiEntry) -> bool {
    matches!(
        entry.type_name.as_str(),
        "message" | "branch_summary" | "custom_message"
    )
}

/// The compaction in force at `leaf_id`, or `None` when the active path
/// carries no checkpoint.
pub fn latest_compaction_metadata(
    entries: &[PiEntry],
    leaf_id: &str,
) -> Result<Option<CompactionMetadata>, PiError> {
    if entries.is_empty() {
        return Ok(None);
    }
    let (index, _) = index_context_entries(entries)?;
    let path = active_context_path(entries, leaf_id, &index)?;
    let Some(latest) = path
        .iter()
        .rposition(|entry| entry.type_name == "compaction")
    else {
        return Ok(None);
    };
    let entry = &path[latest];
    let compaction = entry
        .compaction
        .as_ref()
        .ok_or_else(|| PiError::invalid("compaction payload is required"))?;
    let (first_kept, retained_tail_only) = resolve_compaction_boundary(&path, latest)?;
    let usage = optional_pi_usage_to_model(compaction.usage.as_ref())?;
    let first_post_checkpoint_message_id = path[latest + 1..]
        .iter()
        .find(|candidate| is_real_compaction_context_entry(candidate))
        .map(|candidate| candidate.id.clone())
        .unwrap_or_default();
    Ok(Some(CompactionMetadata {
        id: entry.id.clone(),
        summary: compaction.summary.clone(),
        first_kept_entry_id: first_kept,
        tokens_before: safe_context_token_count(compaction.tokens_before),
        usage,
        details: decode_compaction_details(compaction.details.as_deref()),
        retained_tail_only,
        first_post_checkpoint_message_id,
    }))
}

pub fn project_compaction_metadata(
    metadata: &mut CompactionMetadata,
    entries: &[PiEntry],
    leaf_id: &str,
    messages: &[crate::model::Message],
) -> Result<(), PiError> {
    let (index, _) = index_context_entries(entries)?;
    let path = active_context_path(entries, leaf_id, &index)?;
    let Some(checkpoint) = path.iter().position(|entry| entry.id == metadata.id) else {
        return Ok(());
    };
    metadata.first_post_checkpoint_message_id = path[checkpoint + 1..]
        .iter()
        .filter(|entry| is_real_compaction_context_entry(entry))
        .find_map(|entry| {
            messages
                .iter()
                .any(|message| message.id == entry.id)
                .then(|| entry.id.clone())
        })
        .unwrap_or_default();
    Ok(())
}

/// Resolves where the retained context starts. Returns the first-kept entry
/// id and whether the checkpoint uses the synthetic retained tail instead.
pub fn resolve_compaction_boundary(
    path: &[PiEntry],
    checkpoint_index: usize,
) -> Result<(String, bool), PiError> {
    let compaction = path[checkpoint_index]
        .compaction
        .as_ref()
        .ok_or_else(|| PiError::invalid("compaction payload is required"))?;
    if let Some(first_kept) = compaction.first_kept_entry_id.as_ref()
        && path[..checkpoint_index]
            .iter()
            .any(|entry| entry.id == *first_kept)
    {
        return Ok((first_kept.clone(), false));
    }
    if compaction_has_retained_tail(compaction) {
        return Ok((String::new(), true));
    }
    if compaction.first_kept_entry_id.is_none() {
        return Err(PiError::invalid(
            "compaction requires firstKeptEntryId or retainedTail",
        ));
    }
    Err(PiError::invalid(
        "compaction firstKeptEntryId is not on the active path before the checkpoint",
    ))
}

/// Trims an active path down to what the newest checkpoint retains.
///
/// A retained-tail checkpoint keeps the checkpoint and everything after it. A
/// legacy checkpoint keeps the entries from its anchor forward, drops nested
/// checkpoints in that span, and strips the retained tail from the copy of the
/// checkpoint it emits so the tail is not replayed twice.
pub fn compaction_aware_path(path: &[PiEntry]) -> Result<Vec<PiEntry>, PiError> {
    let Some(latest) = path
        .iter()
        .rposition(|entry| entry.type_name == "compaction")
    else {
        return Ok(path.to_vec());
    };
    let (first_kept_id, retained_tail_only) = resolve_compaction_boundary(path, latest)?;
    if retained_tail_only {
        let mut selected = Vec::with_capacity(path.len() - latest);
        selected.push(path[latest].clone());
        selected.extend_from_slice(&path[latest + 1..]);
        return Ok(selected);
    }
    let Some(first_kept) = path[..latest]
        .iter()
        .position(|entry| entry.id == first_kept_id)
    else {
        return Err(PiError::invalid(
            "compaction firstKeptEntryId is not on the active path before the checkpoint",
        ));
    };
    let mut legacy = path[latest].clone();
    if let Some(payload) = legacy.compaction.as_mut() {
        payload.retained_tail = None;
    }
    let mut selected = Vec::with_capacity(path.len() - first_kept);
    selected.push(legacy);
    selected.extend(
        path[first_kept..latest]
            .iter()
            .filter(|entry| entry.type_name != "compaction")
            .cloned(),
    );
    selected.extend_from_slice(&path[latest + 1..]);
    Ok(selected)
}

/// Reads persisted compaction details leniently: anything malformed, including
/// a duplicate JSON key, yields empty details rather than an error.
pub fn decode_compaction_details(raw: Option<&RawValue>) -> CompactionDetails {
    let Some(raw) = raw else {
        return CompactionDetails::default();
    };
    let text = raw.get();
    if text.is_empty() || !unique_json_object(text) {
        return CompactionDetails::default();
    }
    let Ok(object) =
        serde_json::from_str::<std::collections::BTreeMap<String, Box<RawValue>>>(text)
    else {
        return CompactionDetails::default();
    };
    for field in ["readFiles", "modifiedFiles"] {
        if let Some(value) = object.get(field) {
            let trimmed = value.get().trim_start();
            if !trimmed.starts_with('[')
                || serde_json::from_str::<Vec<String>>(value.get()).is_err()
            {
                return CompactionDetails::default();
            }
        }
    }
    for field in ["omittedReadFiles", "omittedModifiedFiles"] {
        if let Some(value) = object.get(field)
            && (value.get().trim() == "null" || serde_json::from_str::<i64>(value.get()).is_err())
        {
            return CompactionDetails::default();
        }
    }
    for field in ["memoryRecords", "memoryRetracted"] {
        if let Some(value) = object.get(field)
            && (!value.get().trim_start().starts_with('[')
                || serde_json::from_str::<Vec<MemoryRecordRef>>(value.get()).is_err())
        {
            return CompactionDetails::default();
        }
    }
    if let Some(value) = object.get("memoryCoverage")
        && (!value.get().trim_start().starts_with('"')
            || serde_json::from_str::<MemoryReferenceCoverage>(value.get()).is_err())
    {
        return CompactionDetails::default();
    }
    let Ok(details) = serde_json::from_str::<CompactionDetails>(text) else {
        return CompactionDetails::default();
    };
    sanitize_compaction_details(&details)
}

/// Drops invalid and duplicated paths, prefers modified over read, sorts, and
/// applies the path and byte caps, counting what it dropped. Memory references
/// are normalized and bounded; any dropped reference resets the coverage to
/// unknown because the list no longer names every record.
pub fn sanitize_compaction_details(details: &CompactionDetails) -> CompactionDetails {
    let modified: BTreeSet<&str> = details
        .modified_files
        .iter()
        .filter(|path| valid_compaction_detail_path(path))
        .map(String::as_str)
        .collect();
    let reads: BTreeSet<&str> = details
        .read_files
        .iter()
        .filter(|path| valid_compaction_detail_path(path) && !modified.contains(path.as_str()))
        .map(String::as_str)
        .collect();

    let mut result = CompactionDetails {
        omitted_read_files: details.omitted_read_files.max(0),
        omitted_modified_files: details.omitted_modified_files.max(0),
        ..CompactionDetails::default()
    };
    let mut remaining_paths = COMPACTION_DETAILS_MAXIMUM_PATHS;
    let mut remaining_bytes = COMPACTION_DETAILS_MAXIMUM_BYTES;
    for path in modified {
        if remaining_paths == 0 || path.len() > remaining_bytes {
            result.omitted_modified_files = result.omitted_modified_files.saturating_add(1);
            continue;
        }
        result.modified_files.push(path.to_owned());
        remaining_paths -= 1;
        remaining_bytes -= path.len();
    }
    for path in reads {
        if remaining_paths == 0 || path.len() > remaining_bytes {
            result.omitted_read_files = result.omitted_read_files.saturating_add(1);
            continue;
        }
        result.read_files.push(path.to_owned());
        remaining_paths -= 1;
        remaining_bytes -= path.len();
    }
    let mut memory_records = normalize_memory_records(&details.memory_records);
    memory_records.truncate(MEMORY_RECALL_MAXIMUM_RECORDS);
    result.memory_coverage = if memory_records.len() == details.memory_records.len() {
        details.memory_coverage
    } else {
        MemoryReferenceCoverage::Unknown
    };
    result.memory_records = memory_records;
    let mut memory_retracted = normalize_memory_records(&details.memory_retracted);
    memory_retracted.truncate(MEMORY_RECALL_MAXIMUM_RECORDS);
    result.memory_retracted = memory_retracted;
    result
}

/// True when `text` is one JSON object and no object in it, at any depth,
/// repeats a key. `serde_json` keeps the last value for a duplicate key, so the
/// rejection is checked explicitly.
fn unique_json_object(text: &str) -> bool {
    text.trim_start().starts_with('{') && serde_json::from_str::<UniqueJson>(text).is_ok()
}

/// A value that deserializes from any JSON but fails on a repeated key.
struct UniqueJson;

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

struct UniqueJsonVisitor;

impl<'de> Visitor<'de> for UniqueJsonVisitor {
    type Value = UniqueJson;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("any JSON value with no repeated object key")
    }

    fn visit_unit<E: de::Error>(self) -> Result<UniqueJson, E> {
        Ok(UniqueJson)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<UniqueJson, E> {
        Ok(UniqueJson)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<UniqueJson, E> {
        Ok(UniqueJson)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<UniqueJson, E> {
        Ok(UniqueJson)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<UniqueJson, E> {
        Ok(UniqueJson)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<UniqueJson, E> {
        Ok(UniqueJson)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<UniqueJson, A::Error> {
        while sequence.next_element::<UniqueJson>()?.is_some() {}
        Ok(UniqueJson)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<UniqueJson, A::Error> {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key) {
                return Err(de::Error::custom("duplicate object key"));
            }
            map.next_value::<UniqueJson>()?;
        }
        Ok(UniqueJson)
    }
}

#[cfg(test)]
mod tests;
