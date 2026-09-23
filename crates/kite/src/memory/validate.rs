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
/// simple-fold walk for every case pair Kite stores and differs only for exotic
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
    if action == CandidateAction::Forget {
        let mut copy = record.clone();
        copy.scope = Scope::default();
        copy.source = Provenance::default();
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
        CandidateAction::Update | CandidateAction::Forget => {
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

fn validate_scopes(scopes: &[Scope], allow_empty: bool, phase_one: bool) -> Result<()> {
    if scopes.is_empty() && !allow_empty {
        return Err(invalid_request("scope count"));
    }
    if scopes.len() > MAX_REQUEST_SCOPES {
        return Err(invalid_count(false, "scope count", MAX_REQUEST_SCOPES));
    }
    let mut seen = BTreeSet::new();
    let (mut users, mut workspaces) = (0usize, 0usize);
    for scope in scopes {
        validate_scope(scope)?;
        if !seen.insert(scope.clone()) {
            return Err(invalid_request("duplicate scope"));
        }
        if phase_one {
            match scope.namespace.as_str() {
                NAMESPACE_USER => users += 1,
                NAMESPACE_WORKSPACE => workspaces += 1,
                _ => return Err(invalid_request("phase 1 namespace")),
            }
        }
    }
    if phase_one && (users != 1 || workspaces > 1) {
        return Err(invalid_request("phase 1 scope set"));
    }
    Ok(())
}

fn validate_filters(kinds: &[String], labels: &[String]) -> Result<()> {
    if kinds.len() > MAX_REQUEST_KINDS {
        return Err(invalid_count(false, "kind filter count", MAX_REQUEST_KINDS));
    }
    let mut seen = BTreeSet::new();
    for kind in kinds {
        if !valid_name(kind, MAX_KIND_BYTES) {
            return Err(invalid_request("kind filter"));
        }
        if !seen.insert(kind.as_str()) {
            return Err(invalid_request("duplicate kind filter"));
        }
    }
    if labels.len() > MAX_REQUEST_LABELS {
        return Err(invalid_count(
            false,
            "label filter count",
            MAX_REQUEST_LABELS,
        ));
    }
    let mut seen = BTreeSet::new();
    for label in labels {
        if !valid_semantic(label, MAX_LABEL_BYTES, true) {
            return Err(invalid_request_limit("label filter", MAX_LABEL_BYTES));
        }
        if !seen.insert(label.as_str()) {
            return Err(invalid_request("duplicate label filter"));
        }
    }
    Ok(())
}

fn validate_page(limit: usize, cursor: &str, max: usize) -> Result<()> {
    if limit == 0 || limit > max {
        return Err(invalid_request_limit("page limit", max));
    }
    if !valid_cursor(cursor) {
        return Err(invalid_cursor());
    }
    Ok(())
}

fn validate_now(now: DateTime<Utc>, include_expired: bool) -> Result<()> {
    if include_expired && now == zero_time() {
        return Ok(());
    }
    if !valid_timestamp(now) {
        return Err(invalid_request("request time"));
    }
    Ok(())
}

pub fn validate_list_request(request: &ListRequest) -> Result<()> {
    validate_scopes(&request.scopes, true, false)?;
    validate_filters(&request.kinds, &request.labels)?;
    validate_page(request.limit, &request.cursor, MAX_PAGE_SIZE)?;
    validate_now(request.now, request.include_expired)
}

pub fn validate_candidate_list_request(request: &CandidateListRequest) -> Result<()> {
    validate_scopes(&request.scopes, true, false)?;
    if request.states.len() > MAX_REQUEST_KINDS {
        return Err(invalid_request("candidate state count"));
    }
    let mut seen = BTreeSet::new();
    for state in &request.states {
        if !seen.insert(*state) {
            return Err(invalid_request("duplicate candidate state"));
        }
    }
    validate_page(request.limit, &request.cursor, MAX_PAGE_SIZE)
}

fn validate_query(value: &str, allow_empty: bool) -> Result<()> {
    if value.len() <= MAX_QUERY_BYTES && value.trim().is_empty() {
        if allow_empty {
            return Ok(());
        }
        return Err(invalid_request_limit("query", MAX_QUERY_BYTES));
    }
    if !valid_semantic(value, MAX_QUERY_BYTES, true) {
        return Err(invalid_request_limit("query", MAX_QUERY_BYTES));
    }
    Ok(())
}

fn validate_budget(limit: usize, budget: usize, max_limit: usize) -> Result<()> {
    if limit == 0 || limit > max_limit {
        return Err(invalid_request_limit("result limit", max_limit));
    }
    if budget == 0 || budget > MAX_TOKEN_BUDGET {
        return Err(invalid_request_limit("token budget", MAX_TOKEN_BUDGET));
    }
    Ok(())
}

pub fn validate_retrieval_request(request: &RetrievalRequest) -> Result<()> {
    validate_query(&request.query, true)?;
    validate_scopes(&request.scopes, false, true)?;
    validate_filters(&request.kinds, &request.labels)?;
    validate_budget(request.limit, request.token_budget, MAX_PAGE_SIZE)?;
    validate_now(request.now, request.include_expired)
}

pub fn validate_search_request(request: &SearchRequest) -> Result<()> {
    validate_query(&request.query, true)?;
    validate_scopes(&request.scopes, false, true)?;
    validate_filters(&request.kinds, &request.labels)?;
    validate_budget(request.limit, request.token_budget, MAX_PAGE_SIZE)?;
    if !valid_cursor(&request.cursor) {
        return Err(invalid_cursor());
    }
    if !request.include_candidates && !request.candidate_states.is_empty() {
        return Err(invalid_request("candidate states require inclusion"));
    }
    let mut seen = BTreeSet::new();
    for state in &request.candidate_states {
        if !seen.insert(*state) {
            return Err(invalid_request("duplicate candidate state"));
        }
    }
    validate_now(request.now, request.include_expired)
}

#[allow(clippy::too_many_arguments)]
fn validate_transient_content(
    scope: &Scope,
    kind: &str,
    key: &str,
    text: &str,
    labels: &[String],
    metadata: &BTreeMap<String, String>,
    confidence: f64,
    expires: Option<DateTime<Utc>>,
    source: &Provenance,
    allowed_origin: fn(Option<Origin>) -> bool,
) -> Result<()> {
    validate_scope(scope)?;
    if !valid_name(kind, MAX_KIND_BYTES)
        || !valid_semantic(key, MAX_SEMANTIC_KEY_BYTES, false)
        || !valid_semantic(text, MAX_RECORD_TEXT_BYTES, true)
    {
        return Err(invalid_request("content fields"));
    }
    validate_labels(labels, false)?;
    validate_metadata(metadata, false)?;
    if !valid_confidence(confidence) {
        return Err(invalid_request("confidence"));
    }
    if !valid_optional_timestamp(expires) {
        return Err(invalid_request("expiry"));
    }
    if !allowed_origin(source.origin) {
        return Err(invalid_request("source origin"));
    }
    validate_provenance(source, false, false)
}

pub fn validate_remember_request(request: &RememberRequest) -> Result<()> {
    let mut source = request.source.clone();
    if provenance_zero(&source) {
        source.origin = Some(Origin::Human);
    }
    validate_transient_content(
        &request.scope,
        &request.kind,
        &request.key,
        &request.text,
        &request.labels,
        &request.metadata,
        request.confidence,
        request.expires_at,
        &source,
        decision_origin,
    )?;
    if source.decision_at.is_some() {
        return Err(invalid_request("manager source decision"));
    }
    match request.expected_revision {
        None => {
            if !request.id.is_empty() {
                return Err(invalid_request("create ID"));
            }
        }
        Some(expected) => {
            let identified = !request.id.is_empty() || !request.key.is_empty();
            let id_ok = request.id.is_empty() || valid_opaque_id(&request.id, MAX_ID_BYTES);
            if !valid_revision(expected) || !identified || !id_ok {
                return Err(invalid_request("update identity and revision"));
            }
        }
    }
    Ok(())
}

pub fn validate_propose_request(request: &ProposeRequest) -> Result<()> {
    if !valid_semantic(&request.reason, MAX_REASON_BYTES, false) {
        return Err(invalid_request_limit("proposal reason", MAX_REASON_BYTES));
    }
    if !pending_origin(request.source.origin)
        || request.source.decision_at.is_some()
        || request.source.decision_source.is_some()
    {
        return Err(invalid_request("proposal source origin or decision"));
    }
    if request.action == CandidateAction::Forget {
        validate_scope(&request.scope)?;
        if !valid_opaque_id(&request.target_id, MAX_ID_BYTES)
            || !valid_revision(request.base_revision)
        {
            return Err(invalid_request("forget proposal target"));
        }
        if !request.kind.is_empty()
            || !request.key.is_empty()
            || !request.text.is_empty()
            || !request.labels.is_empty()
            || !request.metadata.is_empty()
            || request.confidence != 0.0
            || request.expires_at.is_some()
        {
            return Err(invalid_request("forget proposal content"));
        }
        return validate_provenance(&request.source, false, false);
    }
    validate_transient_content(
        &request.scope,
        &request.kind,
        &request.key,
        &request.text,
        &request.labels,
        &request.metadata,
        request.confidence,
        request.expires_at,
        &request.source,
        pending_origin,
    )?;
    match request.action {
        CandidateAction::Create => {
            if !request.target_id.is_empty() || request.base_revision != 0 {
                return Err(invalid_request("create proposal target"));
            }
        }
        CandidateAction::Update => {
            if !valid_opaque_id(&request.target_id, MAX_ID_BYTES)
                || !valid_revision(request.base_revision)
            {
                return Err(invalid_request("update proposal target"));
            }
        }
        CandidateAction::Forget => unreachable!("handled above"),
    }
    Ok(())
}

pub fn validate_forget_request(request: &ForgetRequest) -> Result<()> {
    validate_record_ref(&request.reference)?;
    if !valid_revision(request.expected_revision) {
        return Err(invalid_request("expected revision"));
    }
    if request.purge_backups && !request.confirm_purge {
        return Err(invalid_request("purge confirmation"));
    }
    Ok(())
}

pub fn validate_record_ref(reference: &RecordRef) -> Result<()> {
    validate_scope(&reference.scope)?;
    if !valid_opaque_id(&reference.id, MAX_ID_BYTES) {
        return Err(invalid_request("record ID"));
    }
    Ok(())
}

pub fn validate_candidate_ref(reference: &CandidateRef) -> Result<()> {
    validate_scope(&reference.scope)?;
    if !valid_opaque_id(&reference.id, MAX_ID_BYTES) {
        return Err(invalid_request("candidate ID"));
    }
    Ok(())
}

pub fn validate_record_key(key: &RecordKey) -> Result<()> {
    validate_scope(&key.scope)?;
    if !valid_name(&key.kind, MAX_KIND_BYTES)
        || !valid_semantic(&key.key, MAX_SEMANTIC_KEY_BYTES, true)
    {
        return Err(invalid_request("record key"));
    }
    Ok(())
}

pub fn validate_upsert_request(request: &UpsertRequest) -> Result<()> {
    match request.expected_revision {
        None => {
            if request.record.revision != 0 {
                return Err(invalid_request("create revision"));
            }
            let mut record = request.record.clone();
            record.revision = 1;
            validate_record(&record)
        }
        Some(expected) => {
            if !valid_revision(expected) || request.record.revision != expected {
                return Err(invalid_request("expected revision"));
            }
            validate_record(&request.record)
        }
    }
}

pub fn validate_store_forget_request(request: &StoreForgetRequest) -> Result<()> {
    validate_record_ref(&request.reference)?;
    if !valid_revision(request.expected_revision) || !valid_timestamp(request.forgotten_at) {
        return Err(invalid_request("forget revision or time"));
    }
    Ok(())
}

pub fn validate_proposal_batch(candidates: &[Candidate]) -> Result<()> {
    if candidates.is_empty() {
        return Err(invalid_request("candidate batch count"));
    }
    if candidates.len() > MAX_CANDIDATE_BATCH {
        return Err(invalid_count(
            false,
            "candidate batch count",
            MAX_CANDIDATE_BATCH,
        ));
    }
    for candidate in candidates {
        validate_candidate(candidate)?;
        if candidate.state != CandidateState::Pending {
            return Err(invalid_request("candidate batch state"));
        }
    }
    Ok(())
}

pub fn validate_store_review_request(
    request: &StoreReviewRequest,
    candidate: &Candidate,
) -> Result<()> {
    validate_candidate_ref(&request.reference)?;
    validate_candidate(candidate)?;
    if candidate.state != CandidateState::Pending {
        return Err(invalid_request("review candidate state"));
    }
    if request.reference.scope != candidate.proposed.scope || request.reference.id != candidate.id {
        return Err(invalid_request("review candidate reference"));
    }
    if !decision_origin(request.decision_source) || !valid_timestamp(request.decided_at) {
        return Err(invalid_request("review decision"));
    }
    if !request.result_record_id.is_empty()
        && !valid_opaque_id(&request.result_record_id, MAX_ID_BYTES)
    {
        return Err(invalid_request("review result ID"));
    }
    if request
        .target_revision
        .is_some_and(|revision| !valid_revision(revision))
    {
        return Err(invalid_request("review target revision"));
    }
    if request.decided_at < candidate.created_at {
        return Err(invalid_request("review decision time"));
    }
    if request.decision == ReviewDecision::Reject {
        if request.edited.is_some()
            || !request.result_record_id.is_empty()
            || request.target_revision.is_some()
        {
            return Err(invalid_request("rejected review fields"));
        }
        return Ok(());
    }
    if candidate.action == CandidateAction::Forget && request.edited.is_some() {
        return Err(invalid_request("forget edit"));
    }
    if candidate.action == CandidateAction::Create {
        if !valid_opaque_id(&request.result_record_id, MAX_ID_BYTES)
            || request.target_revision.is_some()
        {
            return Err(invalid_request("accepted create fields"));
        }
    } else {
        if !request.result_record_id.is_empty() {
            return Err(invalid_request("accepted target result ID"));
        }
        let target = request.target_revision.unwrap_or(candidate.base_revision);
        if candidate.action == CandidateAction::Update
            && target != candidate.base_revision
            && request.edited.is_none()
        {
            return Err(invalid_request("review rebase edit"));
        }
    }
    if let Some(edited) = &request.edited {
        if candidate.action == CandidateAction::Forget || edited.scope != candidate.proposed.scope {
            return Err(invalid_request("review edit scope"));
        }
        validate_proposed_record(edited, candidate.action)?;
    }
    Ok(())
}

pub fn validate_review_request(request: &ReviewRequest) -> Result<()> {
    validate_candidate_ref(&request.reference)?;
    if request.decision == ReviewDecision::Reject
        && (request.edited.is_some() || request.target_revision.is_some())
    {
        return Err(invalid_request("rejected review fields"));
    }
    if request
        .target_revision
        .is_some_and(|revision| !valid_revision(revision))
    {
        return Err(invalid_request("target revision"));
    }
    if let Some(edited) = &request.edited {
        if edited.scope != request.reference.scope {
            return Err(invalid_request("review edit scope"));
        }
        validate_proposed_record(edited, CandidateAction::Update)?;
    }
    Ok(())
}

pub fn validate_tombstone(tombstone: &Tombstone) -> Result<()> {
    if !valid_opaque_id(&tombstone.id, MAX_ID_BYTES) {
        return Err(invalid_record("tombstone ID"));
    }
    validate_scope(&tombstone.scope).map_err(|_| invalid_record("tombstone scope"))?;
    if !valid_revision(tombstone.revision)
        || !valid_timestamp(tombstone.created_at)
        || !valid_timestamp(tombstone.updated_at)
        || !valid_timestamp(tombstone.forgotten_at)
    {
        return Err(invalid_record("tombstone revision or time"));
    }
    if tombstone.updated_at < tombstone.created_at
        || tombstone.forgotten_at < tombstone.created_at
        || tombstone.forgotten_at > tombstone.updated_at
    {
        return Err(invalid_record("tombstone time relationship"));
    }
    Ok(())
}

pub fn validate_recall_request(request: &RecallRequest) -> Result<()> {
    validate_query(&request.query, false)?;
    validate_filters(&request.kinds, &[])?;
    validate_budget(request.limit, request.token_budget, MAX_RECALL_RECORDS)
}

pub fn validate_bind_scopes(scopes: &[Scope], default_write_scope: &Scope) -> Result<()> {
    validate_scopes(scopes, false, true)?;
    validate_scope(default_write_scope)?;
    if scopes.contains(default_write_scope) {
        return Ok(());
    }
    Err(invalid_request("default write scope"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp")
    }

    fn active_record() -> Record {
        Record {
            id: "a1".into(),
            scope: Scope::new(NAMESPACE_USER, "install"),
            kind: "preference".into(),
            key: "editor".into(),
            text: "prefers tabs".into(),
            source: Provenance {
                origin: Some(Origin::Human),
                ..Provenance::default()
            },
            confidence: 1.0,
            revision: 1,
            created_at: now(),
            updated_at: now(),
            ..Record::default()
        }
    }

    #[test]
    fn valid_name_accepts_only_the_name_alphabet() {
        assert!(valid_name("preference", MAX_KIND_BYTES));
        assert!(valid_name("a.b_c-1", MAX_KIND_BYTES));
        assert!(!valid_name("", MAX_KIND_BYTES));
        assert!(!valid_name("Preference", MAX_KIND_BYTES));
        assert!(!valid_name("1abc", MAX_KIND_BYTES));
        assert!(!valid_name("a b", MAX_KIND_BYTES));
    }

    #[test]
    fn text_rejects_control_characters() {
        assert!(valid_text("plain text", 32));
        assert!(!valid_text("with\ttab", 32));
        assert!(!valid_text("with\u{80}c1", 32));
        assert!(!valid_text("toolong", 3));
    }

    #[test]
    fn record_validation_accepts_a_well_formed_record() {
        validate_record(&active_record()).expect("valid record");
    }

    #[test]
    fn record_validation_rejects_each_broken_field() {
        for (mutate, detail) in [
            (
                (|r: &mut Record| r.id = String::new()) as fn(&mut Record),
                "record ID exceeds 64 bytes",
            ),
            (|r| r.kind = "Bad".into(), "record kind exceeds 32 bytes"),
            (|r| r.text = String::new(), "record text exceeds 8192 bytes"),
            (|r| r.confidence = 2.0, "record confidence"),
            (|r| r.revision = 0, "record revision"),
            (|r| r.created_at = zero_time(), "record timestamps"),
        ] {
            let mut record = active_record();
            mutate(&mut record);
            let error = validate_record(&record).expect_err(detail);
            assert_eq!(error.kind, ErrorKind::InvalidRecord);
            assert_eq!(error.detail.as_deref(), Some(detail));
        }
    }

    #[test]
    fn active_pending_origin_requires_a_decision() {
        let mut record = active_record();
        record.source.origin = Some(Origin::Model);
        let error = validate_record(&record).expect_err("undecided model record");
        assert_eq!(error.detail.as_deref(), Some("active source decision"));
    }

    #[test]
    fn duplicate_labels_and_metadata_keys_are_rejected() {
        let mut record = active_record();
        record.labels = vec!["Tabs".into(), "tabs".into()];
        assert_eq!(
            validate_record(&record)
                .expect_err("labels")
                .detail
                .as_deref(),
            Some("duplicate label")
        );
    }

    #[test]
    fn scope_sets_must_carry_exactly_one_user_scope() {
        let user = Scope::new(NAMESPACE_USER, "install");
        let workspace = Scope::new(NAMESPACE_WORKSPACE, "sha256:abc");
        validate_scopes(&[user.clone(), workspace.clone()], false, true).expect("phase one");
        assert!(validate_scopes(std::slice::from_ref(&workspace), false, true).is_err());
        assert!(validate_scopes(&[user.clone(), user], false, true).is_err());
        assert!(validate_scopes(&[], false, true).is_err());
    }

    #[test]
    fn remember_requests_default_to_a_human_origin() {
        let request = RememberRequest {
            scope: Scope::new(NAMESPACE_USER, "install"),
            kind: "preference".into(),
            text: "prefers tabs".into(),
            ..RememberRequest::default()
        };
        validate_remember_request(&request).expect("human default");

        let mut model = request.clone();
        model.source.origin = Some(Origin::Model);
        assert_eq!(
            validate_remember_request(&model)
                .expect_err("model")
                .detail
                .as_deref(),
            Some("source origin")
        );
    }

    #[test]
    fn remember_update_requires_an_identity() {
        let mut request = RememberRequest {
            scope: Scope::new(NAMESPACE_USER, "install"),
            kind: "preference".into(),
            text: "prefers tabs".into(),
            expected_revision: Some(1),
            ..RememberRequest::default()
        };
        assert!(validate_remember_request(&request).is_err());
        request.key = "editor".into();
        validate_remember_request(&request).expect("keyed update");
    }

    #[test]
    fn propose_requests_require_a_pending_origin() {
        let mut request = ProposeRequest {
            action: CandidateAction::Create,
            scope: Scope::new(NAMESPACE_USER, "install"),
            kind: "preference".into(),
            text: "prefers tabs".into(),
            source: Provenance {
                origin: Some(Origin::Model),
                ..Provenance::default()
            },
            ..ProposeRequest::default()
        };
        validate_propose_request(&request).expect("model proposal");
        request.source.origin = Some(Origin::Human);
        assert_eq!(
            validate_propose_request(&request)
                .expect_err("human")
                .detail
                .as_deref(),
            Some("proposal source origin or decision")
        );
    }

    #[test]
    fn forget_proposals_carry_no_content() {
        let mut request = ProposeRequest {
            action: CandidateAction::Forget,
            scope: Scope::new(NAMESPACE_USER, "install"),
            target_id: "a1".into(),
            base_revision: 1,
            source: Provenance {
                origin: Some(Origin::Model),
                ..Provenance::default()
            },
            ..ProposeRequest::default()
        };
        validate_propose_request(&request).expect("forget proposal");
        request.text = "leftover".into();
        assert_eq!(
            validate_propose_request(&request)
                .expect_err("content")
                .detail
                .as_deref(),
            Some("forget proposal content")
        );
    }

    #[test]
    fn page_limits_and_budgets_are_bounded() {
        assert!(validate_page(0, "", MAX_PAGE_SIZE).is_err());
        assert!(validate_page(MAX_PAGE_SIZE + 1, "", MAX_PAGE_SIZE).is_err());
        validate_page(1, "", MAX_PAGE_SIZE).expect("valid page");
        assert!(validate_budget(1, MAX_TOKEN_BUDGET + 1, MAX_PAGE_SIZE).is_err());
        validate_budget(1, 100, MAX_PAGE_SIZE).expect("valid budget");
    }
}
