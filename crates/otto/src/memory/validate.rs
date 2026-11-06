//! Request and record validation.
//!
//! Every trust boundary into the store runs through here: the service validates
//! before it touches the store, and the store validates again on decode so a
//! corrupted row cannot become a valid record.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, Utc};

use super::contracts::*;

/// Timestamps default to this and validation rejects it, so an unset time can
/// never reach the store.
pub fn zero_time() -> DateTime<Utc> {
    DateTime::UNIX_EPOCH
}

fn invalid_record(field: &str) -> Error {
    Error::detailed(ErrorKind::InvalidRecord, field)
}

fn invalid_record_limit(field: &str, limit: usize) -> Error {
    Error::detailed(
        ErrorKind::InvalidRecord,
        format!("{field} exceeds {limit} bytes"),
    )
}

fn invalid_request_limit(field: &str, limit: usize) -> Error {
    Error::detailed(
        ErrorKind::InvalidRequest,
        format!("{field} exceeds {limit}"),
    )
}

fn invalid_count(record_base: bool, field: &str, limit: usize) -> Error {
    let kind = if record_base {
        ErrorKind::InvalidRecord
    } else {
        ErrorKind::InvalidRequest
    };
    Error::detailed(kind, format!("{field} exceeds count limit {limit}"))
}

fn invalid_cursor() -> Error {
    Error::detailed(ErrorKind::InvalidRequest, "invalid memory cursor: cursor")
}

/// Lowercase first byte, then lowercase alphanumerics plus `.`, `_` and `-`.
pub fn valid_name(value: &str, max: usize) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > max || !bytes[0].is_ascii_lowercase() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))
}

/// Valid UTF-8 within `max` bytes and free of C0/C1 control characters.
pub fn valid_text(value: &str, max: usize) -> bool {
    if value.len() > max {
        return false;
    }
    !value
        .chars()
        .any(|character| character <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&character))
}

fn valid_semantic(value: &str, max: usize, required: bool) -> bool {
    if !valid_text(value, max) || value != value.trim() {
        return false;
    }
    !required || !value.is_empty()
}

fn valid_cursor(value: &str) -> bool {
    valid_text(value, MAX_CURSOR_BYTES)
}

pub fn valid_timestamp(value: DateTime<Utc>) -> bool {
    value != zero_time() && value.year() >= 1 && value.year() <= 9999
}

fn valid_optional_timestamp(value: Option<DateTime<Utc>>) -> bool {
    value.is_none_or(valid_timestamp)
}

pub fn valid_revision(value: u64) -> bool {
    value >= 1 && value <= i64::MAX as u64
}

fn pending_origin(origin: Option<Origin>) -> bool {
    matches!(
        origin,
        Some(Origin::Model) | Some(Origin::Extractor) | Some(Origin::Import)
    )
}

fn decision_origin(origin: Option<Origin>) -> bool {
    matches!(origin, Some(Origin::Human) | Some(Origin::Migration))
}

pub fn provenance_zero(value: &Provenance) -> bool {
    value.origin.is_none()
        && value.session_id.is_empty()
        && value.message_ids.is_empty()
        && value.observation_id.is_empty()
        && value.decision_at.is_none()
        && value.decision_source.is_none()
}

pub fn validate_scope(scope: &Scope) -> Result<()> {
    if !valid_name(&scope.namespace, MAX_NAMESPACE_BYTES) {
        return Err(invalid_request_limit(
            "scope namespace",
            MAX_NAMESPACE_BYTES,
        ));
    }
    if !valid_opaque_id(&scope.id, MAX_SCOPE_ID_BYTES) {
        return Err(invalid_request_limit("scope ID", MAX_SCOPE_ID_BYTES));
    }
    Ok(())
}

fn validate_provenance(value: &Provenance, active: bool, record_base: bool) -> Result<()> {
    let bad = |field: &str| -> Error {
        let kind = if record_base {
            ErrorKind::InvalidRecord
        } else {
            ErrorKind::InvalidRequest
        };
        Error::detailed(kind, field)
    };
    if value.origin.is_none() {
        return Err(bad("source origin"));
    }
    if !value.session_id.is_empty() && !valid_opaque_id(&value.session_id, MAX_SESSION_ID_BYTES) {
        return Err(bad("source session ID"));
    }
    if !value.observation_id.is_empty() && !valid_opaque_id(&value.observation_id, MAX_ID_BYTES) {
        return Err(bad("source observation ID"));
    }
    if value.message_ids.len() > MAX_PROVENANCE_MESSAGE_IDS {
        return Err(invalid_count(
            record_base,
            "source message ID count",
            MAX_PROVENANCE_MESSAGE_IDS,
        ));
    }
    let mut seen = BTreeSet::new();
    for id in &value.message_ids {
        if !valid_opaque_id(id, MAX_MESSAGE_ID_BYTES) {
            return Err(bad("source message ID"));
        }
        if !seen.insert(id.as_str()) {
            return Err(bad("duplicate source message ID"));
        }
    }
    if value.decision_at.is_none() != value.decision_source.is_none() {
        return Err(bad("source decision pair"));
    }
    if let Some(decided) = value.decision_at
        && (!valid_timestamp(decided) || !decision_origin(value.decision_source))
    {
        return Err(bad("source decision"));
    }
    if active && pending_origin(value.origin) && value.decision_at.is_none() {
        return Err(bad("active source decision"));
    }
    Ok(())
}

fn valid_confidence(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

pub fn validate_record(record: &Record) -> Result<()> {
    if !valid_opaque_id(&record.id, MAX_ID_BYTES) {
        return Err(invalid_record_limit("record ID", MAX_ID_BYTES));
    }
    validate_scope(&record.scope).map_err(|_| invalid_record("record scope"))?;
    if !valid_name(&record.kind, MAX_KIND_BYTES) {
        return Err(invalid_record_limit("record kind", MAX_KIND_BYTES));
    }
    if !valid_semantic(&record.key, MAX_SEMANTIC_KEY_BYTES, false) {
        return Err(invalid_record_limit("record key", MAX_SEMANTIC_KEY_BYTES));
    }
    if !valid_semantic(&record.text, MAX_RECORD_TEXT_BYTES, true) {
        return Err(invalid_record_limit("record text", MAX_RECORD_TEXT_BYTES));
    }
    validate_labels(&record.labels, true)?;
    validate_metadata(&record.metadata, true)?;
    validate_provenance(&record.source, true, true)?;
    if !valid_confidence(record.confidence) {
        return Err(invalid_record("record confidence"));
    }
    if !valid_revision(record.revision) {
        return Err(invalid_record("record revision"));
    }
    if !valid_timestamp(record.created_at)
        || !valid_timestamp(record.updated_at)
        || record.updated_at < record.created_at
    {
        return Err(invalid_record("record timestamps"));
    }
    if !valid_optional_timestamp(record.expires_at)
        || record
            .expires_at
            .is_some_and(|expiry| expiry < record.created_at)
    {
        return Err(invalid_record("record expiry"));
    }
    if record
        .source
        .decision_at
        .is_some_and(|decided| decided > record.updated_at)
    {
        return Err(invalid_record("source decision time"));
    }
    validate_record_lifecycle(record)
}

/// The lifecycle fields: the replacement id belongs to `Superseded` only, the
/// reason is bounded text, and the verification and review times are valid.
fn validate_record_lifecycle(record: &Record) -> Result<()> {
    if record.state == RecordState::Superseded {
        if !valid_opaque_id(&record.superseded_by, MAX_ID_BYTES)
            || record.superseded_by == record.id
        {
            return Err(invalid_record("replacement record ID"));
        }
    } else if !record.superseded_by.is_empty() {
        return Err(invalid_record(
            "replacement record ID without superseded state",
        ));
    }
    if !record.state_reason.is_empty()
        && !valid_semantic(&record.state_reason, MAX_STATE_REASON_BYTES, true)
    {
        return Err(invalid_record_limit("state reason", MAX_STATE_REASON_BYTES));
    }
    if !valid_optional_timestamp(record.verified_at)
        || record
            .verified_at
            .is_some_and(|verified| verified < record.created_at)
    {
        return Err(invalid_record("verification time"));
    }
    if !valid_optional_timestamp(record.review_at) {
        return Err(invalid_record("review time"));
    }
    Ok(())
}

pub fn validate_labels(labels: &[String], record_base: bool) -> Result<()> {
    let bad = |field: &str, limit: Option<usize>| -> Error {
        let kind = if record_base {
            ErrorKind::InvalidRecord
        } else {
            ErrorKind::InvalidRequest
        };
        match limit {
            Some(limit) if record_base => {
                Error::detailed(kind, format!("{field} exceeds {limit} bytes"))
            }
            Some(limit) => Error::detailed(kind, format!("{field} exceeds {limit}")),
            None => Error::detailed(kind, field),
        }
    };
    if labels.len() > MAX_LABELS {
        return Err(invalid_count(record_base, "label count", MAX_LABELS));
    }
    let mut normalized = BTreeSet::new();
    for label in labels {
        if !valid_semantic(label, MAX_LABEL_BYTES, true) {
            return Err(bad("label", Some(MAX_LABEL_BYTES)));
        }
        if !normalized.insert(fold_canonical(label.trim())) {
            return Err(bad("duplicate label", None));
        }
    }
    Ok(())
}

fn presentation_route_only(metadata: &BTreeMap<String, String>) -> bool {
    metadata.is_empty()
        || (metadata.len() == 1
            && metadata
                .get(super::PRESENTATION_ORIGIN_METADATA)
                .is_some_and(|origin| crate::schedule::Origin::parse(origin).is_ok()))
}

pub fn validate_metadata(metadata: &BTreeMap<String, String>, record_base: bool) -> Result<()> {
    let bad = |field: &str, limit: Option<usize>| -> Error {
        let kind = if record_base {
            ErrorKind::InvalidRecord
        } else {
            ErrorKind::InvalidRequest
        };
        match limit {
            Some(limit) if record_base => {
                Error::detailed(kind, format!("{field} exceeds {limit} bytes"))
            }
            Some(limit) => Error::detailed(kind, format!("{field} exceeds {limit}")),
            None => Error::detailed(kind, field),
        }
    };
    if metadata.len() > MAX_METADATA_ENTRIES {
        return Err(invalid_count(
            record_base,
            "metadata entry count",
            MAX_METADATA_ENTRIES,
        ));
    }
    let mut normalized = BTreeSet::new();
    for (key, value) in metadata {
        if !valid_semantic(key, MAX_METADATA_KEY_BYTES, true) {
            return Err(bad("metadata key", Some(MAX_METADATA_KEY_BYTES)));
        }
        if !valid_text(value, MAX_METADATA_VALUE_BYTES) {
            return Err(bad("metadata value", Some(MAX_METADATA_VALUE_BYTES)));
        }
        if !normalized.insert(fold_canonical(key.trim())) {
            return Err(bad("duplicate metadata key", None));
        }
    }
    if super::json::encode_string_map(metadata).len() > MAX_METADATA_BYTES {
        return Err(bad(
            "metadata canonical JSON bytes",
            Some(MAX_METADATA_BYTES),
        ));
    }
    Ok(())
}

/// Case folding for duplicate detection. Lowercasing agrees with a full
/// simple-fold walk for every case pair Otto stores and differs only for exotic
/// orbits such as the Kelvin sign.
fn fold_canonical(value: &str) -> String {
    value.to_lowercase()
}

fn record_zero(record: &Record) -> bool {
    record.id.is_empty()
        && record.kind.is_empty()
        && record.key.is_empty()
        && record.text.is_empty()
        && record.labels.is_empty()
        && record.metadata.is_empty()
        && record.confidence == 0.0
        && record.revision == 0
        && record.created_at == zero_time()
        && record.updated_at == zero_time()
        && record.expires_at.is_none()
}

pub fn validate_proposed_record(record: &Record, action: CandidateAction) -> Result<()> {
    validate_scope(&record.scope).map_err(|_| invalid_record("proposed scope"))?;
    if !record.id.is_empty()
        || record.revision != 0
        || record.created_at != zero_time()
        || record.updated_at != zero_time()
    {
        return Err(invalid_record("proposed persistence fields"));
    }
    if record
        .expires_at
        .is_some_and(|expiry| !valid_timestamp(expiry))
    {
        return Err(invalid_record("proposed expiry"));
    }
    if !pending_origin(record.source.origin)
        || record.source.decision_at.is_some()
        || record.source.decision_source.is_some()
    {
        return Err(invalid_record("proposed source"));
    }
    validate_provenance(&record.source, false, true)?;
    if !action.carries_content() {
        // Forget, retract and needs_review carry no proposed content.
        if !presentation_route_only(&record.metadata) {
            return Err(invalid_record("forget proposed content"));
        }
        let mut copy = record.clone();
        copy.scope = Scope::default();
        copy.source = Provenance::default();
        copy.metadata.clear();
        if !record_zero(&copy) {
            return Err(invalid_record("forget proposed content"));
        }
        return Ok(());
    }
    if !valid_name(&record.kind, MAX_KIND_BYTES) {
        return Err(invalid_record_limit("proposed kind", MAX_KIND_BYTES));
    }
    if !valid_semantic(&record.key, MAX_SEMANTIC_KEY_BYTES, false)
        || !valid_semantic(&record.text, MAX_RECORD_TEXT_BYTES, true)
    {
        return Err(invalid_record("proposed content"));
    }
    validate_labels(&record.labels, true)?;
    validate_metadata(&record.metadata, true)?;
    if !valid_confidence(record.confidence) {
        return Err(invalid_record("proposed confidence"));
    }
    Ok(())
}

fn decided_proposed_empty(record: &Record) -> bool {
    if validate_scope(&record.scope).is_err() {
        return false;
    }
    if record.source != Provenance::default() {
        return false;
    }
    let mut copy = record.clone();
    copy.scope = Scope::default();
    record_zero(&copy)
}

pub fn validate_candidate(candidate: &Candidate) -> Result<()> {
    if !valid_opaque_id(&candidate.id, MAX_ID_BYTES) {
        return Err(invalid_record("candidate ID"));
    }
    if !valid_timestamp(candidate.created_at) {
        return Err(invalid_record("candidate created time"));
    }
    match candidate.action {
        CandidateAction::Create => {
            if !candidate.target_id.is_empty() || candidate.base_revision != 0 {
                return Err(invalid_record("create target"));
            }
        }
        CandidateAction::Update
        | CandidateAction::Forget
        | CandidateAction::Retract
        | CandidateAction::Supersede
        | CandidateAction::Correct
        | CandidateAction::NeedsReview => {
            if !valid_opaque_id(&candidate.target_id, MAX_ID_BYTES)
                || !valid_revision(candidate.base_revision)
            {
                return Err(invalid_record("candidate target"));
            }
        }
    }
    match candidate.state {
        CandidateState::Pending => {
            if candidate.decided_at.is_some()
                || candidate.decision_source.is_some()
                || !candidate.result_record_id.is_empty()
                || candidate.result_revision != 0
            {
                return Err(invalid_record("pending decision fields"));
            }
            validate_proposed_record(&candidate.proposed, candidate.action)?;
            if !valid_semantic(&candidate.reason, MAX_REASON_BYTES, false) {
                return Err(invalid_record_limit("candidate reason", MAX_REASON_BYTES));
            }
        }
        CandidateState::Accepted | CandidateState::Rejected => {
            let decided_ok = candidate
                .decided_at
                .is_some_and(|decided| valid_timestamp(decided) && decided >= candidate.created_at);
            if !decided_ok || !decision_origin(candidate.decision_source) {
                return Err(invalid_record("candidate decision"));
            }
            if !candidate.reason.is_empty() || !decided_proposed_empty(&candidate.proposed) {
                return Err(invalid_record("decided candidate retained content"));
            }
            if candidate.state == CandidateState::Rejected {
                if !candidate.result_record_id.is_empty() || candidate.result_revision != 0 {
                    return Err(invalid_record("rejected result"));
                }
            } else {
                if !valid_revision(candidate.result_revision) {
                    return Err(invalid_record("accepted result revision"));
                }
                if candidate.action == CandidateAction::Create {
                    if !valid_opaque_id(&candidate.result_record_id, MAX_ID_BYTES) {
                        return Err(invalid_record("accepted create result ID"));
                    }
                } else if !candidate.result_record_id.is_empty() {
                    return Err(invalid_record("accepted result ID"));
                }
            }
        }
    }
    Ok(())
}

mod requests;

pub use requests::*;

#[cfg(test)]
mod tests;
