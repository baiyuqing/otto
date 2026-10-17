//! Reads the memory tables of a Turso database (schema version 2) into an
//! [`ImportSet`].
//!
//! The database is opened read-only and never written. Every row is decoded
//! and validated before the caller writes any file; the first invalid row
//! fails the import with its table and id. Error details name tables and ids
//! only, never row content.

use std::path::Path;

use super::ImportSet;
use crate::memory::codec::{
    decode_labels, decode_metadata, decode_provenance, parse_timestamp, valid_stored_float,
};
use crate::memory::proposed::decode_proposed_record;
use crate::memory::validate::{validate_candidate, validate_record, validate_tombstone};
use crate::memory::{
    Candidate, CandidateAction, CandidateState, Error, ErrorKind, Origin, Record, Result, Scope,
    Tombstone,
};
use crate::storage::{Connection, Row};

const SCHEMA_VERSION: i64 = 2;

const NOT_VERSION_2: &str = "the memory database is not schema version 2; migrate it first with \
`otto storage migrate` from Otto 0.4.8 or earlier";

const RECORD_SQL: &str = "SELECT id, scope_namespace, scope_id, kind, semantic_key, text_value, \
labels_json, metadata_json, source_json, confidence, revision, created_at, updated_at, \
expires_at, state, forgotten_at FROM memory_records ORDER BY id";

const CANDIDATE_SQL: &str = "SELECT id, scope_namespace, scope_id, action, target_id, \
base_revision, proposed_json, reason, state, created_at, decided_at, decision_source, \
result_record_id, result_revision FROM memory_candidates ORDER BY id";

fn unavailable(what: &str) -> Error {
    Error::detailed(
        ErrorKind::Unavailable,
        format!("read memory database: {what}"),
    )
}

fn corrupt(table: &str, id: &str, what: &str) -> Error {
    Error::detailed(ErrorKind::Corrupt, format!("{table} row {id}: {what}"))
}

/// Driver errors can carry row data, so only the failing step is reported.
fn read<T>(result: crate::storage::Result<T>, step: &str) -> Result<T> {
    result.map_err(|_| unavailable(step))
}

fn valid_meta_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

fn string(row: &Row, index: usize) -> std::result::Result<String, &'static str> {
    row.get::<Option<String>>(index)
        .ok()
        .flatten()
        .ok_or("column is missing or not text")
}

fn optional_string(row: &Row, index: usize) -> std::result::Result<Option<String>, &'static str> {
    row.get::<Option<String>>(index)
        .map_err(|_| "column is not text")
}

fn integer(row: &Row, index: usize) -> std::result::Result<u64, &'static str> {
    row.get::<Option<i64>>(index)
        .ok()
        .flatten()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or("column is missing or negative")
}

fn timestamp(value: &str) -> std::result::Result<chrono::DateTime<chrono::Utc>, &'static str> {
    parse_timestamp(value).map_err(|_| "timestamp is not canonical")
}

fn optional_timestamp(
    value: Option<String>,
) -> std::result::Result<Option<chrono::DateTime<chrono::Utc>>, &'static str> {
    value.as_deref().map(timestamp).transpose()
}

/// A decoded `memory_records` row.
enum RecordRow {
    Active(Record),
    Tombstone(Tombstone),
}

fn decode_record_row(row: &Row) -> std::result::Result<RecordRow, &'static str> {
    let id = string(row, 0)?;
    let scope = Scope::new(string(row, 1)?, string(row, 2)?);
    let revision = integer(row, 10)?;
    let created_at = timestamp(&string(row, 11)?)?;
    let updated_at = timestamp(&string(row, 12)?)?;
    let state = string(row, 14)?;
    let forgotten_at = optional_timestamp(optional_string(row, 15)?)?;
    if state == "tombstone" {
        let tombstone = Tombstone {
            id,
            scope,
            revision,
            created_at,
            updated_at,
            forgotten_at: forgotten_at.ok_or("tombstone has no forgotten_at")?,
        };
        validate_tombstone(&tombstone).map_err(|_| "tombstone is invalid")?;
        return Ok(RecordRow::Tombstone(tombstone));
    }
    if state != "active" || forgotten_at.is_some() {
        return Err("state is not active or tombstone");
    }
    let confidence = row
        .get::<Option<f64>>(9)
        .ok()
        .flatten()
        .ok_or("confidence is missing")?;
    let record = Record {
        id,
        scope,
        kind: string(row, 3)?,
        key: string(row, 4)?,
        text: string(row, 5)?,
        labels: decode_labels(&string(row, 6)?).map_err(|_| "labels_json is not canonical")?,
        metadata: decode_metadata(&string(row, 7)?)
            .map_err(|_| "metadata_json is not canonical")?,
        source: decode_provenance(&string(row, 8)?).map_err(|_| "source_json is not canonical")?,
        confidence,
        revision,
        created_at,
        updated_at,
        expires_at: optional_timestamp(optional_string(row, 13)?)?,
    };
    if !valid_stored_float(record.confidence) || validate_record(&record).is_err() {
        return Err("record is invalid");
    }
    Ok(RecordRow::Active(record))
}

fn decode_candidate_row(row: &Row) -> std::result::Result<Candidate, &'static str> {
    let scope = Scope::new(string(row, 1)?, string(row, 2)?);
    let decision_source = string(row, 11)?;
    let mut candidate = Candidate {
        id: string(row, 0)?,
        action: CandidateAction::parse(&string(row, 3)?).ok_or("action is unknown")?,
        target_id: string(row, 4)?,
        base_revision: integer(row, 5)?,
        proposed: decode_proposed_record(&string(row, 6)?)
            .map_err(|_| "proposed_json is not canonical")?,
        reason: string(row, 7)?,
        state: CandidateState::parse(&string(row, 8)?).ok_or("state is unknown")?,
        created_at: timestamp(&string(row, 9)?)?,
        decided_at: optional_timestamp(optional_string(row, 10)?)?,
        decision_source: if decision_source.is_empty() {
            None
        } else {
            Some(Origin::parse(&decision_source).ok_or("decision_source is unknown")?)
        },
        result_record_id: string(row, 12)?,
        result_revision: integer(row, 13)?,
    };
    // The database keeps the target ID as the result ID of an accepted update
    // or forget; the neutral contract exposes a result ID only for a create.
    if candidate.state == CandidateState::Accepted && candidate.action != CandidateAction::Create {
        if candidate.result_record_id != candidate.target_id {
            return Err("result_record_id differs from target_id");
        }
        candidate.result_record_id = String::new();
    }
    if candidate.proposed.scope != scope {
        return Err("scope differs from proposed_json");
    }
    validate_candidate(&candidate).map_err(|_| "candidate is invalid")?;
    Ok(candidate)
}

/// Runs `sql` and decodes every row. A decode failure names the row's id.
fn read_table<T>(
    connection: &Connection,
    table: &str,
    sql: &str,
    decode: fn(&Row) -> std::result::Result<T, &'static str>,
) -> Result<Vec<T>> {
    let mut statement = read(connection.prepare(sql), table)?;
    let mut rows = read(statement.query(()), table)?;
    let mut out = Vec::new();
    while let Some(row) = read(rows.next(), table)? {
        match decode(row) {
            Ok(value) => out.push(value),
            Err(what) => {
                let id = row
                    .get::<Option<String>>(0)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                return Err(corrupt(table, &id, what));
            }
        }
    }
    Ok(out)
}

fn read_meta(connection: &Connection) -> Result<(String, String)> {
    let mut statement = read(
        connection.prepare("SELECT key, value FROM memory_meta"),
        "memory_meta",
    )?;
    let mut rows = read(statement.query(()), "memory_meta")?;
    let (mut database_id, mut user_scope_id) = (None, None);
    while let Some(row) = read(rows.next(), "memory_meta")? {
        let key = row.get::<String>(0).unwrap_or_default();
        let value = row.get::<String>(1).unwrap_or_default();
        match key.as_str() {
            "database_id" => database_id = Some(value),
            "user_scope_id" => user_scope_id = Some(value),
            _ => {}
        }
    }
    match (database_id, user_scope_id) {
        (Some(database), Some(user))
            if valid_meta_id(&database) && valid_meta_id(&user) && database != user =>
        {
            Ok((database, user))
        }
        _ => Err(Error::detailed(
            ErrorKind::Corrupt,
            "memory_meta: database_id and user_scope_id must be distinct 32-character hex IDs",
        )),
    }
}

/// Reads the whole database at `source` without modifying it.
pub(crate) fn import_turso(source: &Path) -> Result<ImportSet> {
    let connection = read(Connection::open_read_only(source), "open")?;
    let version: i64 = read(
        connection.query_row("PRAGMA user_version", (), |row| row.get(0)),
        "user_version",
    )?;
    if version != SCHEMA_VERSION {
        return Err(Error::detailed(
            ErrorKind::IncompatibleSchema,
            NOT_VERSION_2,
        ));
    }
    let (database_id, user_scope_id) = read_meta(&connection)?;
    let mut records = Vec::new();
    let mut tombstones = Vec::new();
    for row in read_table(&connection, "memory_records", RECORD_SQL, decode_record_row)? {
        match row {
            RecordRow::Active(record) => records.push(record),
            RecordRow::Tombstone(tombstone) => tombstones.push(tombstone),
        }
    }
    let candidates = read_table(
        &connection,
        "memory_candidates",
        CANDIDATE_SQL,
        decode_candidate_row,
    )?;
    let _ = connection.close();
    Ok(ImportSet {
        database_id,
        user_scope_id,
        records,
        tombstones,
        candidates,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::memory::codec::{encode_record, format_timestamp};
    use crate::memory::files::testsupport::{at, sample_record};
    use crate::memory::files::{FileStore, has_identity, write_import};
    use crate::memory::proposed::encode_proposed_record;
    use crate::memory::repository::MemoryOptions;
    use crate::memory::{CandidateRef, Provenance, RecordRef};
    use crate::storage::params;

    const DATABASE_ID: &str = "0123456789abcdef0123456789abcdef";
    const USER_SCOPE_ID: &str = "fedcba9876543210fedcba9876543210";

    fn user() -> Scope {
        Scope::new(crate::memory::NAMESPACE_USER, USER_SCOPE_ID)
    }

    fn workspace() -> Scope {
        Scope::new(crate::memory::NAMESPACE_WORKSPACE, "w".repeat(32))
    }

    fn new_database(directory: &Path) -> (PathBuf, Connection) {
        let path = directory.join("memory.db");
        let connection = Connection::open(&path).expect("open database");
        connection
            .execute_batch(include_str!("testdata/memory-v2.sql"))
            .expect("load v2 schema");
        (path, connection)
    }

    fn insert_record(connection: &Connection, record: &Record) {
        let encoded = encode_record(record).expect("encode record");
        connection
            .execute(
                "INSERT INTO memory_records(id, scope_namespace, scope_id, kind, semantic_key, \
                 text_value, labels_json, metadata_json, source_json, confidence, revision, \
                 created_at, updated_at, expires_at, state, forgotten_at) \
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,NULL,'active',NULL)",
                params![
                    record.id,
                    record.scope.namespace,
                    record.scope.id,
                    record.kind,
                    record.key,
                    record.text,
                    encoded.labels,
                    encoded.metadata,
                    encoded.source,
                    record.confidence,
                    record.revision as i64,
                    format_timestamp(record.created_at),
                    format_timestamp(record.updated_at),
                ],
            )
            .expect("insert record");
    }

    fn insert_tombstone(connection: &Connection, id: &str) {
        let forgotten = format_timestamp(at(9));
        connection
            .execute(
                "INSERT INTO memory_records(id, scope_namespace, scope_id, kind, semantic_key, \
                 text_value, labels_json, metadata_json, source_json, confidence, revision, \
                 created_at, updated_at, expires_at, state, forgotten_at) \
                 VALUES (?,?,?,'','','','[]','{}','{}',0.0,3,?,?,NULL,'tombstone',?)",
                params![
                    id,
                    "user",
                    USER_SCOPE_ID,
                    format_timestamp(at(0)),
                    forgotten.clone(),
                    forgotten,
                ],
            )
            .expect("insert tombstone");
    }

    fn insert_candidate(connection: &Connection, id: &str, proposed: &Record, state: &str) {
        let (decided, source, result_id, result_revision) = if state == "pending" {
            (None, "", "", 0)
        } else {
            (Some(format_timestamp(at(8))), "human", "r-created", 1)
        };
        connection
            .execute(
                "INSERT INTO memory_candidates(id, scope_namespace, scope_id, action, target_id, \
                 base_revision, observation_id, proposed_json, reason, state, created_at, \
                 decided_at, decision_source, result_record_id, result_revision) \
                 VALUES (?,?,?,'create','',0,NULL,?,?,?,?,?,?,?,?)",
                params![
                    id,
                    proposed.scope.namespace,
                    proposed.scope.id,
                    encode_proposed_record(proposed).expect("encode proposed"),
                    if state == "pending" { "why" } else { "" },
                    state,
                    format_timestamp(at(7)),
                    decided,
                    source,
                    result_id,
                    result_revision,
                ],
            )
            .expect("insert candidate");
    }

    /// A stored record: revision 1 or more, as the schema requires.
    fn stored(id: &str, scope: &Scope, key: &str, text: &str) -> Record {
        Record {
            revision: 1,
            ..sample_record(id, scope, key, text)
        }
    }

    fn cleared(proposed: &Record) -> Record {
        Record {
            scope: proposed.scope.clone(),
            ..Record::default()
        }
    }

    fn populated(directory: &Path) -> PathBuf {
        let (path, connection) = new_database(directory);
        let mut first = stored("r-user", &user(), "style", "prefers short answers");
        first.revision = 2;
        first.updated_at = at(3);
        insert_record(&connection, &first);
        let mut second = stored("r-work", &workspace(), "build", "uses cargo nextest");
        second.revision = 1;
        insert_record(&connection, &second);
        insert_tombstone(&connection, "r-gone");
        let pending = Record {
            scope: user(),
            kind: "preference".into(),
            key: "tone".into(),
            text: "likes plain language".into(),
            labels: vec!["style".into()],
            source: Provenance {
                origin: Some(Origin::Model),
                ..Provenance::default()
            },
            confidence: 0.5,
            ..Record::default()
        };
        insert_candidate(&connection, "c-pending", &pending, "pending");
        insert_candidate(&connection, "c-accepted", &cleared(&pending), "accepted");
        connection.close().expect("close database");
        path
    }

    fn open_options() -> MemoryOptions {
        MemoryOptions::default()
    }

    #[test]
    fn a_v2_database_imports_into_a_file_store() {
        let directory = tempfile::tempdir().expect("temp dir");
        let source = populated(directory.path());
        let set = import_turso(&source).expect("import");
        let root = directory.path().join("memory");
        assert!(write_import(&root, &set, std::time::Duration::ZERO).expect("write"));
        let store = FileStore::open(&root, open_options()).expect("open");

        let identity = store.identity().expect("identity");
        assert_eq!(identity.database_id, DATABASE_ID);
        assert_eq!(identity.user_scope, user());

        let first = store
            .get(&RecordRef {
                scope: user(),
                id: "r-user".into(),
            })
            .expect("user record");
        assert_eq!((first.revision, first.updated_at), (2, at(3)));
        assert_eq!(first.text, "prefers short answers");
        let second = store
            .get(&RecordRef {
                scope: workspace(),
                id: "r-work".into(),
            })
            .expect("workspace record");
        assert_eq!((second.revision, second.created_at), (1, at(0)));

        let tombstone = store
            .get_tombstone(&RecordRef {
                scope: user(),
                id: "r-gone".into(),
            })
            .expect("tombstone");
        assert_eq!((tombstone.revision, tombstone.forgotten_at), (3, at(9)));

        let pending = store
            .get_candidate(&CandidateRef {
                scope: user(),
                id: "c-pending".into(),
            })
            .expect("pending candidate");
        assert_eq!(pending.state, CandidateState::Pending);
        assert_eq!(pending.created_at, at(7));
        assert_eq!(pending.base_revision, 0);
        assert_eq!(pending.proposed.text, "likes plain language");
        let accepted = store
            .get_candidate(&CandidateRef {
                scope: user(),
                id: "c-accepted".into(),
            })
            .expect("accepted candidate");
        assert_eq!(accepted.state, CandidateState::Accepted);
        assert_eq!(accepted.result_record_id, "r-created");
        assert_eq!(accepted.decided_at, Some(at(8)));
        assert!(store.invalid_files().expect("invalid").is_empty());
    }

    #[test]
    fn importing_leaves_the_source_bytes_unchanged() {
        let directory = tempfile::tempdir().expect("temp dir");
        let source = populated(directory.path());
        let wal = directory.path().join("memory.db-wal");
        let snapshot = || {
            (
                std::fs::read(&source).expect("read database"),
                std::fs::read(&wal).ok(),
            )
        };
        let before = snapshot();
        import_turso(&source).expect("import");
        assert!(snapshot() == before, "import changed the database files");
    }

    #[test]
    fn a_second_open_does_not_import_again() {
        let directory = tempfile::tempdir().expect("temp dir");
        let source = populated(directory.path());
        let root = directory.path().join("memory");
        let set = import_turso(&source).expect("import");
        assert!(write_import(&root, &set, std::time::Duration::ZERO).expect("first write"));
        std::fs::remove_file(root.join("records/r-work.md")).expect("delete record");
        assert!(!write_import(&root, &set, std::time::Duration::ZERO).expect("second write"));
        let store = FileStore::open(&root, open_options()).expect("open");
        assert!(
            store
                .get(&RecordRef {
                    scope: workspace(),
                    id: "r-work".into(),
                })
                .is_err()
        );
    }

    #[test]
    fn another_schema_version_fails_with_the_migration_message() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (path, connection) = new_database(directory.path());
        connection
            .execute_batch("PRAGMA user_version=1")
            .expect("set version");
        connection.close().expect("close database");
        let error = import_turso(&path).err().expect("import must fail");
        assert_eq!(error.kind, ErrorKind::IncompatibleSchema);
        assert!(
            error.to_string().contains("otto storage migrate"),
            "{error}"
        );
        assert!(!has_identity(&directory.path().join("memory")));
    }

    #[test]
    fn an_invalid_row_fails_the_import_and_names_the_row() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (path, connection) = new_database(directory.path());
        insert_record(&connection, &stored("r-ok", &user(), "a", "valid record"));
        let broken = stored("r-bad", &user(), "b", "second record");
        insert_record(&connection, &broken);
        connection
            .execute_batch("UPDATE memory_records SET labels_json='[1]' WHERE id='r-bad'")
            .expect("corrupt labels");
        connection.close().expect("close database");
        let error = import_turso(&path).err().expect("import must fail");
        assert_eq!(error.kind, ErrorKind::Corrupt);
        let message = error.to_string();
        assert!(message.contains("memory_records row r-bad"), "{message}");
        assert!(!message.contains("second record"), "{message}");
        assert!(!directory.path().join("memory").exists());
    }

    #[test]
    fn a_bad_timestamp_fails_the_import() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (path, connection) = new_database(directory.path());
        insert_record(&connection, &stored("r-time", &user(), "a", "valid record"));
        connection
            .execute_batch(
                "UPDATE memory_records SET updated_at='2026-13-01T12:00:00.000000000Z' \
                 WHERE id='r-time'",
            )
            .expect("corrupt timestamp");
        connection.close().expect("close database");
        let error = import_turso(&path).err().expect("import must fail");
        assert!(
            error.to_string().contains("memory_records row r-time"),
            "{error}"
        );
    }
}
