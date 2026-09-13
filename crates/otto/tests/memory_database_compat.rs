//! Database compatibility with the Go binary.
//!
//! The two binaries open the same `~/.otto/memory/memory.db`, so a database
//! the Rust store creates must carry exactly the schema the Go migration
//! writes. The expected SQL below is copied verbatim from the Go source,
//! `internal/memory/sqlite/schema.go`, including the FTS5 shadow tables SQLite
//! derives from the virtual-table definition.

use std::collections::BTreeMap;

use otto::memory::sqlite::{Options, Store};
use otto::memory::{ListRequest, NAMESPACE_USER, Origin, Provenance, Record, Scope, UpsertRequest};

/// Every object `sqlite_master` must hold, as `(type, name, sql)`. Copied from
/// the Go migration statements.
const EXPECTED_OBJECTS: &[(&str, &str, &str)] = &[
    (
        "table",
        "memory_meta",
        r#"CREATE TABLE memory_meta (
    key TEXT PRIMARY KEY CHECK (length(key) BETWEEN 1 AND 64),
    value TEXT NOT NULL CHECK (length(value) <= 256)
) STRICT"#,
    ),
    (
        "table",
        "memory_records",
        r#"CREATE TABLE memory_records (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 64),
    scope_namespace TEXT NOT NULL CHECK (length(scope_namespace) BETWEEN 1 AND 32),
    scope_id TEXT NOT NULL CHECK (length(scope_id) BETWEEN 1 AND 128),
    kind TEXT NOT NULL CHECK (length(kind) <= 32),
    semantic_key TEXT NOT NULL CHECK (length(CAST(semantic_key AS BLOB)) <= 256),
    text_value TEXT NOT NULL CHECK (length(CAST(text_value AS BLOB)) <= 8192),
    labels_json TEXT NOT NULL CHECK (length(CAST(labels_json AS BLOB)) <= 8192 AND json_valid(labels_json) AND json_type(labels_json) = 'array'),
    metadata_json TEXT NOT NULL CHECK (length(CAST(metadata_json AS BLOB)) <= 4096 AND json_valid(metadata_json) AND json_type(metadata_json) = 'object'),
    source_json TEXT NOT NULL CHECK (length(CAST(source_json AS BLOB)) <= 8192 AND json_valid(source_json) AND json_type(source_json) = 'object'),
    confidence REAL NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    created_at TEXT NOT NULL CHECK (length(created_at) = 30),
    updated_at TEXT NOT NULL CHECK (length(updated_at) = 30),
    expires_at TEXT CHECK (expires_at IS NULL OR length(expires_at) = 30),
    state TEXT NOT NULL CHECK (state IN ('active','tombstone')),
    forgotten_at TEXT CHECK (forgotten_at IS NULL OR length(forgotten_at) = 30),
    CHECK (updated_at >= created_at),
    CHECK (expires_at IS NULL OR expires_at >= created_at),
    CHECK (
        (state = 'active' AND forgotten_at IS NULL) OR
        (state = 'tombstone' AND forgotten_at IS NOT NULL AND forgotten_at = updated_at AND kind = '' AND semantic_key = ''
         AND text_value = '' AND labels_json = '[]' AND metadata_json = '{}'
         AND source_json = '{}' AND confidence = 0.0 AND expires_at IS NULL)
    )
) STRICT"#,
    ),
    (
        "index",
        "memory_records_key_active",
        r#"CREATE UNIQUE INDEX memory_records_key_active
ON memory_records(scope_namespace, scope_id, kind, semantic_key)
WHERE state = 'active' AND semantic_key <> ''"#,
    ),
    (
        "index",
        "memory_records_list",
        r#"CREATE INDEX memory_records_list
ON memory_records(scope_namespace, scope_id, state, updated_at DESC, id ASC)"#,
    ),
    (
        "table",
        "memory_observations",
        r#"CREATE TABLE memory_observations (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 64),
    candidate_ids_json TEXT NOT NULL CHECK (length(CAST(candidate_ids_json AS BLOB)) <= 1024 AND json_valid(candidate_ids_json) AND json_type(candidate_ids_json) = 'array'),
    created_at TEXT NOT NULL CHECK (length(created_at) = 30)
) STRICT"#,
    ),
    (
        "table",
        "memory_candidates",
        r#"CREATE TABLE memory_candidates (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 64),
    scope_namespace TEXT NOT NULL CHECK (length(scope_namespace) BETWEEN 1 AND 32),
    scope_id TEXT NOT NULL CHECK (length(scope_id) BETWEEN 1 AND 128),
    action TEXT NOT NULL CHECK (action IN ('create','update','forget')),
    target_id TEXT NOT NULL CHECK (length(target_id) <= 64),
    base_revision INTEGER NOT NULL CHECK (base_revision >= 0),
    observation_id TEXT REFERENCES memory_observations(id) ON DELETE RESTRICT CHECK (observation_id IS NULL OR length(observation_id) BETWEEN 1 AND 64),
    proposed_json TEXT NOT NULL CHECK (length(CAST(proposed_json AS BLOB)) <= 32768 AND json_valid(proposed_json) AND json_type(proposed_json) = 'object'),
    reason TEXT NOT NULL CHECK (length(CAST(reason AS BLOB)) <= 2048),
    state TEXT NOT NULL CHECK (state IN ('pending','accepted','rejected')),
    created_at TEXT NOT NULL CHECK (length(created_at) = 30),
    decided_at TEXT CHECK (decided_at IS NULL OR length(decided_at) = 30),
    decision_source TEXT NOT NULL CHECK (length(decision_source) <= 32),
    result_record_id TEXT NOT NULL CHECK (length(result_record_id) <= 64),
    result_revision INTEGER NOT NULL CHECK (result_revision >= 0),
    CHECK (COALESCE(json_type(proposed_json, '$.scope_namespace'), '') = 'text' AND COALESCE(json_extract(proposed_json, '$.scope_namespace'), '') = scope_namespace),
    CHECK (COALESCE(json_type(proposed_json, '$.scope_id'), '') = 'text' AND COALESCE(json_extract(proposed_json, '$.scope_id'), '') = scope_id),
    CHECK (
        state <> 'pending' OR
        CASE WHEN observation_id IS NULL THEN
            COALESCE(json_extract(proposed_json, '$.source.observation_id'), '') = ''
        ELSE
            COALESCE(json_type(proposed_json, '$.source.observation_id'), '') = 'text'
            AND COALESCE(json_extract(proposed_json, '$.source.observation_id'), '') = observation_id
        END
    ),
    CHECK (
        (action = 'create' AND target_id = '' AND base_revision = 0) OR
        (action IN ('update','forget') AND target_id <> '' AND base_revision > 0)
    ),
    CHECK (
        (state = 'pending' AND decided_at IS NULL AND decision_source = ''
         AND result_record_id = '' AND result_revision = 0) OR
        (state = 'accepted' AND decided_at IS NOT NULL AND decided_at >= created_at AND decision_source <> '' AND reason = ''
         AND result_record_id <> '' AND result_revision > 0) OR
        (state = 'rejected' AND decided_at IS NOT NULL AND decided_at >= created_at AND decision_source <> '' AND reason = ''
         AND result_record_id = '' AND result_revision = 0)
    ),
    CHECK (
        state = 'pending' OR (
            COALESCE(json_type(proposed_json, '$.kind'), '') = 'text' AND COALESCE(json_extract(proposed_json, '$.kind'), '') = '' AND
            COALESCE(json_type(proposed_json, '$.key'), '') = 'text' AND COALESCE(json_extract(proposed_json, '$.key'), '') = '' AND
            COALESCE(json_type(proposed_json, '$.text'), '') = 'text' AND COALESCE(json_extract(proposed_json, '$.text'), '') = '' AND
            COALESCE(json_type(proposed_json, '$.labels'), '') = 'array' AND json_array_length(proposed_json, '$.labels') = 0 AND
            COALESCE(json_type(proposed_json, '$.metadata'), '') = 'object' AND json_extract(proposed_json, '$.metadata') = '{}' AND
            COALESCE(json_type(proposed_json, '$.source'), '') = 'object' AND json_extract(proposed_json, '$.source') = '{}' AND
            COALESCE(json_type(proposed_json, '$.confidence'), '') IN ('integer','real') AND json_extract(proposed_json, '$.confidence') = 0 AND
            COALESCE(json_type(proposed_json, '$.expiry'), '') = 'null'
        )
    )
) STRICT"#,
    ),
    (
        "index",
        "memory_candidates_list",
        r#"CREATE INDEX memory_candidates_list
ON memory_candidates(scope_namespace, scope_id, state, created_at DESC, id ASC)"#,
    ),
    (
        "index",
        "memory_candidates_observation",
        r#"CREATE INDEX memory_candidates_observation
ON memory_candidates(observation_id)
WHERE observation_id IS NOT NULL"#,
    ),
    (
        "table",
        "memory_records_fts",
        r#"CREATE VIRTUAL TABLE memory_records_fts USING fts5(
    record_id UNINDEXED,
    text_value,
    kind,
    semantic_key,
    labels,
    tokenize = 'unicode61'
)"#,
    ),
    (
        "table",
        "memory_records_fts_config",
        r#"CREATE TABLE 'memory_records_fts_config'(k PRIMARY KEY, v) WITHOUT ROWID"#,
    ),
    (
        "table",
        "memory_records_fts_content",
        r#"CREATE TABLE 'memory_records_fts_content'(id INTEGER PRIMARY KEY, c0, c1, c2, c3, c4)"#,
    ),
    (
        "table",
        "memory_records_fts_data",
        r#"CREATE TABLE 'memory_records_fts_data'(id INTEGER PRIMARY KEY, block BLOB)"#,
    ),
    (
        "table",
        "memory_records_fts_docsize",
        r#"CREATE TABLE 'memory_records_fts_docsize'(id INTEGER PRIMARY KEY, sz BLOB)"#,
    ),
    (
        "table",
        "memory_records_fts_idx",
        r#"CREATE TABLE 'memory_records_fts_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID"#,
    ),
];

/// The indexes SQLite creates for the `TEXT PRIMARY KEY` columns. Their
/// recorded SQL is NULL, which is part of the expected schema.
const EXPECTED_AUTOINDEXES: &[&str] = &[
    "sqlite_autoindex_memory_candidates_1",
    "sqlite_autoindex_memory_meta_1",
    "sqlite_autoindex_memory_observations_1",
    "sqlite_autoindex_memory_records_1",
];

fn sample(id: &str, scope: &Scope, key: &str, text: &str) -> Record {
    Record {
        id: id.to_string(),
        scope: scope.clone(),
        kind: "preference".into(),
        key: key.into(),
        text: text.into(),
        labels: vec!["style".into()],
        metadata: BTreeMap::from([("editor".to_string(), "vim".to_string())]),
        source: Provenance {
            origin: Some(Origin::Human),
            ..Provenance::default()
        },
        confidence: 0.5,
        created_at: chrono::DateTime::from_timestamp(1_772_000_000, 0).expect("timestamp"),
        updated_at: chrono::DateTime::from_timestamp(1_772_000_000, 0).expect("timestamp"),
        ..Record::default()
    }
}

#[test]
fn a_rust_written_database_carries_the_go_schema() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("memory.db");
    let store = Store::open(&path, Options::default()).expect("open");
    store.close().expect("close");

    let connection = rusqlite::Connection::open(&path).expect("reopen");
    let mut statement = connection
        .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY name")
        .expect("prepare");
    let rows: Vec<(String, String, Option<String>)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");

    for (kind, name, sql) in EXPECTED_OBJECTS {
        let found = rows
            .iter()
            .find(|(_, row_name, _)| row_name == name)
            .unwrap_or_else(|| panic!("sqlite_master has no object named {name}"));
        assert_eq!(&found.0, kind, "object {name} has the wrong type");
        assert_eq!(
            found.2.as_deref(),
            Some(*sql),
            "object {name} does not match the Go migration SQL"
        );
    }
    for name in EXPECTED_AUTOINDEXES {
        let found = rows
            .iter()
            .find(|(_, row_name, _)| row_name == name)
            .unwrap_or_else(|| panic!("sqlite_master has no autoindex named {name}"));
        assert_eq!(found.2, None, "autoindex {name} must have no recorded SQL");
    }

    let expected: Vec<&str> = EXPECTED_OBJECTS
        .iter()
        .map(|(_, name, _)| *name)
        .chain(EXPECTED_AUTOINDEXES.iter().copied())
        .collect();
    let mut unexpected: Vec<&str> = rows
        .iter()
        .map(|(_, name, _)| name.as_str())
        .filter(|name| !expected.contains(name))
        .collect();
    unexpected.sort_unstable();
    assert!(
        unexpected.is_empty(),
        "unexpected schema objects: {unexpected:?}"
    );

    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version");
    assert_eq!(version, 1, "the Go migration stamps user_version 1");
}

#[test]
fn a_rust_written_database_round_trips_through_a_fresh_open() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("memory.db");
    let scope = Scope::new(NAMESPACE_USER, "user-1");

    let store = Store::open(&path, Options::default()).expect("open");
    let identity = store.identity().expect("identity");
    let written = store
        .upsert(&UpsertRequest {
            record: sample(
                "0123456789abcdef",
                &scope,
                "editor",
                "prefers vim for editing",
            ),
            expected_revision: None,
        })
        .expect("upsert");
    store.close().expect("close");

    let reopened = Store::open(&path, Options::default()).expect("reopen");
    let reopened_identity = reopened.identity().expect("identity");
    assert_eq!(reopened_identity.database_id, identity.database_id);
    assert_eq!(reopened_identity.user_scope, identity.user_scope);
    assert_eq!(reopened_identity.schema_version, identity.schema_version);
    assert_eq!(
        reopened_identity.generation, 1,
        "the write survived the reopen"
    );
    let page = reopened
        .list(&ListRequest {
            scopes: vec![scope],
            kinds: Vec::new(),
            labels: Vec::new(),
            limit: 10,
            cursor: String::new(),
            now: chrono::DateTime::from_timestamp(1_772_000_100, 0).expect("timestamp"),
            include_expired: false,
        })
        .expect("list");
    assert_eq!(page.records, vec![written]);
    reopened.close().expect("close");
}
