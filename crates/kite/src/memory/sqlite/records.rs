//! Record reads and mutations.
//!
//! Every read decodes through the gated projection in [`super::query`], so a
//! row whose stored shape is wrong becomes [`ErrorKind::Corrupt`] instead of a
//! plausible record.
//!
//! The compare-and-swap that protects a mutation against a concurrent writer
//! compares the decoded record rather than a SHA-256 digest of its canonical
//! JSON. The digest would never leave the process, and the two detect exactly
//! the same changes.

use rusqlite::{Connection, Row, params, params_from_iter};

use super::codec::{
    encode_record, format_timestamp, fts_labels, parse_timestamp, valid_stored_float,
};
use super::cursor::{decode_record_cursor, encode_record_cursor, fingerprint_list};
use super::query::{Statement, build_list_query, record_projection, tombstone_projection};
use super::{Store, map_sqlite_error};
use crate::memory::guard::guard_record;
use crate::memory::validate::{
    validate_list_request, validate_record, validate_record_key, validate_record_ref,
    validate_store_forget_request, validate_tombstone, validate_upsert_request,
};
use crate::memory::{
    Error, ErrorKind, ListRequest, MAX_ID_BYTES, MAX_PAGE_SIZE, Record, RecordKey, RecordPage,
    RecordRef, Result, Scope, StoreForgetRequest, Tombstone, UpsertRequest, invalid_request,
};
use crate::memory::{GuardField, GuardInput};

fn corrupt() -> Error {
    Error::new(ErrorKind::Corrupt)
}

fn text(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<String>> {
    row.get(index)
}

/// Decodes one row of [`record_projection`].
pub fn decode_record_row(row: &Row<'_>) -> Result<Record> {
    let valid: Option<i64> = row.get(0).map_err(map_sqlite_error)?;
    if valid != Some(1) {
        return Err(corrupt());
    }
    let mut strings = Vec::with_capacity(9);
    for index in 1..=9 {
        strings.push(
            text(row, index)
                .map_err(map_sqlite_error)?
                .ok_or_else(corrupt)?,
        );
    }
    let confidence: Option<f64> = row.get(10).map_err(map_sqlite_error)?;
    let revision: Option<i64> = row.get(11).map_err(map_sqlite_error)?;
    let created = text(row, 12)
        .map_err(map_sqlite_error)?
        .ok_or_else(corrupt)?;
    let updated = text(row, 13)
        .map_err(map_sqlite_error)?
        .ok_or_else(corrupt)?;
    let expires = text(row, 14).map_err(map_sqlite_error)?;
    let (confidence, revision) = match (confidence, revision) {
        (Some(confidence), Some(revision)) if revision >= 0 => (confidence, revision as u64),
        _ => return Err(corrupt()),
    };
    let record = Record {
        id: strings[0].clone(),
        scope: Scope::new(strings[1].clone(), strings[2].clone()),
        kind: strings[3].clone(),
        key: strings[4].clone(),
        text: strings[5].clone(),
        labels: super::codec::decode_labels(&strings[6]).map_err(|_| corrupt())?,
        metadata: super::codec::decode_metadata(&strings[7]).map_err(|_| corrupt())?,
        source: super::codec::decode_provenance(&strings[8]).map_err(|_| corrupt())?,
        confidence,
        revision,
        created_at: parse_timestamp(&created).map_err(|_| corrupt())?,
        updated_at: parse_timestamp(&updated).map_err(|_| corrupt())?,
        expires_at: match expires {
            Some(value) => Some(parse_timestamp(&value).map_err(|_| corrupt())?),
            None => None,
        },
    };
    if !valid_stored_float(record.confidence) || validate_record(&record).is_err() {
        return Err(corrupt());
    }
    Ok(record)
}

/// Decodes one row of [`tombstone_projection`].
pub fn decode_tombstone_row(row: &Row<'_>) -> Result<Tombstone> {
    let valid: Option<i64> = row.get(0).map_err(map_sqlite_error)?;
    if valid != Some(1) {
        return Err(corrupt());
    }
    let id = text(row, 1)
        .map_err(map_sqlite_error)?
        .ok_or_else(corrupt)?;
    let namespace = text(row, 2)
        .map_err(map_sqlite_error)?
        .ok_or_else(corrupt)?;
    let scope_id = text(row, 3)
        .map_err(map_sqlite_error)?
        .ok_or_else(corrupt)?;
    let revision: Option<i64> = row.get(4).map_err(map_sqlite_error)?;
    let created = text(row, 5)
        .map_err(map_sqlite_error)?
        .ok_or_else(corrupt)?;
    let updated = text(row, 6)
        .map_err(map_sqlite_error)?
        .ok_or_else(corrupt)?;
    let forgotten = text(row, 7)
        .map_err(map_sqlite_error)?
        .ok_or_else(corrupt)?;
    let Some(revision) = revision.filter(|value| *value >= 0) else {
        return Err(corrupt());
    };
    let value = Tombstone {
        id,
        scope: Scope::new(namespace, scope_id),
        revision: revision as u64,
        created_at: parse_timestamp(&created).map_err(|_| corrupt())?,
        updated_at: parse_timestamp(&updated).map_err(|_| corrupt())?,
        forgotten_at: parse_timestamp(&forgotten).map_err(|_| corrupt())?,
    };
    if validate_tombstone(&value).is_err() {
        return Err(corrupt());
    }
    Ok(value)
}

fn no_rows(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::QueryReturnedNoRows)
}

/// Runs `body` inside a deferred read transaction so the generation and the
/// rows it labels come from one snapshot.
pub(crate) fn in_read_transaction<T>(
    connection: &Connection,
    body: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    connection
        .execute_batch("BEGIN")
        .map_err(map_sqlite_error)?;
    let outcome = body(connection);
    let end = connection.execute_batch("COMMIT").map_err(map_sqlite_error);
    match outcome {
        Ok(value) => end.map(|()| value),
        Err(error) => Err(error),
    }
}

fn query_record(connection: &Connection, statement: &Statement) -> Result<Record> {
    let mut prepared = connection
        .prepare(&statement.sql)
        .map_err(map_sqlite_error)?;
    let outcome = prepared.query_row(params_from_iter(statement.arguments.iter()), |row| {
        Ok(decode_record_row(row))
    });
    match outcome {
        Ok(record) => record,
        Err(error) if no_rows(&error) => Err(Error::new(ErrorKind::NotFound)),
        Err(error) => Err(map_sqlite_error(error)),
    }
}

pub fn read_generation(connection: &Connection) -> Result<u64> {
    let raw: Option<String> = connection
        .query_row(
            "SELECT CASE WHEN typeof(value)='text' AND length(CAST(value AS BLOB)) BETWEEN 1 AND 20 THEN value END FROM memory_meta WHERE key='generation' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .map_err(|error| if no_rows(&error) { corrupt() } else { map_sqlite_error(error) })?;
    let raw = raw.ok_or_else(corrupt)?;
    let generation: u64 = raw.parse().map_err(|_| corrupt())?;
    if generation.to_string() != raw {
        return Err(corrupt());
    }
    Ok(generation)
}

/// Increments the generation counter, refusing to guess when the row it was
/// about to replace is no longer the one it read.
pub fn bump_generation(connection: &Connection) -> Result<u64> {
    let generation = read_generation(connection)?;
    let next = generation.checked_add(1).ok_or_else(corrupt)?;
    let changed = connection
        .execute(
            "UPDATE memory_meta SET value=? WHERE key='generation' AND value=?",
            params![next.to_string(), generation.to_string()],
        )
        .map_err(map_sqlite_error)?;
    if changed != 1 {
        return Err(corrupt());
    }
    Ok(next)
}

fn state_projection() -> String {
    format!(
        "CASE WHEN typeof(state)='text' AND state IN ('active','tombstone') AND typeof(revision)='integer' AND revision BETWEEN 1 AND {} THEN state END",
        i64::MAX
    )
}

/// What a mutation found under the reference it is about to change.
// A record is larger than a tombstone by design; one snapshot lives on the
// stack at a time, so boxing it would only add an allocation.
#[allow(clippy::large_enum_variant)]
pub enum MutationSnapshot {
    Active(Record),
    Forgotten(Tombstone),
}

pub(crate) fn read_mutation_snapshot(
    connection: &Connection,
    reference: &RecordRef,
) -> Result<MutationSnapshot> {
    let state: Option<String> = connection
        .query_row(
            &format!(
                "SELECT {} FROM memory_records WHERE scope_namespace=? AND scope_id=? AND id=? LIMIT 1",
                state_projection()
            ),
            params![&reference.scope.namespace, &reference.scope.id, &reference.id],
            |row| row.get(0),
        )
        .map_err(|error| {
            if no_rows(&error) { Error::new(ErrorKind::NotFound) } else { map_sqlite_error(error) }
        })?;
    match state.as_deref() {
        Some("active") => {
            let statement = Statement {
                sql: format!(
                    "SELECT {} FROM memory_records WHERE state='active' AND scope_namespace=? AND scope_id=? AND id=? LIMIT 1",
                    record_projection("")
                ),
                arguments: vec![
                    reference.scope.namespace.clone().into(),
                    reference.scope.id.clone().into(),
                    reference.id.clone().into(),
                ],
            };
            query_record(connection, &statement).map(MutationSnapshot::Active)
        }
        Some("tombstone") => {
            let sql = format!(
                "SELECT {} FROM memory_records WHERE state='tombstone' AND scope_namespace=? AND scope_id=? AND id=? LIMIT 1",
                tombstone_projection()
            );
            let mut prepared = connection.prepare(&sql).map_err(map_sqlite_error)?;
            let outcome = prepared.query_row(
                params![
                    &reference.scope.namespace,
                    &reference.scope.id,
                    &reference.id
                ],
                |row| Ok(decode_tombstone_row(row)),
            );
            match outcome {
                Ok(value) => value.map(MutationSnapshot::Forgotten),
                Err(error) if no_rows(&error) => Err(Error::new(ErrorKind::NotFound)),
                Err(error) => Err(map_sqlite_error(error)),
            }
        }
        _ => Err(corrupt()),
    }
}

fn conflict_record(id: &str, expected: u64, actual: u64) -> Error {
    Error::conflict("record", id, expected, actual)
}

/// Explains why a conditional `UPDATE` matched no row: the record is gone, the
/// stored row is unreadable, or another writer moved the revision.
fn classify_conditional_miss(
    connection: &Connection,
    reference: &RecordRef,
    expected: u64,
) -> Error {
    let safety = format!(
        "typeof(id)='text' AND length(CAST(id AS BLOB)) BETWEEN 1 AND {MAX_ID_BYTES} AND typeof(state)='text' AND state IN ('active','tombstone') AND typeof(revision)='integer' AND revision BETWEEN 1 AND {}",
        i64::MAX
    );
    let sql = format!(
        "SELECT CASE WHEN {safety} THEN 1 ELSE 0 END,CASE WHEN {safety} THEN id END,CASE WHEN {safety} THEN revision END FROM memory_records WHERE scope_namespace=? AND scope_id=? AND id=? LIMIT 1"
    );
    let outcome = connection.query_row(
        &sql,
        params![
            &reference.scope.namespace,
            &reference.scope.id,
            &reference.id
        ],
        |row| {
            let valid: Option<i64> = row.get(0)?;
            let id: Option<String> = row.get(1)?;
            let revision: Option<i64> = row.get(2)?;
            Ok((valid, id, revision))
        },
    );
    match outcome {
        Err(error) if no_rows(&error) => Error::new(ErrorKind::NotFound),
        Err(error) => map_sqlite_error(error),
        Ok((Some(1), Some(id), Some(revision))) if revision >= 0 => {
            conflict_record(&id, expected, revision as u64)
        }
        Ok(_) => corrupt(),
    }
}

/// Confirms the FTS index holds exactly `expected` rows for `record_id`.
fn require_fts_row_count(connection: &Connection, record_id: &str, expected: usize) -> Result<()> {
    if expected > 1 {
        return Err(corrupt());
    }
    let gate = format!(
        "typeof(record_id)='text' AND length(CAST(record_id AS BLOB)) BETWEEN 1 AND {MAX_ID_BYTES}"
    );
    let sql = format!(
        "SELECT CASE WHEN {gate} THEN record_id END FROM memory_records_fts WHERE record_id=? LIMIT 2"
    );
    let mut prepared = connection.prepare(&sql).map_err(map_sqlite_error)?;
    let mut rows = prepared
        .query(params![record_id])
        .map_err(map_sqlite_error)?;
    let mut count = 0usize;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let id: Option<String> = row.get(0).map_err(map_sqlite_error)?;
        if id.as_deref() != Some(record_id) {
            return Err(corrupt());
        }
        count += 1;
    }
    if count != expected {
        return Err(corrupt());
    }
    Ok(())
}

fn insert_fts(connection: &Connection, record: &Record) -> Result<()> {
    connection
        .execute(
            "INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES(?,?,?,?,?)",
            params![
                &record.id,
                &record.text,
                &record.kind,
                &record.key,
                fts_labels(&record.labels)
            ],
        )
        .map_err(map_sqlite_error)?;
    Ok(())
}

fn replace_fts(connection: &Connection, record: &Record) -> Result<()> {
    require_fts_row_count(connection, &record.id, 1)?;
    let changed = connection
        .execute(
            "DELETE FROM memory_records_fts WHERE record_id=?",
            params![&record.id],
        )
        .map_err(map_sqlite_error)?;
    if changed != 1 {
        return Err(corrupt());
    }
    insert_fts(connection, record)
}

/// Writes the record an accepted `create` candidate authorizes. The caller has
/// already validated and guarded it and owns the surrounding transaction.
pub(crate) fn insert_accepted_record(connection: &Connection, record: &Record) -> Result<()> {
    let encoded = encode_record(record)?;
    let gate =
        format!("typeof(id)='text' AND length(CAST(id AS BLOB)) BETWEEN 1 AND {MAX_ID_BYTES}");
    let existing: std::result::Result<Option<String>, rusqlite::Error> = connection.query_row(
        &format!("SELECT CASE WHEN {gate} THEN id END FROM memory_records WHERE id=? LIMIT 1"),
        params![&record.id],
        |row| row.get(0),
    );
    match existing {
        Ok(Some(_)) => return Err(Error::conflict("record", &record.id, 0, 0)),
        Ok(None) => return Err(corrupt()),
        Err(error) if no_rows(&error) => {}
        Err(error) => return Err(map_sqlite_error(error)),
    }
    require_fts_row_count(connection, &record.id, 0)?;
    connection
        .execute(
            "INSERT INTO memory_records(\
id,scope_namespace,scope_id,kind,semantic_key,text_value,labels_json,metadata_json,source_json,\
confidence,revision,created_at,updated_at,expires_at,state,forgotten_at\
) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,'active',NULL)",
            params![
                &record.id,
                &record.scope.namespace,
                &record.scope.id,
                &record.kind,
                &record.key,
                &record.text,
                &encoded.labels,
                &encoded.metadata,
                &encoded.source,
                record.confidence,
                record.revision as i64,
                &encoded.created,
                &encoded.updated,
                &encoded.expires,
            ],
        )
        .map_err(map_sqlite_error)?;
    insert_fts(connection, record)?;
    require_fts_row_count(connection, &record.id, 1)
}

/// Applies the record an accepted `update` candidate authorizes.
pub(crate) fn update_accepted_record(
    connection: &Connection,
    record: &Record,
    expected: u64,
) -> Result<()> {
    let encoded = encode_record(record)?;
    let changed = connection
        .execute(
            "UPDATE memory_records SET \
kind=?,semantic_key=?,text_value=?,labels_json=?,metadata_json=?,source_json=?,confidence=?,revision=?,updated_at=?,expires_at=? \
WHERE id=? AND scope_namespace=? AND scope_id=? AND state='active' AND revision=?",
            params![
                &record.kind,
                &record.key,
                &record.text,
                &encoded.labels,
                &encoded.metadata,
                &encoded.source,
                record.confidence,
                record.revision as i64,
                &encoded.updated,
                &encoded.expires,
                &record.id,
                &record.scope.namespace,
                &record.scope.id,
                expected as i64,
            ],
        )
        .map_err(map_sqlite_error)?;
    if changed != 1 {
        let reference = RecordRef {
            scope: record.scope.clone(),
            id: record.id.clone(),
        };
        return Err(classify_conditional_miss(connection, &reference, expected));
    }
    replace_fts(connection, record)
}

/// Applies the tombstone an accepted `forget` candidate authorizes.
pub(crate) fn forget_accepted_record(
    connection: &Connection,
    tombstone: &Tombstone,
    expected: u64,
) -> Result<()> {
    let changed = connection
        .execute(
            "UPDATE memory_records SET \
kind='',semantic_key='',text_value='',labels_json='[]',metadata_json='{}',source_json='{}',confidence=0.0,\
revision=?,updated_at=?,expires_at=NULL,state='tombstone',forgotten_at=? \
WHERE id=? AND scope_namespace=? AND scope_id=? AND state='active' AND revision=?",
            params![
                tombstone.revision as i64,
                format_timestamp(tombstone.updated_at),
                format_timestamp(tombstone.forgotten_at),
                &tombstone.id,
                &tombstone.scope.namespace,
                &tombstone.scope.id,
                expected as i64,
            ],
        )
        .map_err(map_sqlite_error)?;
    if changed != 1 {
        let reference = RecordRef {
            scope: tombstone.scope.clone(),
            id: tombstone.id.clone(),
        };
        return Err(classify_conditional_miss(connection, &reference, expected));
    }
    require_fts_row_count(connection, &tombstone.id, 1)?;
    let removed = connection
        .execute(
            "DELETE FROM memory_records_fts WHERE record_id=?",
            params![&tombstone.id],
        )
        .map_err(map_sqlite_error)?;
    if removed != 1 {
        return Err(corrupt());
    }
    Ok(())
}

impl Store {
    pub fn get(&self, reference: &RecordRef) -> Result<Record> {
        validate_record_ref(reference)?;
        let statement = Statement {
            sql: format!(
                "SELECT {} FROM memory_records WHERE state='active' AND scope_namespace=? AND scope_id=? AND id=? LIMIT 1",
                record_projection("")
            ),
            arguments: vec![
                reference.scope.namespace.clone().into(),
                reference.scope.id.clone().into(),
                reference.id.clone().into(),
            ],
        };
        let record = self.with_read(|connection| query_record(connection, &statement))?;
        guard_record(self.guard(), &record)?;
        Ok(record)
    }

    /// The forgotten counterpart of [`Store::get`]. A tombstone keeps no
    /// content, so the guard only checks its identifiers.
    pub fn get_tombstone(&self, reference: &RecordRef) -> Result<Tombstone> {
        validate_record_ref(reference)?;
        let sql = format!(
            "SELECT {} FROM memory_records WHERE state='tombstone' AND scope_namespace=? AND scope_id=? AND id=? LIMIT 1",
            tombstone_projection()
        );
        let tombstone = self.with_read(|connection| {
            let mut prepared = connection.prepare(&sql).map_err(map_sqlite_error)?;
            let outcome = prepared.query_row(
                params![
                    &reference.scope.namespace,
                    &reference.scope.id,
                    &reference.id
                ],
                |row| Ok(decode_tombstone_row(row)),
            );
            match outcome {
                Ok(value) => value,
                Err(error) if no_rows(&error) => Err(Error::new(ErrorKind::NotFound)),
                Err(error) => Err(map_sqlite_error(error)),
            }
        })?;
        self.guard().check(&GuardInput {
            fields: vec![
                GuardField {
                    name: "tombstone ID".into(),
                    value: tombstone.id.clone(),
                    opaque: true,
                },
                GuardField {
                    name: "tombstone scope namespace".into(),
                    value: tombstone.scope.namespace.clone(),
                    opaque: false,
                },
                GuardField {
                    name: "tombstone scope ID".into(),
                    value: tombstone.scope.id.clone(),
                    opaque: true,
                },
            ],
        })?;
        Ok(tombstone)
    }

    pub fn get_by_key(&self, key: &RecordKey) -> Result<Record> {
        validate_record_key(key)?;
        let statement = Statement {
            sql: format!(
                "SELECT {} FROM memory_records WHERE state='active' AND scope_namespace=? AND scope_id=? AND kind=? AND semantic_key=? LIMIT 1",
                record_projection("")
            ),
            arguments: vec![
                key.scope.namespace.clone().into(),
                key.scope.id.clone().into(),
                key.kind.clone().into(),
                key.key.clone().into(),
            ],
        };
        let record = self.with_read(|connection| query_record(connection, &statement))?;
        guard_record(self.guard(), &record)?;
        Ok(record)
    }

    pub fn list(&self, request: &ListRequest) -> Result<RecordPage> {
        validate_list_request(request)?;
        let fingerprint = fingerprint_list(request);
        let cursor = decode_record_cursor(&request.cursor, &fingerprint)?;
        let (generation, records) = self.with_read(|connection| {
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
                let statement = build_list_query(request, cursor.as_ref());
                let mut prepared = connection
                    .prepare(&statement.sql)
                    .map_err(map_sqlite_error)?;
                let mut rows = prepared
                    .query(params_from_iter(statement.arguments.iter()))
                    .map_err(map_sqlite_error)?;
                let mut records = Vec::new();
                while let Some(row) = rows.next().map_err(map_sqlite_error)? {
                    if records.len() > MAX_PAGE_SIZE {
                        return Err(corrupt());
                    }
                    records.push(decode_record_row(row)?);
                }
                Ok((generation, records))
            })
        })?;
        for record in &records {
            guard_record(self.guard(), record)?;
        }
        let mut page = RecordPage {
            records,
            next_cursor: String::new(),
        };
        if page.records.len() > request.limit {
            page.records.truncate(request.limit);
            let last = page.records.last().expect("limit is positive");
            page.next_cursor = encode_record_cursor(
                &fingerprint,
                generation,
                &format_timestamp(last.updated_at),
                &last.id,
            )?;
        }
        Ok(page)
    }

    pub fn upsert(&self, request: &UpsertRequest) -> Result<Record> {
        // The encoded JSON must spell an empty collection `[]`/`{}`; Rust's
        // collections are already non-null, so there is nothing to normalize.
        let request = request.clone();
        validate_upsert_request(&request)?;
        guard_record(self.guard(), &request.record)?;
        match request.expected_revision {
            None => self.create_record(request.record),
            Some(expected) => self.update_record(request.record, expected),
        }
    }

    fn create_record(&self, input: Record) -> Result<Record> {
        let mut desired = input;
        desired.revision = 1;
        let encoded = encode_record(&desired)?;
        let stored = desired.clone();
        self.with_write(move |connection| {
            let gate = format!(
                "typeof(id)='text' AND length(CAST(id AS BLOB)) BETWEEN 1 AND {MAX_ID_BYTES}"
            );
            let existing: std::result::Result<Option<String>, rusqlite::Error> = connection
                .query_row(
                    &format!(
                        "SELECT CASE WHEN {gate} THEN id END FROM memory_records WHERE id=? LIMIT 1"
                    ),
                    params![&stored.id],
                    |row| row.get(0),
                );
            match existing {
                Ok(Some(_)) => return Err(Error::new(ErrorKind::Conflict)),
                Ok(None) => return Err(corrupt()),
                Err(error) if no_rows(&error) => {}
                Err(error) => return Err(map_sqlite_error(error)),
            }
            require_fts_row_count(connection, &stored.id, 0)?;
            connection
                .execute(
                    "INSERT INTO memory_records(\
id,scope_namespace,scope_id,kind,semantic_key,text_value,labels_json,metadata_json,source_json,\
confidence,revision,created_at,updated_at,expires_at,state,forgotten_at\
) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,'active',NULL)",
                    params![
                        &stored.id,
                        &stored.scope.namespace,
                        &stored.scope.id,
                        &stored.kind,
                        &stored.key,
                        &stored.text,
                        &encoded.labels,
                        &encoded.metadata,
                        &encoded.source,
                        stored.confidence,
                        1i64,
                        &encoded.created,
                        &encoded.updated,
                        &encoded.expires,
                    ],
                )
                .map_err(map_sqlite_error)?;
            insert_fts(connection, &stored)?;
            require_fts_row_count(connection, &stored.id, 1)?;
            let generation = bump_generation(connection)?;
            Ok(((), generation))
        })
        .map_err(|error| match error.kind {
            ErrorKind::Conflict if error.conflict.is_none() => {
                Error::conflict("record", &desired.id, 0, 0)
            }
            _ => error,
        })?;
        Ok(desired)
    }

    fn update_record(&self, input: Record, expected: u64) -> Result<Record> {
        let reference = RecordRef {
            scope: input.scope.clone(),
            id: input.id.clone(),
        };
        let guarded = self.read_guarded_snapshot(&reference)?;
        let existing = match guarded {
            MutationSnapshot::Forgotten(tombstone) => {
                return Err(conflict_record(&input.id, expected, tombstone.revision));
            }
            MutationSnapshot::Active(record) => record,
        };
        if existing.revision != expected {
            return Err(conflict_record(&input.id, expected, existing.revision));
        }
        if input.created_at != existing.created_at {
            return Err(invalid_request("immutable record creation time"));
        }
        if input.updated_at < existing.updated_at {
            return Err(invalid_request("record update time moved backwards"));
        }
        if existing.revision >= i64::MAX as u64 {
            return Err(corrupt());
        }
        let mut desired = input;
        desired.revision = existing.revision + 1;
        let encoded = encode_record(&desired)?;
        let stored = desired.clone();
        let baseline = existing.clone();
        self.with_write(move |connection| {
            match read_mutation_snapshot(connection, &reference)? {
                MutationSnapshot::Forgotten(tombstone) => {
                    return Err(conflict_record(&stored.id, expected, tombstone.revision));
                }
                MutationSnapshot::Active(current) => {
                    if current != baseline {
                        return Err(conflict_record(&stored.id, expected, current.revision));
                    }
                }
            }
            let changed = connection
                .execute(
                    "UPDATE memory_records SET \
kind=?,semantic_key=?,text_value=?,labels_json=?,metadata_json=?,source_json=?,confidence=?,revision=?,updated_at=?,expires_at=? \
WHERE id=? AND scope_namespace=? AND scope_id=? AND state='active' AND revision=?",
                    params![
                        &stored.kind,
                        &stored.key,
                        &stored.text,
                        &encoded.labels,
                        &encoded.metadata,
                        &encoded.source,
                        stored.confidence,
                        stored.revision as i64,
                        &encoded.updated,
                        &encoded.expires,
                        &stored.id,
                        &stored.scope.namespace,
                        &stored.scope.id,
                        baseline.revision as i64,
                    ],
                )
                .map_err(map_sqlite_error)?;
            if changed != 1 {
                return Err(classify_conditional_miss(connection, &reference, expected));
            }
            replace_fts(connection, &stored)?;
            let generation = bump_generation(connection)?;
            Ok(((), generation))
        })?;
        Ok(desired)
    }

    fn read_guarded_snapshot(&self, reference: &RecordRef) -> Result<MutationSnapshot> {
        let snapshot = self.with_read(|connection| {
            in_read_transaction(connection, |connection| {
                read_mutation_snapshot(connection, reference)
            })
        })?;
        if let MutationSnapshot::Active(record) = &snapshot {
            guard_record(self.guard(), record)?;
        }
        Ok(snapshot)
    }

    pub fn forget(&self, request: &StoreForgetRequest) -> Result<Tombstone> {
        validate_store_forget_request(request)?;
        let existing = match self.read_guarded_snapshot(&request.reference)? {
            MutationSnapshot::Forgotten(value) => {
                if request.expected_revision != value.revision {
                    return Err(conflict_record(
                        &value.id,
                        request.expected_revision,
                        value.revision,
                    ));
                }
                if request.forgotten_at < value.updated_at {
                    return Err(invalid_request("forget time moved backwards"));
                }
                return Ok(value);
            }
            MutationSnapshot::Active(record) => record,
        };
        if request.expected_revision != existing.revision {
            return Err(conflict_record(
                &existing.id,
                request.expected_revision,
                existing.revision,
            ));
        }
        if request.forgotten_at < existing.updated_at {
            return Err(invalid_request("forget time moved backwards"));
        }
        if existing.revision >= i64::MAX as u64 {
            return Err(corrupt());
        }
        let value = Tombstone {
            id: existing.id.clone(),
            scope: existing.scope.clone(),
            revision: existing.revision + 1,
            created_at: existing.created_at,
            updated_at: request.forgotten_at,
            forgotten_at: request.forgotten_at,
        };
        let reference = request.reference.clone();
        let expected = request.expected_revision;
        let baseline = existing;
        let tombstone = value.clone();
        self.with_write(move |connection| {
            match read_mutation_snapshot(connection, &reference)? {
                MutationSnapshot::Forgotten(current) => {
                    return Err(conflict_record(&baseline.id, expected, current.revision));
                }
                MutationSnapshot::Active(current) => {
                    if current != baseline {
                        return Err(conflict_record(&baseline.id, expected, current.revision));
                    }
                }
            }
            let changed = connection
                .execute(
                    "UPDATE memory_records SET \
kind='',semantic_key='',text_value='',labels_json='[]',metadata_json='{}',source_json='{}',confidence=0.0,\
revision=?,updated_at=?,expires_at=NULL,state='tombstone',forgotten_at=? \
WHERE id=? AND scope_namespace=? AND scope_id=? AND state='active' AND revision=?",
                    params![
                        tombstone.revision as i64,
                        format_timestamp(tombstone.updated_at),
                        format_timestamp(tombstone.forgotten_at),
                        &tombstone.id,
                        &tombstone.scope.namespace,
                        &tombstone.scope.id,
                        baseline.revision as i64,
                    ],
                )
                .map_err(map_sqlite_error)?;
            if changed != 1 {
                return Err(classify_conditional_miss(connection, &reference, expected));
            }
            require_fts_row_count(connection, &tombstone.id, 1)?;
            let removed = connection
                .execute(
                    "DELETE FROM memory_records_fts WHERE record_id=?",
                    params![&tombstone.id],
                )
                .map_err(map_sqlite_error)?;
            if removed != 1 {
                return Err(corrupt());
            }
            let generation = bump_generation(connection)?;
            Ok(((), generation))
        })?;
        Ok(value)
    }
}

#[cfg(test)]
pub(crate) mod testsupport {
    use super::*;
    use chrono::TimeZone;

    pub fn at(second: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(2026, 3, 1, 12, 0, second)
            .single()
            .expect("timestamp")
    }

    pub fn sample_record(id: &str, scope: &Scope, key: &str, text: &str) -> Record {
        Record {
            id: id.to_string(),
            scope: scope.clone(),
            kind: "preference".to_string(),
            key: key.to_string(),
            text: text.to_string(),
            labels: vec!["style".to_string()],
            metadata: std::collections::BTreeMap::new(),
            source: crate::memory::Provenance {
                origin: Some(crate::memory::Origin::Human),
                ..crate::memory::Provenance::default()
            },
            confidence: 0.5,
            revision: 0,
            created_at: at(0),
            updated_at: at(0),
            expires_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testsupport::{at, sample_record};
    use super::*;
    use crate::memory::sqlite::testsupport::open_temp;

    fn user_scope(store: &Store) -> Scope {
        store.identity().expect("identity").user_scope
    }

    #[test]
    fn a_record_round_trips_through_create_read_update_and_forget() {
        let (_directory, store) = open_temp();
        let scope = user_scope(&store);
        let record = sample_record("rec-1", &scope, "tone", "prefers terse answers");

        let created = store
            .upsert(&UpsertRequest {
                record: record.clone(),
                expected_revision: None,
            })
            .expect("create");
        assert_eq!(created.revision, 1);

        let fetched = store
            .get_by_key(&RecordKey {
                scope: scope.clone(),
                kind: "preference".to_string(),
                key: "tone".to_string(),
            })
            .expect("get by key");
        assert_eq!(fetched, created);

        let mut edited = created.clone();
        edited.text = "prefers very terse answers".to_string();
        edited.updated_at = at(1);
        let updated = store
            .upsert(&UpsertRequest {
                record: edited,
                expected_revision: Some(1),
            })
            .expect("update");
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.text, "prefers very terse answers");

        let tombstone = store
            .forget(&StoreForgetRequest {
                reference: RecordRef {
                    scope: scope.clone(),
                    id: "rec-1".to_string(),
                },
                expected_revision: 2,
                forgotten_at: at(2),
            })
            .expect("forget");
        assert_eq!(tombstone.revision, 3);
        assert!(
            store
                .get(&RecordRef {
                    scope,
                    id: "rec-1".to_string()
                })
                .expect_err("gone")
                .is(ErrorKind::NotFound)
        );
    }

    #[test]
    fn a_stale_expected_revision_is_a_conflict() {
        let (_directory, store) = open_temp();
        let scope = user_scope(&store);
        let record = sample_record("rec-2", &scope, "tone", "first");
        store
            .upsert(&UpsertRequest {
                record: record.clone(),
                expected_revision: None,
            })
            .expect("create");
        let mut edited = record;
        edited.revision = 7;
        edited.updated_at = at(1);
        let error = store
            .upsert(&UpsertRequest {
                record: edited,
                expected_revision: Some(7),
            })
            .expect_err("conflict");
        assert!(error.is(ErrorKind::Conflict));
        let conflict = error.conflict.expect("conflict detail");
        assert_eq!(
            (conflict.expected_revision, conflict.actual_revision),
            (7, 1)
        );
    }

    #[test]
    fn an_update_cannot_move_the_creation_or_update_time_backwards() {
        let (_directory, store) = open_temp();
        let scope = user_scope(&store);
        let created = store
            .upsert(&UpsertRequest {
                record: sample_record("rec-3", &scope, "tone", "first"),
                expected_revision: None,
            })
            .expect("create");

        let mut moved = created.clone();
        moved.created_at = at(1);
        moved.updated_at = at(1);
        let error = store
            .upsert(&UpsertRequest {
                record: moved,
                expected_revision: Some(1),
            })
            .expect_err("rejected");
        assert_eq!(
            error.to_string(),
            "invalid memory request: immutable record creation time"
        );

        let mut backwards = created;
        backwards.created_at = at(0);
        backwards.updated_at = at(0);
        backwards.created_at = at(0);
        let mut earlier = backwards.clone();
        earlier.updated_at = at(0);
        earlier.created_at = at(0);
        // The stored record already sits at t=0, so only a forget before it can
        // move backwards; assert that message here.
        let error = store
            .forget(&StoreForgetRequest {
                reference: RecordRef {
                    scope,
                    id: "rec-3".to_string(),
                },
                expected_revision: 1,
                forgotten_at: chrono::DateTime::UNIX_EPOCH + chrono::Duration::seconds(1),
            })
            .expect_err("rejected");
        assert_eq!(
            error.to_string(),
            "invalid memory request: forget time moved backwards"
        );
    }

    #[test]
    fn listing_pages_by_cursor_and_rejects_a_cursor_from_another_query() {
        let (_directory, store) = open_temp();
        let scope = user_scope(&store);
        for index in 0..3u32 {
            let mut record = sample_record(
                &format!("rec-{index}"),
                &scope,
                &format!("k{index}"),
                "text",
            );
            record.updated_at = at(index);
            record.created_at = at(0);
            store
                .upsert(&UpsertRequest {
                    record,
                    expected_revision: None,
                })
                .expect("create");
        }
        let request = ListRequest {
            scopes: vec![scope.clone()],
            kinds: Vec::new(),
            labels: Vec::new(),
            limit: 2,
            cursor: String::new(),
            now: at(9),
            include_expired: false,
        };
        let first = store.list(&request).expect("list");
        assert_eq!(first.records.len(), 2);
        assert!(!first.next_cursor.is_empty());

        let second = store
            .list(&ListRequest {
                cursor: first.next_cursor.clone(),
                ..request.clone()
            })
            .expect("page two");
        assert_eq!(second.records.len(), 1);
        assert!(second.next_cursor.is_empty());

        let mismatched = ListRequest {
            kinds: vec!["instruction".to_string()],
            cursor: first.next_cursor,
            ..request
        };
        assert!(
            store
                .list(&mismatched)
                .expect_err("rejected")
                .is(ErrorKind::InvalidCursor)
        );
    }

    #[test]
    fn the_generation_counter_advances_once_per_mutation() {
        let (_directory, store) = open_temp();
        let scope = user_scope(&store);
        assert_eq!(store.identity().expect("identity").generation, 0);
        store
            .upsert(&UpsertRequest {
                record: sample_record("rec-4", &scope, "tone", "text"),
                expected_revision: None,
            })
            .expect("create");
        assert_eq!(store.identity().expect("identity").generation, 1);
    }
}
