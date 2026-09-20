//! Pending candidates and their review.
//!
//! Observation receipts are not implemented, because automatic memory
//! extraction is out of scope, so a row's `observation_id` is read and carried
//! but never cross-checked against a receipt. The compare-and-swap that
//! protects a review compares the decoded candidate rather than a SHA-256
//! digest of its canonical JSON.

use rusqlite::{Connection, Row, params, params_from_iter, types::Value};

use super::codec::{
    MAX_SOURCE_JSON_BYTES, encode_provenance, format_timestamp, parse_timestamp, valid_stored_float,
};
use super::cursor::{decode_record_cursor, encode_record_cursor, fingerprint_candidates};
use super::query::placeholders;
use super::records::{
    MutationSnapshot, bump_generation, forget_accepted_record, in_read_transaction,
    insert_accepted_record, read_generation, read_mutation_snapshot, update_accepted_record,
};
use super::{Store, map_sqlite_error};
use crate::memory::guard::{guard_candidate, guard_record};
use crate::memory::json::{encode_float, encode_string, encode_string_map, encode_string_slice};
use crate::memory::validate::{
    provenance_zero, validate_candidate, validate_candidate_list_request, validate_candidate_ref,
    validate_proposal_batch, validate_record, validate_store_review_request, validate_tombstone,
};
use crate::memory::{
    Candidate, CandidateAction, CandidateListRequest, CandidatePage, CandidateRef, CandidateState,
    Error, ErrorKind, MAX_CANDIDATE_BATCH, MAX_CANDIDATE_BATCH_BYTES, MAX_ID_BYTES, MAX_KIND_BYTES,
    MAX_LABELS, MAX_METADATA_ENTRIES, MAX_NAMESPACE_BYTES, MAX_PAGE_SIZE,
    MAX_PROVENANCE_MESSAGE_IDS, MAX_REASON_BYTES, MAX_SCOPE_ID_BYTES, Origin, Provenance, Record,
    RecordRef, Result, ReviewDecision, ReviewResult, Scope, StoreReviewRequest, Tombstone,
    invalid_request,
};

const MAX_PROPOSED_JSON_BYTES: usize = 32 * 1024;

fn corrupt() -> Error {
    Error::new(ErrorKind::Corrupt)
}

/// The proposed record as stored in `memory_candidates.proposed_json`.
///
/// A wholly empty provenance stores as `{}` rather than the fully populated
/// object [`encode_provenance`] emits.
pub fn encode_proposed_record(record: &Record) -> Result<String> {
    let source = if provenance_zero(&record.source) {
        "{}".to_string()
    } else {
        let encoded = encode_provenance(&record.source);
        if encoded.len() > MAX_SOURCE_JSON_BYTES {
            return Err(corrupt());
        }
        encoded
    };
    let mut raw = String::from("{\"scope_namespace\":");
    encode_string(&record.scope.namespace, &mut raw);
    raw.push_str(",\"scope_id\":");
    encode_string(&record.scope.id, &mut raw);
    raw.push_str(",\"kind\":");
    encode_string(&record.kind, &mut raw);
    raw.push_str(",\"key\":");
    encode_string(&record.key, &mut raw);
    raw.push_str(",\"text\":");
    encode_string(&record.text, &mut raw);
    raw.push_str(",\"labels\":");
    raw.push_str(&encode_string_slice(&record.labels));
    raw.push_str(",\"metadata\":");
    raw.push_str(&encode_string_map(&record.metadata));
    raw.push_str(",\"source\":");
    raw.push_str(&source);
    raw.push_str(",\"confidence\":");
    raw.push_str(&encode_float(record.confidence));
    raw.push_str(",\"expiry\":");
    match record.expires_at {
        Some(expiry) => encode_string(&format_timestamp(expiry), &mut raw),
        None => raw.push_str("null"),
    }
    raw.push('}');
    if raw.len() > MAX_PROPOSED_JSON_BYTES {
        return Err(corrupt());
    }
    Ok(raw)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposedWire {
    scope_namespace: String,
    scope_id: String,
    kind: String,
    key: String,
    text: String,
    labels: Vec<String>,
    metadata: std::collections::BTreeMap<String, String>,
    source: serde_json::Value,
    confidence: f64,
    expiry: Option<String>,
}

/// The provenance inside a proposed blob.
///
/// [`super::codec::decode_provenance`] cannot be reused here: it compares its
/// input against its own canonical encoding, and a nested value re-serialized
/// out of `serde_json` has its keys in sorted rather than struct-field order.
/// The whole-blob byte comparison in [`decode_proposed_record`] enforces the
/// same canonicality one level up.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvenanceWire {
    origin: String,
    session_id: String,
    message_ids: Vec<String>,
    observation_id: String,
    decision_at: Option<String>,
    decision_source: String,
}

fn decode_origin(value: &str) -> Result<Option<Origin>> {
    if value.is_empty() {
        return Ok(None);
    }
    Origin::parse(value).map(Some).ok_or_else(corrupt)
}

fn decode_proposed_provenance(value: serde_json::Value) -> Result<Provenance> {
    if value.as_object().is_some_and(|object| object.is_empty()) {
        return Ok(Provenance::default());
    }
    let wire: ProvenanceWire = serde_json::from_value(value).map_err(|_| corrupt())?;
    Ok(Provenance {
        origin: decode_origin(&wire.origin)?,
        session_id: wire.session_id,
        message_ids: wire.message_ids,
        observation_id: wire.observation_id,
        decision_at: match &wire.decision_at {
            Some(at) => Some(parse_timestamp(at).map_err(|_| corrupt())?),
            None => None,
        },
        decision_source: decode_origin(&wire.decision_source)?,
    })
}

/// Decodes a stored blob and refuses anything that is not its own canonical
/// encoding, so a hand-edited row is corruption rather than silent input.
pub fn decode_proposed_record(raw: &str) -> Result<Record> {
    if raw.is_empty() || raw.len() > MAX_PROPOSED_JSON_BYTES {
        return Err(corrupt());
    }
    let wire: ProposedWire = serde_json::from_str(raw).map_err(|_| corrupt())?;
    let source = decode_proposed_provenance(wire.source)?;
    let record = Record {
        id: String::new(),
        scope: Scope::new(wire.scope_namespace, wire.scope_id),
        kind: wire.kind,
        key: wire.key,
        text: wire.text,
        labels: wire.labels,
        metadata: wire.metadata,
        source,
        confidence: wire.confidence,
        expires_at: match wire.expiry {
            Some(value) => Some(parse_timestamp(&value).map_err(|_| corrupt())?),
            None => None,
        },
        ..Record::default()
    };
    if encode_proposed_record(&record)? != raw || !valid_stored_float(record.confidence) {
        return Err(corrupt());
    }
    Ok(record)
}

const CANDIDATE_COLUMNS: [&str; 15] = [
    "id",
    "scope_namespace",
    "scope_id",
    "action",
    "target_id",
    "base_revision",
    "observation_id",
    "proposed_json",
    "reason",
    "state",
    "created_at",
    "decided_at",
    "decision_source",
    "result_record_id",
    "result_revision",
];

fn candidate_safety() -> String {
    let json_gate = format!(
        "typeof(proposed_json)='text' AND length(CAST(proposed_json AS BLOB)) BETWEEN 1 AND {MAX_PROPOSED_JSON_BYTES}"
    );
    let bounded = format!("CASE WHEN {json_gate} THEN proposed_json END");
    let stamp = super::codec::TIMESTAMP_BYTES;
    [
        format!("typeof(id)='text' AND length(CAST(id AS BLOB)) BETWEEN 1 AND {MAX_ID_BYTES}"),
        format!(
            "typeof(scope_namespace)='text' AND length(CAST(scope_namespace AS BLOB)) BETWEEN 1 AND {MAX_NAMESPACE_BYTES}"
        ),
        format!(
            "typeof(scope_id)='text' AND length(CAST(scope_id AS BLOB)) BETWEEN 1 AND {MAX_SCOPE_ID_BYTES}"
        ),
        "typeof(action)='text' AND action IN ('create','update','forget')".to_string(),
        format!(
            "typeof(target_id)='text' AND length(CAST(target_id AS BLOB))<={MAX_ID_BYTES}"
        ),
        format!(
            "typeof(base_revision)='integer' AND base_revision BETWEEN 0 AND {}",
            i64::MAX
        ),
        format!(
            "(observation_id IS NULL OR (typeof(observation_id)='text' AND length(CAST(observation_id AS BLOB)) BETWEEN 1 AND {MAX_ID_BYTES}))"
        ),
        json_gate.clone(),
        format!("json_valid({bounded})"),
        format!("json_type({bounded})='object'"),
        format!("typeof(reason)='text' AND length(CAST(reason AS BLOB))<={MAX_REASON_BYTES}"),
        "typeof(state)='text' AND state IN ('pending','accepted','rejected')".to_string(),
        format!("typeof(created_at)='text' AND length(CAST(created_at AS BLOB))={stamp}"),
        format!(
            "(decided_at IS NULL OR (typeof(decided_at)='text' AND length(CAST(decided_at AS BLOB))={stamp}))"
        ),
        format!(
            "typeof(decision_source)='text' AND length(CAST(decision_source AS BLOB))<={MAX_KIND_BYTES}"
        ),
        format!(
            "typeof(result_record_id)='text' AND length(CAST(result_record_id AS BLOB))<={MAX_ID_BYTES}"
        ),
        format!(
            "typeof(result_revision)='integer' AND result_revision BETWEEN 0 AND {}",
            i64::MAX
        ),
    ]
    .join(" AND ")
}

pub fn candidate_projection() -> String {
    let safety = candidate_safety();
    let mut parts = vec![format!("CASE WHEN {safety} THEN 1 ELSE 0 END")];
    for column in CANDIDATE_COLUMNS {
        parts.push(format!("CASE WHEN {safety} THEN {column} END"));
    }
    parts.join(",")
}

/// A candidate plus the observation it came from, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateSnapshot {
    pub candidate: Candidate,
    pub observation_id: String,
}

fn decode_candidate_row(row: &Row<'_>) -> Result<CandidateSnapshot> {
    let valid: Option<i64> = row.get(0).map_err(map_sqlite_error)?;
    if valid != Some(1) {
        return Err(corrupt());
    }
    let string =
        |index: usize| -> Result<Option<String>> { row.get(index).map_err(map_sqlite_error) };
    let required = |index: usize| -> Result<String> { string(index)?.ok_or_else(corrupt) };
    let id = required(1)?;
    let namespace = required(2)?;
    let scope_id = required(3)?;
    let action = CandidateAction::parse(&required(4)?).ok_or_else(corrupt)?;
    let target_id = required(5)?;
    let base_revision: Option<i64> = row.get(6).map_err(map_sqlite_error)?;
    let observation_id = string(7)?;
    let proposed = required(8)?;
    let reason = required(9)?;
    let state = CandidateState::parse(&required(10)?).ok_or_else(corrupt)?;
    let created_at = required(11)?;
    let decided_at = string(12)?;
    let decision_source = required(13)?;
    let result_record_id = required(14)?;
    let result_revision: Option<i64> = row.get(15).map_err(map_sqlite_error)?;
    let (Some(base_revision), Some(result_revision)) = (base_revision, result_revision) else {
        return Err(corrupt());
    };
    if base_revision < 0 || result_revision < 0 {
        return Err(corrupt());
    }
    let mut candidate = Candidate {
        id,
        proposed: decode_proposed_record(&proposed)?,
        action,
        target_id,
        base_revision: base_revision as u64,
        reason,
        state,
        created_at: parse_timestamp(&created_at).map_err(|_| corrupt())?,
        decided_at: match decided_at {
            Some(value) => Some(parse_timestamp(&value).map_err(|_| corrupt())?),
            None => None,
        },
        decision_source: if decision_source.is_empty() {
            None
        } else {
            Some(Origin::parse(&decision_source).ok_or_else(corrupt)?)
        },
        result_record_id,
        result_revision: result_revision as u64,
    };
    // Schema v1 keeps an internal result ID on every accepted row. The neutral
    // API exposes it only for a create, because update and forget already carry
    // the target ID.
    if candidate.state == CandidateState::Accepted && candidate.action != CandidateAction::Create {
        if candidate.result_record_id != candidate.target_id {
            return Err(corrupt());
        }
        candidate.result_record_id = String::new();
    }
    if candidate.proposed.scope != Scope::new(namespace, scope_id)
        || validate_candidate(&candidate).is_err()
    {
        return Err(corrupt());
    }
    Ok(CandidateSnapshot {
        candidate,
        observation_id: observation_id.unwrap_or_default(),
    })
}

/// Rejects a batch whose total text would exceed the wire budget before any of
/// it is encoded, so an oversized proposal cannot reach the database.
fn preflight_batch_shape(candidates: &[Candidate]) -> Result<()> {
    if candidates.is_empty() || candidates.len() > MAX_CANDIDATE_BATCH {
        return Err(invalid_request("candidate batch count"));
    }
    let mut total = 0usize;
    let mut add = |value: &str| -> Result<()> {
        if value.len() > MAX_CANDIDATE_BATCH_BYTES.saturating_sub(total) {
            return Err(invalid_request("candidate batch bytes"));
        }
        total += value.len();
        Ok(())
    };
    for candidate in candidates {
        if candidate.proposed.labels.len() > MAX_LABELS
            || candidate.proposed.metadata.len() > MAX_METADATA_ENTRIES
            || candidate.proposed.source.message_ids.len() > MAX_PROVENANCE_MESSAGE_IDS
        {
            return Err(invalid_request("candidate batch collections"));
        }
        for value in [
            &candidate.id,
            &candidate.target_id,
            &candidate.reason,
            &candidate.proposed.scope.namespace,
            &candidate.proposed.scope.id,
            &candidate.proposed.kind,
            &candidate.proposed.key,
            &candidate.proposed.text,
            &candidate.proposed.source.session_id,
            &candidate.proposed.source.observation_id,
        ] {
            add(value)?;
        }
        for label in &candidate.proposed.labels {
            add(label)?;
        }
        for (key, value) in &candidate.proposed.metadata {
            add(key)?;
            add(value)?;
        }
        for message in &candidate.proposed.source.message_ids {
            add(message)?;
        }
    }
    Ok(())
}

const INSERT_CANDIDATE_SQL: &str = "INSERT INTO memory_candidates(\
id,scope_namespace,scope_id,action,target_id,base_revision,observation_id,proposed_json,reason,state,\
created_at,decided_at,decision_source,result_record_id,result_revision\
) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)";

fn no_rows(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::QueryReturnedNoRows)
}

fn read_candidate_snapshot(
    connection: &Connection,
    reference: &CandidateRef,
) -> Result<CandidateSnapshot> {
    let sql = format!(
        "SELECT {} FROM memory_candidates WHERE scope_namespace=? AND scope_id=? AND id=? LIMIT 1",
        candidate_projection()
    );
    let mut prepared = connection.prepare(&sql).map_err(map_sqlite_error)?;
    let outcome = prepared.query_row(
        params![
            &reference.scope.namespace,
            &reference.scope.id,
            &reference.id
        ],
        |row| Ok(decode_candidate_row(row)),
    );
    let snapshot = match outcome {
        Ok(value) => value?,
        Err(error) if no_rows(&error) => return Err(Error::new(ErrorKind::NotFound)),
        Err(error) => return Err(map_sqlite_error(error)),
    };
    if snapshot.candidate.state == CandidateState::Pending {
        let declared = &snapshot.candidate.proposed.source.observation_id;
        let stored = &snapshot.observation_id;
        if (stored.is_empty() && !declared.is_empty()) || (!stored.is_empty() && declared != stored)
        {
            return Err(corrupt());
        }
    }
    Ok(snapshot)
}

fn build_candidate_list_query(
    request: &CandidateListRequest,
    cursor: Option<&super::cursor::RecordCursor>,
) -> super::query::Statement {
    let mut clauses = Vec::new();
    let mut arguments: Vec<Value> = Vec::new();
    let mut scopes = Vec::new();
    for scope in &request.scopes {
        scopes.push("(scope_namespace=? AND scope_id=?)");
        arguments.push(scope.namespace.clone().into());
        arguments.push(scope.id.clone().into());
    }
    clauses.push(format!("({})", scopes.join(" OR ")));
    if !request.states.is_empty() {
        clauses.push(format!("state IN ({})", placeholders(request.states.len())));
        arguments.extend(
            request
                .states
                .iter()
                .map(|state| Value::Text(state.as_str().to_string())),
        );
    }
    if let Some(cursor) = cursor {
        clauses.push("(created_at<? OR (created_at=? AND id>?))".to_string());
        arguments.push(cursor.updated_at.clone().into());
        arguments.push(cursor.updated_at.clone().into());
        arguments.push(cursor.id.clone().into());
    }
    arguments.push(Value::Integer(request.limit as i64 + 1));
    super::query::Statement {
        sql: format!(
            "SELECT {} FROM memory_candidates WHERE {} ORDER BY created_at DESC,id ASC LIMIT ?",
            candidate_projection(),
            clauses.join(" AND ")
        ),
        arguments,
    }
}

fn conflict_record(id: &str, expected: u64, actual: u64) -> Error {
    Error::conflict("record", id, expected, actual)
}

fn cleared_proposed(scope: &Scope) -> Record {
    Record {
        scope: scope.clone(),
        ..Record::default()
    }
}

fn accepted_record_content(candidate: &Candidate, request: &StoreReviewRequest) -> Record {
    let mut value = request
        .edited
        .clone()
        .unwrap_or_else(|| candidate.proposed.clone());
    value.scope = candidate.proposed.scope.clone();
    value.source.decision_at = Some(request.decided_at);
    value.source.decision_source = request.decision_source;
    value
}

impl Store {
    /// Records pending candidates. Every candidate lands as `pending`: a
    /// proposal never becomes a record without a human review.
    pub fn propose(&self, candidates: &[Candidate]) -> Result<Vec<Candidate>> {
        preflight_batch_shape(candidates)?;
        validate_proposal_batch(candidates)?;
        let mut seen = std::collections::BTreeSet::new();
        for candidate in candidates {
            if !candidate.proposed.source.observation_id.is_empty() {
                return Err(invalid_request("candidate observation"));
            }
            if !seen.insert(candidate.id.clone()) {
                return Err(Error::new(ErrorKind::Conflict));
            }
            guard_candidate(self.guard(), candidate)?;
        }
        let batch: Vec<Candidate> = candidates.to_vec();
        let stored = batch.clone();
        self.with_write(move |connection| {
            for candidate in &stored {
                let proposed = encode_proposed_record(&candidate.proposed)?;
                connection
                    .execute(
                        INSERT_CANDIDATE_SQL,
                        params![
                            &candidate.id,
                            &candidate.proposed.scope.namespace,
                            &candidate.proposed.scope.id,
                            candidate.action.as_str(),
                            &candidate.target_id,
                            candidate.base_revision as i64,
                            None::<String>,
                            &proposed,
                            &candidate.reason,
                            candidate.state.as_str(),
                            format_timestamp(candidate.created_at),
                            None::<String>,
                            "",
                            "",
                            0i64,
                        ],
                    )
                    .map_err(map_sqlite_error)?;
            }
            let generation = bump_generation(connection)?;
            Ok(((), generation))
        })?;
        Ok(batch)
    }

    pub fn get_candidate(&self, reference: &CandidateRef) -> Result<Candidate> {
        validate_candidate_ref(reference)?;
        let snapshot =
            self.with_read(|connection| read_candidate_snapshot(connection, reference))?;
        guard_candidate(self.guard(), &snapshot.candidate)?;
        Ok(snapshot.candidate)
    }

    pub fn list_candidates(&self, request: &CandidateListRequest) -> Result<CandidatePage> {
        validate_candidate_list_request(request)?;
        let fingerprint = fingerprint_candidates(request);
        let cursor = decode_record_cursor(&request.cursor, &fingerprint)?;
        let (generation, candidates) = self.with_read(|connection| {
            in_read_transaction(connection, |connection| {
                let generation = read_generation(connection)?;
                if let Some(cursor) = &cursor
                    && cursor.generation != generation
                {
                    return Err(Error::new(ErrorKind::Conflict));
                }
                if request.scopes.is_empty() {
                    return Ok((generation, Vec::new()));
                }
                let statement = build_candidate_list_query(request, cursor.as_ref());
                let mut prepared = connection
                    .prepare(&statement.sql)
                    .map_err(map_sqlite_error)?;
                let mut rows = prepared
                    .query(params_from_iter(statement.arguments.iter()))
                    .map_err(map_sqlite_error)?;
                let mut candidates = Vec::new();
                while let Some(row) = rows.next().map_err(map_sqlite_error)? {
                    if candidates.len() > MAX_PAGE_SIZE {
                        return Err(corrupt());
                    }
                    candidates.push(decode_candidate_row(row)?.candidate);
                }
                Ok((generation, candidates))
            })
        })?;
        for candidate in &candidates {
            guard_candidate(self.guard(), candidate)?;
        }
        let mut page = CandidatePage {
            candidates,
            next_cursor: String::new(),
        };
        if page.candidates.len() > request.limit {
            page.candidates.truncate(request.limit);
            let last = page.candidates.last().expect("limit is positive");
            page.next_cursor = encode_record_cursor(
                &fingerprint,
                generation,
                &format_timestamp(last.created_at),
                &last.id,
            )?;
        }
        Ok(page)
    }

    /// Applies a human decision to one pending candidate, writing the record it
    /// authorizes and the decision itself in a single transaction.
    pub fn review(&self, request: &StoreReviewRequest) -> Result<ReviewResult> {
        validate_candidate_ref(&request.reference)?;
        let guarded =
            self.with_read(|connection| read_candidate_snapshot(connection, &request.reference))?;
        guard_candidate(self.guard(), &guarded.candidate)?;
        if guarded.candidate.state != CandidateState::Pending {
            return Err(Error::new(ErrorKind::Conflict));
        }
        if let Some(edited) = &request.edited
            && (edited.labels.len() > MAX_LABELS
                || edited.metadata.len() > MAX_METADATA_ENTRIES
                || edited.source.message_ids.len() > MAX_PROVENANCE_MESSAGE_IDS)
        {
            return Err(invalid_request("review edit collections"));
        }
        validate_store_review_request(request, &guarded.candidate)?;
        if let Some(edited) = &request.edited {
            guard_record(self.guard(), edited)?;
        }

        let accepts_existing = request.decision == ReviewDecision::Accept
            && guarded.candidate.action != CandidateAction::Create;
        let target = if accepts_existing {
            let reference = RecordRef {
                scope: guarded.candidate.proposed.scope.clone(),
                id: guarded.candidate.target_id.clone(),
            };
            let snapshot =
                self.with_read(|connection| read_mutation_snapshot(connection, &reference))?;
            let record = match snapshot {
                MutationSnapshot::Forgotten(tombstone) => {
                    return Err(conflict_record(
                        &guarded.candidate.target_id,
                        guarded.candidate.base_revision,
                        tombstone.revision,
                    ));
                }
                MutationSnapshot::Active(record) => record,
            };
            guard_record(self.guard(), &record)?;
            let expected = request
                .target_revision
                .unwrap_or(guarded.candidate.base_revision);
            if record.revision != expected {
                return Err(conflict_record(
                    &guarded.candidate.target_id,
                    expected,
                    record.revision,
                ));
            }
            Some(record)
        } else {
            None
        };

        let mut decided = guarded.candidate.clone();
        decided.state = CandidateState::Rejected;
        decided.decided_at = Some(request.decided_at);
        decided.decision_source = request.decision_source;
        decided.reason = String::new();
        decided.proposed = cleared_proposed(&guarded.candidate.proposed.scope);

        let mut desired_record: Option<Record> = None;
        let mut desired_tombstone: Option<Tombstone> = None;
        if request.decision == ReviewDecision::Accept {
            decided.state = CandidateState::Accepted;
            match guarded.candidate.action {
                CandidateAction::Create => {
                    let mut desired = accepted_record_content(&guarded.candidate, request);
                    desired.id = request.result_record_id.clone();
                    desired.revision = 1;
                    desired.created_at = request.decided_at;
                    desired.updated_at = request.decided_at;
                    validate_record(&desired)?;
                    guard_record(self.guard(), &desired)?;
                    decided.result_record_id = desired.id.clone();
                    decided.result_revision = desired.revision;
                    desired_record = Some(desired);
                }
                CandidateAction::Update => {
                    let target = target.as_ref().expect("update reads its target");
                    if request.decided_at < target.updated_at || target.revision >= i64::MAX as u64
                    {
                        return Err(invalid_request("review decision time"));
                    }
                    let mut desired = accepted_record_content(&guarded.candidate, request);
                    desired.id = target.id.clone();
                    desired.revision = target.revision + 1;
                    desired.created_at = target.created_at;
                    desired.updated_at = request.decided_at;
                    validate_record(&desired)?;
                    guard_record(self.guard(), &desired)?;
                    decided.result_revision = desired.revision;
                    desired_record = Some(desired);
                }
                CandidateAction::Forget => {
                    let target = target.as_ref().expect("forget reads its target");
                    if request.decided_at < target.updated_at || target.revision >= i64::MAX as u64
                    {
                        return Err(invalid_request("review decision time"));
                    }
                    let desired = Tombstone {
                        id: target.id.clone(),
                        scope: target.scope.clone(),
                        revision: target.revision + 1,
                        created_at: target.created_at,
                        updated_at: request.decided_at,
                        forgotten_at: request.decided_at,
                    };
                    validate_tombstone(&desired)?;
                    decided.result_revision = desired.revision;
                    desired_tombstone = Some(desired);
                }
            }
        }
        if validate_candidate(&decided).is_err() {
            return Err(corrupt());
        }
        guard_candidate(self.guard(), &decided)?;

        let baseline = guarded.clone();
        let reference = request.reference.clone();
        let action = guarded.candidate.action;
        let decided_at = request.decided_at;
        let decision_source = request.decision_source;
        let committed = decided.clone();
        let record_to_write = desired_record.clone();
        let tombstone_to_write = desired_tombstone.clone();
        let target_snapshot = target.clone();
        self.with_write(move |connection| {
            let current = read_candidate_snapshot(connection, &reference)?;
            if current.candidate.state != CandidateState::Pending || current != baseline {
                return Err(Error::new(ErrorKind::Conflict));
            }
            if let Some(target) = &target_snapshot {
                let reference =
                    RecordRef { scope: target.scope.clone(), id: target.id.clone() };
                match read_mutation_snapshot(connection, &reference)? {
                    MutationSnapshot::Forgotten(tombstone) => {
                        return Err(conflict_record(
                            &target.id,
                            target.revision,
                            tombstone.revision,
                        ));
                    }
                    MutationSnapshot::Active(current) => {
                        if &current != target {
                            return Err(conflict_record(
                                &target.id,
                                target.revision,
                                current.revision,
                            ));
                        }
                    }
                }
            }
            match (&record_to_write, &tombstone_to_write) {
                (Some(record), _) if action == CandidateAction::Create => {
                    insert_accepted_record(connection, record)?;
                }
                (Some(record), _) => {
                    update_accepted_record(connection, record, record.revision - 1)?;
                }
                (None, Some(tombstone)) => {
                    forget_accepted_record(connection, tombstone, tombstone.revision - 1)?;
                }
                (None, None) => {}
            }
            let cleared = encode_proposed_record(&committed.proposed)?;
            let stored_result_id = if committed.state == CandidateState::Accepted
                && committed.action != CandidateAction::Create
            {
                committed.target_id.clone()
            } else {
                committed.result_record_id.clone()
            };
            let changed = connection
                .execute(
                    "UPDATE memory_candidates SET proposed_json=?,reason='',state=?,decided_at=?,decision_source=?,result_record_id=?,result_revision=? WHERE id=? AND scope_namespace=? AND scope_id=? AND state='pending'",
                    params![
                        &cleared,
                        committed.state.as_str(),
                        format_timestamp(decided_at),
                        decision_source.map(Origin::as_str).unwrap_or(""),
                        &stored_result_id,
                        committed.result_revision as i64,
                        &committed.id,
                        &committed.proposed.scope.namespace,
                        &committed.proposed.scope.id,
                    ],
                )
                .map_err(map_sqlite_error)?;
            if changed != 1 {
                return Err(Error::new(ErrorKind::Conflict));
            }
            let generation = bump_generation(connection)?;
            Ok(((), generation))
        })?;
        Ok(ReviewResult {
            candidate: decided,
            record: desired_record,
            tombstone: desired_tombstone,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::sqlite::records::testsupport::{at, sample_record};
    use crate::memory::sqlite::testsupport::open_temp;
    use crate::memory::{NAMESPACE_USER, UpsertRequest};

    fn proposed(scope: &Scope, key: &str, text: &str) -> Record {
        Record {
            scope: scope.clone(),
            kind: "preference".into(),
            key: key.into(),
            text: text.into(),
            labels: vec!["style".into()],
            source: Provenance {
                origin: Some(Origin::Model),
                ..Provenance::default()
            },
            confidence: 0.5,
            ..Record::default()
        }
    }

    fn candidate(
        id: &str,
        scope: &Scope,
        action: CandidateAction,
        target_id: &str,
        base_revision: u64,
    ) -> Candidate {
        let mut value = Candidate {
            id: id.into(),
            proposed: proposed(scope, "tone", "prefers terse replies"),
            action,
            target_id: target_id.into(),
            base_revision,
            reason: "the user said so".into(),
            state: CandidateState::Pending,
            created_at: at(1),
            decided_at: None,
            decision_source: None,
            result_record_id: String::new(),
            result_revision: 0,
        };
        if action == CandidateAction::Forget {
            value.proposed = Record {
                scope: scope.clone(),
                source: value.proposed.source.clone(),
                ..Record::default()
            };
        }
        value
    }

    fn accept(reference: CandidateRef, result_record_id: &str) -> StoreReviewRequest {
        StoreReviewRequest {
            reference,
            result_record_id: result_record_id.into(),
            decision: ReviewDecision::Accept,
            edited: None,
            target_revision: None,
            decision_source: Some(Origin::Human),
            decided_at: at(2),
        }
    }

    fn user_scope(store: &Store) -> Scope {
        store.identity().expect("identity").user_scope
    }

    fn reference(scope: &Scope, id: &str) -> CandidateRef {
        CandidateRef {
            scope: scope.clone(),
            id: id.into(),
        }
    }

    #[test]
    fn a_proposed_blob_round_trips_and_a_rewritten_blob_is_corrupt() {
        let scope = Scope::new(NAMESPACE_USER, "u1");
        let record = proposed(&scope, "tone", "prefers terse replies");
        let raw = encode_proposed_record(&record).expect("encode");
        assert!(raw.contains("\"confidence\":0.5"));
        assert_eq!(decode_proposed_record(&raw).expect("decode"), record);
        // The stored bytes spell a whole float as `0` where serde_json writes
        // `0.0`; either spelling decodes, so only the byte-equality check
        // separates them.
        let rewritten = raw.replace("\"confidence\":0.5", "\"confidence\":0.50");
        assert!(decode_proposed_record(&rewritten).is_err());
    }

    #[test]
    fn a_create_candidate_is_listed_then_accepted_into_a_record() {
        let (_directory, store) = open_temp();
        let scope = user_scope(&store);
        let proposal = candidate("cand-1", &scope, CandidateAction::Create, "", 0);
        store
            .propose(std::slice::from_ref(&proposal))
            .expect("propose");

        let page = store
            .list_candidates(&CandidateListRequest {
                scopes: vec![scope.clone()],
                states: vec![CandidateState::Pending],
                limit: 10,
                cursor: String::new(),
            })
            .expect("list");
        assert_eq!(page.candidates, vec![proposal.clone()]);
        assert!(page.next_cursor.is_empty());

        let outcome = store
            .review(&accept(reference(&scope, "cand-1"), "rec-1"))
            .expect("review");
        let stored = outcome.record.expect("accepted create writes a record");
        assert_eq!(stored.id, "rec-1");
        assert_eq!(stored.revision, 1);
        assert_eq!(stored.text, proposal.proposed.text);
        assert_eq!(stored.source.decision_source, Some(Origin::Human));
        assert_eq!(
            store.get(&RecordRef {
                scope: scope.clone(),
                id: "rec-1".into()
            }),
            Ok(stored)
        );

        let decided = store
            .get_candidate(&reference(&scope, "cand-1"))
            .expect("get");
        assert_eq!(decided.state, CandidateState::Accepted);
        assert_eq!(decided.result_record_id, "rec-1");
        assert_eq!(decided.reason, "");
        assert_eq!(decided.proposed, cleared_proposed(&scope));
    }

    #[test]
    fn a_rejected_candidate_writes_no_record_and_keeps_no_content() {
        let (_directory, store) = open_temp();
        let scope = user_scope(&store);
        store
            .propose(&[candidate("cand-2", &scope, CandidateAction::Create, "", 0)])
            .expect("propose");
        let mut request = accept(reference(&scope, "cand-2"), "");
        request.decision = ReviewDecision::Reject;
        let outcome = store.review(&request).expect("review");
        assert_eq!(outcome.record, None);
        assert_eq!(outcome.tombstone, None);
        assert_eq!(outcome.candidate.state, CandidateState::Rejected);
        assert_eq!(outcome.candidate.result_record_id, "");
        assert_eq!(outcome.candidate.proposed, cleared_proposed(&scope));
        // A second review of the same candidate finds it already decided.
        assert_eq!(
            store.review(&request).unwrap_err().kind,
            ErrorKind::Conflict
        );
    }

    #[test]
    fn an_accepted_update_advances_its_target_and_an_accepted_forget_tombstones_it() {
        let (_directory, store) = open_temp();
        let scope = user_scope(&store);
        let record = store
            .upsert(&UpsertRequest {
                record: sample_record("rec-9", &scope, "tone", "prefers long replies"),
                expected_revision: None,
            })
            .expect("upsert");

        store
            .propose(&[candidate(
                "cand-3",
                &scope,
                CandidateAction::Update,
                &record.id,
                1,
            )])
            .expect("propose");
        let updated = store
            .review(&accept(reference(&scope, "cand-3"), ""))
            .expect("review")
            .record
            .expect("accepted update writes a record");
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.text, "prefers terse replies");
        assert_eq!(updated.created_at, record.created_at);
        // The result ID is cleared on the wire for a non-create acceptance.
        assert_eq!(
            store
                .get_candidate(&reference(&scope, "cand-3"))
                .expect("get")
                .result_record_id,
            ""
        );

        store
            .propose(&[candidate(
                "cand-4",
                &scope,
                CandidateAction::Forget,
                &record.id,
                2,
            )])
            .expect("propose");
        let tombstone = store
            .review(&accept(reference(&scope, "cand-4"), ""))
            .expect("review")
            .tombstone
            .expect("accepted forget writes a tombstone");
        assert_eq!(tombstone.revision, 3);
        assert_eq!(
            store
                .get(&RecordRef {
                    scope,
                    id: record.id
                })
                .unwrap_err()
                .kind,
            ErrorKind::NotFound
        );
    }

    #[test]
    fn an_accepted_update_whose_target_moved_is_a_conflict() {
        let (_directory, store) = open_temp();
        let scope = user_scope(&store);
        let record = store
            .upsert(&UpsertRequest {
                record: sample_record("rec-8", &scope, "tone", "prefers long replies"),
                expected_revision: None,
            })
            .expect("upsert");
        store
            .propose(&[candidate(
                "cand-5",
                &scope,
                CandidateAction::Update,
                &record.id,
                7,
            )])
            .expect("propose");
        let error = store
            .review(&accept(reference(&scope, "cand-5"), ""))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Conflict);
        assert_eq!(error.conflict.expect("conflict detail").actual_revision, 1);
    }
}
