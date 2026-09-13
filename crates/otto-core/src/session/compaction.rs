//! Compaction checkpoints: validation, boundary resolution, and the
//! compaction-aware view of an active path.
//!
//! Port of the pure functions in `internal/session/compaction.go`. The store
//! methods that append a checkpoint live with the native store; everything
//! here is filesystem-free and builds for `wasm32-unknown-unknown`.
//!
//! Ownership: input is borrowed, results are owned. Concurrency: no shared
//! state. Errors: [`PiError`] with the Go message text, byte for byte.

use std::collections::{BTreeSet, HashSet};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;

use crate::model::Usage;

use super::context::{
    active_context_path, format_persisted_timestamp, index_context_entries, model_usage_to_pi,
    optional_pi_usage_to_model,
};
use super::pi::{PiCompaction, PiEntry, PiUsage};
use super::types::{CompactionCheckpoint, CompactionDetails, CompactionMetadata};
use super::{PiError, PiErrorKind};

/// Largest summary a checkpoint may carry, in bytes.
pub const COMPACTION_SUMMARY_MAXIMUM_BYTES: usize = 128 * 1024;
/// Largest number of read plus modified paths a checkpoint may carry.
pub const COMPACTION_DETAILS_MAXIMUM_PATHS: usize = 1_024;
/// Largest total path length a checkpoint may carry, in bytes.
pub const COMPACTION_DETAILS_MAXIMUM_BYTES: usize = 64 * 1024;

/// Rejects a checkpoint that cannot be persisted. The checks run in the Go
/// order so the first failure reported is the same one.
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
    Ok(())
}

/// True for a nonempty path that is already in cleaned form and carries no
/// control characters. `&str` is UTF-8 by construction, which covers Go's
/// `utf8.ValidString` check.
pub fn valid_compaction_detail_path(path: &str) -> bool {
    if path.is_empty() || clean_path(path) != path {
        return false;
    }
    !path
        .chars()
        .any(|character| character <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&character))
}

/// Port of Go's `filepath.Clean` for the `/` separator. Used only to decide
/// whether a path is already clean, so the exact Go result matters.
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
    let Ok(details) = serde_json::from_str::<CompactionDetails>(text) else {
        return CompactionDetails::default();
    };
    sanitize_compaction_details(&details)
}

/// Drops invalid and duplicated paths, prefers modified over read, sorts, and
/// applies the path and byte caps, counting what it dropped.
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
    result
}

/// True when `text` is one JSON object and no object in it, at any depth,
/// repeats a key. Go's decoder rejects duplicates here; `serde_json` keeps the
/// last value, so the check is explicit.
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
mod tests {
    use super::*;
    use crate::model::Usage;
    use serde_json::value::RawValue;

    fn valid_checkpoint() -> CompactionCheckpoint {
        CompactionCheckpoint {
            summary: "summary".into(),
            first_kept_entry_id: "0000000a".into(),
            tokens_before: 12,
            usage: None,
            details: CompactionDetails::default(),
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        }
    }

    fn fixed_width_path(prefix: &str, index: usize) -> String {
        format!("{prefix}{index:04}-{}", "x".repeat(58))
    }

    /// Port of `TestAppendCompactionEnforcesSummaryAndDetailsBoundsBeforeMutation`.
    /// The Go cases that mutate the summary or a path into invalid UTF-8 have no
    /// Rust equivalent: `String` is UTF-8 by construction.
    #[test]
    fn validate_compaction_checkpoint_enforces_bounds() {
        type Case = (
            &'static str,
            Box<dyn Fn(&mut CompactionCheckpoint)>,
            PiErrorKind,
        );
        let cases: Vec<Case> = vec![
            (
                "summary above byte bound",
                Box::new(|checkpoint| {
                    checkpoint.summary = "s".repeat(COMPACTION_SUMMARY_MAXIMUM_BYTES + 1);
                }),
                PiErrorKind::EntryTooLarge,
            ),
            (
                "blank summary",
                Box::new(|checkpoint| checkpoint.summary = "   ".into()),
                PiErrorKind::Invalid,
            ),
            (
                "blank first-kept entry id",
                Box::new(|checkpoint| checkpoint.first_kept_entry_id = " ".into()),
                PiErrorKind::Invalid,
            ),
            (
                "negative tokens before",
                Box::new(|checkpoint| checkpoint.tokens_before = -1),
                PiErrorKind::Invalid,
            ),
            (
                "negative omitted read files",
                Box::new(|checkpoint| checkpoint.details.omitted_read_files = -1),
                PiErrorKind::Invalid,
            ),
            (
                "negative omitted modified files",
                Box::new(|checkpoint| checkpoint.details.omitted_modified_files = -1),
                PiErrorKind::Invalid,
            ),
            (
                "path count above bound",
                Box::new(|checkpoint| {
                    checkpoint.details.read_files = (0..=COMPACTION_DETAILS_MAXIMUM_PATHS)
                        .map(|index| format!("path-{index}.go"))
                        .collect();
                }),
                PiErrorKind::EntryTooLarge,
            ),
            (
                "path text above bound",
                Box::new(|checkpoint| {
                    checkpoint.details.read_files =
                        vec!["x".repeat(COMPACTION_DETAILS_MAXIMUM_BYTES + 1)];
                }),
                PiErrorKind::EntryTooLarge,
            ),
            (
                "empty path",
                Box::new(|checkpoint| checkpoint.details.read_files = vec![String::new()]),
                PiErrorKind::Invalid,
            ),
            (
                "C0 control path",
                Box::new(|checkpoint| checkpoint.details.read_files = vec!["a\u{0}.go".into()]),
                PiErrorKind::Invalid,
            ),
            (
                "DEL control path",
                Box::new(|checkpoint| checkpoint.details.read_files = vec!["a\u{7f}.go".into()]),
                PiErrorKind::Invalid,
            ),
            (
                "C1 control path",
                Box::new(|checkpoint| checkpoint.details.read_files = vec!["a\u{85}.go".into()]),
                PiErrorKind::Invalid,
            ),
            (
                "unclean path",
                Box::new(|checkpoint| {
                    checkpoint.details.read_files = vec!["dir/../file.go".into()]
                }),
                PiErrorKind::Invalid,
            ),
            (
                "duplicate read path",
                Box::new(|checkpoint| {
                    checkpoint.details.read_files = vec!["dup.go".into(), "dup.go".into()];
                }),
                PiErrorKind::Invalid,
            ),
            (
                "duplicate modified path",
                Box::new(|checkpoint| {
                    checkpoint.details.modified_files = vec!["dup.go".into(), "dup.go".into()];
                }),
                PiErrorKind::Invalid,
            ),
            (
                "read and modified overlap",
                Box::new(|checkpoint| {
                    checkpoint.details.read_files = vec!["both.go".into()];
                    checkpoint.details.modified_files = vec!["both.go".into()];
                }),
                PiErrorKind::Invalid,
            ),
        ];

        for (name, mutate, kind) in cases {
            let mut checkpoint = valid_checkpoint();
            mutate(&mut checkpoint);
            let error = validate_compaction_checkpoint(&checkpoint)
                .expect_err(&format!("{name}: expected rejection"));
            assert_eq!(error.kind(), kind, "{name}: kind");
        }
    }

    /// Port of `TestAppendCompactionAcceptsExactSummaryAndDetailsBounds`.
    #[test]
    fn validate_compaction_checkpoint_accepts_exact_bounds() {
        let mut checkpoint = valid_checkpoint();
        checkpoint.summary = "s".repeat(COMPACTION_SUMMARY_MAXIMUM_BYTES);
        let mut read_files: Vec<String> = (0..COMPACTION_DETAILS_MAXIMUM_PATHS)
            .map(|index| format!("path-{index:04}"))
            .collect();
        let total: usize = read_files.iter().map(String::len).sum();
        let last = read_files.len() - 1;
        read_files[last].push_str(&"x".repeat(COMPACTION_DETAILS_MAXIMUM_BYTES - total));
        checkpoint.details.read_files = read_files;
        checkpoint.usage = Some(Usage::default());
        validate_compaction_checkpoint(&checkpoint).expect("exact bounds must be accepted");
    }

    /// Port of `TestOpenCompactionDetailsRejectsDuplicateKeysAndMalformedKnownFieldsLazily`.
    #[test]
    fn decode_compaction_details_is_lenient() {
        let cases = [
            r#"{"readFiles":["first.go"],"readFiles":["second.go"]}"#,
            "[]",
            r#"{"readFiles":"README.md"}"#,
            r#"{"readFiles":["README.md",1]}"#,
            r#"{"modifiedFiles":{}}"#,
            r#"{"omittedReadFiles":"1"}"#,
            r#"{"omittedModifiedFiles":9223372036854775808}"#,
        ];
        for case in cases {
            let raw = RawValue::from_string(case.to_string()).expect("fixture must be JSON");
            assert_eq!(
                decode_compaction_details(Some(&raw)),
                CompactionDetails::default(),
                "{case}",
            );
        }
        assert_eq!(
            decode_compaction_details(None),
            CompactionDetails::default(),
        );
    }

    /// Port of `TestOpenCompactionDetailsSanitizesPathsAndClonesMetadata`.
    #[test]
    fn decode_compaction_details_sanitizes_paths() {
        let raw = RawValue::from_string(
            serde_json::json!({
                "readFiles": ["z.go", "a.go", "a.go", "dup.go", "", "bad\u{0}.go",
                              "del\u{7f}.go", "c1\u{85}.go", "dir/../unclean.go",
                              "./unclean.go", "both.go"],
                "modifiedFiles": ["m.go", "both.go", "m.go"],
                "omittedReadFiles": 2,
                "omittedModifiedFiles": 3,
                "unknown": {"nested": true},
            })
            .to_string(),
        )
        .expect("fixture must be JSON");
        assert_eq!(
            decode_compaction_details(Some(&raw)),
            CompactionDetails {
                read_files: vec!["a.go".into(), "dup.go".into(), "z.go".into()],
                modified_files: vec!["both.go".into(), "m.go".into()],
                omitted_read_files: 2,
                omitted_modified_files: 3,
            },
        );
    }

    /// Port of `TestOpenCompactionDetailsBoundsOversizedExternalMetadata`.
    #[test]
    fn decode_compaction_details_bounds_oversized_metadata() {
        let modified: Vec<String> = (0..1_100)
            .map(|index| fixed_width_path("m", index))
            .collect();
        let read: Vec<String> = (0..20).map(|index| fixed_width_path("r", index)).collect();
        let raw = RawValue::from_string(
            serde_json::json!({
                "readFiles": read,
                "modifiedFiles": modified,
                "omittedReadFiles": i64::MAX - 10,
                "omittedModifiedFiles": i64::MAX - 50,
            })
            .to_string(),
        )
        .expect("fixture must be JSON");
        let details = decode_compaction_details(Some(&raw));
        assert_eq!(
            details.modified_files.len(),
            COMPACTION_DETAILS_MAXIMUM_PATHS
        );
        assert!(details.read_files.is_empty());
        let bytes: usize = details
            .modified_files
            .iter()
            .chain(&details.read_files)
            .map(String::len)
            .sum();
        assert_eq!(bytes, COMPACTION_DETAILS_MAXIMUM_BYTES);
        assert_eq!(details.omitted_read_files, i64::MAX);
        assert_eq!(details.omitted_modified_files, i64::MAX);
        let mut sorted = details.modified_files.clone();
        sorted.sort();
        assert_eq!(details.modified_files, sorted);
    }

    #[test]
    fn clean_path_matches_go_filepath_clean() {
        for (input, want) in [
            ("", "."),
            (".", "."),
            ("a/b", "a/b"),
            ("a//b", "a/b"),
            ("a/./b", "a/b"),
            ("a/../b", "b"),
            ("../a", "../a"),
            ("/../a", "/a"),
            ("a/b/../..", "."),
            ("./unclean.go", "unclean.go"),
            ("/a/b/", "/a/b"),
        ] {
            assert_eq!(clean_path(input), want, "{input}");
        }
    }

    #[test]
    fn safe_context_token_count_floors_at_zero() {
        assert_eq!(safe_context_token_count(-5), 0);
        assert_eq!(safe_context_token_count(7), 7);
    }

    #[test]
    fn compaction_usage_to_pi_keeps_absent_usage_absent() {
        assert_eq!(compaction_usage_to_pi(None).unwrap(), None);
        let usage = Usage::default();
        assert_eq!(
            compaction_usage_to_pi(Some(&usage)).unwrap(),
            Some(PiUsage::default()),
        );
    }
}
