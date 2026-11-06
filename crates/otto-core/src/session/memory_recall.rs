//! Versioned `otto.memory_recall` records: the text-free references of the
//! memory records one turn rendered into its provider requests.
//!
//! The record holds record ids and revisions only. It never holds record
//! text, kind, key, labels or scope text, so a later forget of a record has
//! nothing to remove here, while a later retirement can still name the turns
//! and checkpoints that saw the record.
//!
//! The agent appends one record after every user message it persists, so a
//! user turn written by this binary always has one; an empty list means no
//! record was rendered. A user turn without a record was written before this
//! record existed, and its memory coverage is unknown.
//!
//! Ownership and concurrency: plain data. Errors: `decode_memory_recall`
//! returns `None` for malformed data or another schema version.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub const MEMORY_RECALL_CUSTOM_TYPE: &str = "otto.memory_recall";
pub const MEMORY_RECALL_SCHEMA_VERSION: u32 = 1;

/// Most references one record or checkpoint may carry.
pub const MEMORY_RECALL_MAXIMUM_RECORDS: usize = 1_024;
/// Longest record id, in bytes.
pub const MEMORY_RECORD_ID_MAXIMUM_BYTES: usize = 256;
/// Longest revision token, in bytes.
pub const MEMORY_RECORD_REVISION_MAXIMUM_BYTES: usize = 64;

/// A text-free reference to one memory record: its id and the revision the
/// store reported when the record was rendered. `revision` is `None` for a
/// store that reports no revision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MemoryRecordRef {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

impl MemoryRecordRef {
    /// A well-formed reference: a nonempty id within the byte bounds and a
    /// revision within its bound, both free of control characters.
    pub fn is_valid(&self) -> bool {
        valid_token(&self.id, MEMORY_RECORD_ID_MAXIMUM_BYTES)
            && self
                .revision
                .as_deref()
                .is_none_or(|revision| valid_token(revision, MEMORY_RECORD_REVISION_MAXIMUM_BYTES))
    }
}

fn valid_token(token: &str, maximum_bytes: usize) -> bool {
    !token.is_empty()
        && token.len() <= maximum_bytes
        && !token.chars().any(char::is_control)
        && token.trim() == token
}

/// Whether a set of turns, or the checkpoint that summarizes them, has a
/// reference record for every user turn it covers. `Unknown` is the default
/// and the reading of every checkpoint written before the field existed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryReferenceCoverage {
    #[default]
    Unknown,
    Complete,
}

impl MemoryReferenceCoverage {
    pub fn is_unknown(&self) -> bool {
        *self == Self::Unknown
    }
}

/// The payload of an `otto.memory_recall` entry. Wire field names are pinned
/// camelCase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRecallRecord {
    pub schema_version: u32,
    pub records: Vec<MemoryRecordRef>,
}

/// The references one turn rendered, anchored to the user message that
/// started the turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnMemoryReferences {
    /// The id of the nearest `Role::User` message before the record on the
    /// active path.
    pub user_message_id: String,
    pub records: Vec<MemoryRecordRef>,
}

/// Encodes the references of one turn as `otto.memory_recall` data.
pub fn encode_memory_recall(records: &[MemoryRecordRef]) -> String {
    serde_json::to_string(&MemoryRecallRecord {
        schema_version: MEMORY_RECALL_SCHEMA_VERSION,
        records: normalize_memory_records(records),
    })
    .expect("memory recall record serializes")
}

/// Decodes `otto.memory_recall` data; `None` for malformed data, another
/// schema version, an invalid reference, or more references than
/// [`MEMORY_RECALL_MAXIMUM_RECORDS`].
pub fn decode_memory_recall(data: &str) -> Option<MemoryRecallRecord> {
    let record: MemoryRecallRecord = serde_json::from_str(data).ok()?;
    if record.schema_version != MEMORY_RECALL_SCHEMA_VERSION
        || record.records.len() > MEMORY_RECALL_MAXIMUM_RECORDS
        || !record.records.iter().all(MemoryRecordRef::is_valid)
    {
        return None;
    }
    Some(MemoryRecallRecord {
        schema_version: record.schema_version,
        records: normalize_memory_records(&record.records),
    })
}

/// Sorts the references and drops duplicates and invalid ones.
pub fn normalize_memory_records(records: &[MemoryRecordRef]) -> Vec<MemoryRecordRef> {
    records
        .iter()
        .filter(|record| record.is_valid())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(id: &str, revision: Option<&str>) -> MemoryRecordRef {
        MemoryRecordRef {
            id: id.into(),
            revision: revision.map(str::to_owned),
        }
    }

    #[test]
    fn record_round_trips_with_camel_case_fields_and_sorted_unique_records() {
        let encoded = encode_memory_recall(&[
            reference("rec-b", Some("3")),
            reference("rec-a", None),
            reference("rec-b", Some("3")),
        ]);
        assert_eq!(
            encoded,
            r#"{"schemaVersion":1,"records":[{"id":"rec-a"},{"id":"rec-b","revision":"3"}]}"#
        );
        let decoded = decode_memory_recall(&encoded).expect("decodes");
        assert_eq!(
            decoded.records,
            [reference("rec-a", None), reference("rec-b", Some("3"))]
        );
    }

    #[test]
    fn other_schemas_and_malformed_data_are_rejected() {
        assert_eq!(
            decode_memory_recall(r#"{"schemaVersion":2,"records":[]}"#),
            None
        );
        assert_eq!(decode_memory_recall("not json"), None);
        assert_eq!(decode_memory_recall(r#"{"schemaVersion":1}"#), None);
        assert_eq!(
            decode_memory_recall(r#"{"schemaVersion":1,"records":[{"id":""}]}"#),
            None,
            "an empty id is not a reference"
        );
        assert_eq!(
            decode_memory_recall(r#"{"schemaVersion":1,"records":[{"id":"a\nb"}]}"#),
            None,
            "control characters are rejected"
        );
        let too_long = format!(
            r#"{{"schemaVersion":1,"records":[{{"id":"{}"}}]}}"#,
            "x".repeat(MEMORY_RECORD_ID_MAXIMUM_BYTES + 1)
        );
        assert_eq!(decode_memory_recall(&too_long), None);
    }

    #[test]
    fn an_empty_record_decodes_as_no_references() {
        let encoded = encode_memory_recall(&[]);
        assert_eq!(encoded, r#"{"schemaVersion":1,"records":[]}"#);
        assert_eq!(
            decode_memory_recall(&encoded),
            Some(MemoryRecallRecord {
                schema_version: 1,
                records: Vec::new(),
            })
        );
    }

    #[test]
    fn the_record_count_bound_rejects_oversized_records() {
        let records: Vec<MemoryRecordRef> = (0..=MEMORY_RECALL_MAXIMUM_RECORDS)
            .map(|index| reference(&format!("rec-{index}"), None))
            .collect();
        let encoded = serde_json::to_string(&MemoryRecallRecord {
            schema_version: MEMORY_RECALL_SCHEMA_VERSION,
            records,
        })
        .expect("encode");
        assert_eq!(decode_memory_recall(&encoded), None);
    }

    #[test]
    fn coverage_defaults_to_unknown_and_serializes_as_snake_case() {
        assert!(MemoryReferenceCoverage::default().is_unknown());
        assert_eq!(
            serde_json::to_string(&MemoryReferenceCoverage::Complete).expect("encode"),
            r#""complete""#
        );
        assert_eq!(
            serde_json::from_str::<MemoryReferenceCoverage>(r#""unknown""#).expect("decode"),
            MemoryReferenceCoverage::Unknown
        );
    }
}
