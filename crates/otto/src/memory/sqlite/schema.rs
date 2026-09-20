//! The schema v1 migration, its fingerprint, and the verification that a
//! database on disk was produced by exactly this manifest.
//!
//! The statement text must not change: the fingerprint stored in `memory_meta`
//! is SHA-256 over the concatenation of these statements, and a database whose
//! fingerprint disagrees is rejected.

use std::collections::BTreeMap;

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::memory::{Error, ErrorKind, Result, Scope, StoreIdentity};

pub const SCHEMA_VERSION: i64 = 1;
pub const CREATE_MEMORY_META: &str = r#"CREATE TABLE memory_meta (
    key TEXT PRIMARY KEY CHECK (length(key) BETWEEN 1 AND 64),
    value TEXT NOT NULL CHECK (length(value) <= 256)
) STRICT"#;
pub const CREATE_MEMORY_RECORDS: &str = r#"CREATE TABLE memory_records (
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
) STRICT"#;
pub const CREATE_MEMORY_RECORDS_KEY_ACTIVE: &str = r#"CREATE UNIQUE INDEX memory_records_key_active
ON memory_records(scope_namespace, scope_id, kind, semantic_key)
WHERE state = 'active' AND semantic_key <> ''"#;
pub const CREATE_MEMORY_RECORDS_LIST: &str = r#"CREATE INDEX memory_records_list
ON memory_records(scope_namespace, scope_id, state, updated_at DESC, id ASC)"#;
pub const CREATE_MEMORY_OBSERVATIONS: &str = r#"CREATE TABLE memory_observations (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 64),
    candidate_ids_json TEXT NOT NULL CHECK (length(CAST(candidate_ids_json AS BLOB)) <= 1024 AND json_valid(candidate_ids_json) AND json_type(candidate_ids_json) = 'array'),
    created_at TEXT NOT NULL CHECK (length(created_at) = 30)
) STRICT"#;
pub const CREATE_MEMORY_CANDIDATES: &str = r#"CREATE TABLE memory_candidates (
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
) STRICT"#;
pub const CREATE_MEMORY_CANDIDATES_LIST: &str = r#"CREATE INDEX memory_candidates_list
ON memory_candidates(scope_namespace, scope_id, state, created_at DESC, id ASC)"#;
pub const CREATE_MEMORY_CANDIDATES_OBSERVATION: &str = r#"CREATE INDEX memory_candidates_observation
ON memory_candidates(observation_id)
WHERE observation_id IS NOT NULL"#;
pub const CREATE_MEMORY_RECORDS_FTS: &str = r#"CREATE VIRTUAL TABLE memory_records_fts USING fts5(
    record_id UNINDEXED,
    text_value,
    kind,
    semantic_key,
    labels,
    tokenize = 'unicode61'
)"#;

/// The nine statements in migration order.
pub const SCHEMA_STATEMENTS: [&str; 9] = [
    CREATE_MEMORY_META,
    CREATE_MEMORY_RECORDS,
    CREATE_MEMORY_RECORDS_KEY_ACTIVE,
    CREATE_MEMORY_RECORDS_LIST,
    CREATE_MEMORY_OBSERVATIONS,
    CREATE_MEMORY_CANDIDATES,
    CREATE_MEMORY_CANDIDATES_LIST,
    CREATE_MEMORY_CANDIDATES_OBSERVATION,
    CREATE_MEMORY_RECORDS_FTS,
];

/// SHA-256 of the statements joined with `";\n"`. A source constant, never a
/// value learned from an opened database.
pub const COMPILED_SCHEMA_FINGERPRINT: &str =
    "f927b04baf82340748b4af92984d0f165f734acb4ea3e539f190e79dd54847e9";

pub fn schema_manifest() -> String {
    SCHEMA_STATEMENTS.join(";\n")
}

pub fn fingerprint_manifest(manifest: &str) -> String {
    Sha256::digest(manifest.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `"type:name"` to the statement that must have created it.
fn expected_application_objects() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("table:memory_meta", CREATE_MEMORY_META),
        ("table:memory_records", CREATE_MEMORY_RECORDS),
        ("table:memory_observations", CREATE_MEMORY_OBSERVATIONS),
        ("table:memory_candidates", CREATE_MEMORY_CANDIDATES),
        ("table:memory_records_fts", CREATE_MEMORY_RECORDS_FTS),
        (
            "index:memory_records_key_active",
            CREATE_MEMORY_RECORDS_KEY_ACTIVE,
        ),
        ("index:memory_records_list", CREATE_MEMORY_RECORDS_LIST),
        (
            "index:memory_candidates_list",
            CREATE_MEMORY_CANDIDATES_LIST,
        ),
        (
            "index:memory_candidates_observation",
            CREATE_MEMORY_CANDIDATES_OBSERVATION,
        ),
    ])
}

fn expected_application_object_tables() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("table:memory_meta", "memory_meta"),
        ("table:memory_records", "memory_records"),
        ("table:memory_observations", "memory_observations"),
        ("table:memory_candidates", "memory_candidates"),
        ("table:memory_records_fts", "memory_records_fts"),
        ("index:memory_records_key_active", "memory_records"),
        ("index:memory_records_list", "memory_records"),
        ("index:memory_candidates_list", "memory_candidates"),
        ("index:memory_candidates_observation", "memory_candidates"),
    ])
}

/// The only SQLite-created indexes schema v1 sanctions. Their `sqlite_schema`
/// SQL must stay NULL and their owning table is pinned.
fn expected_sqlite_autoindexes() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("sqlite_autoindex_memory_meta_1", "memory_meta"),
        ("sqlite_autoindex_memory_records_1", "memory_records"),
        (
            "sqlite_autoindex_memory_observations_1",
            "memory_observations",
        ),
        ("sqlite_autoindex_memory_candidates_1", "memory_candidates"),
    ])
}

/// The FTS5 shadow tables the virtual-table statement creates. Accepting merely
/// compatible names and columns would admit a database whose storage schema was
/// not produced by the compiled manifest.
fn expected_fts_shadow_tables() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        (
            "memory_records_fts_config",
            "CREATE TABLE 'memory_records_fts_config'(k PRIMARY KEY, v) WITHOUT ROWID",
        ),
        (
            "memory_records_fts_content",
            "CREATE TABLE 'memory_records_fts_content'(id INTEGER PRIMARY KEY, c0, c1, c2, c3, c4)",
        ),
        (
            "memory_records_fts_data",
            "CREATE TABLE 'memory_records_fts_data'(id INTEGER PRIMARY KEY, block BLOB)",
        ),
        (
            "memory_records_fts_docsize",
            "CREATE TABLE 'memory_records_fts_docsize'(id INTEGER PRIMARY KEY, sz BLOB)",
        ),
        (
            "memory_records_fts_idx",
            "CREATE TABLE 'memory_records_fts_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID",
        ),
    ])
}

/// Normalizes one statement: trim, drop one trailing semicolon, collapse every
/// whitespace run to a single space.
pub fn normalize_schema_sql(statement: &str) -> String {
    let trimmed = statement.trim();
    let trimmed = trimmed.strip_suffix(';').unwrap_or(trimmed);
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A valid database ID: exactly 32 lowercase hex digits.
pub fn valid_database_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn user_version(conn: &Connection) -> Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(super::map_sqlite_error)
}

fn schema_object_count(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
        .map_err(super::map_sqlite_error)
}

/// Creates schema v1 in an empty database, or verifies an existing one.
///
/// The caller holds the write transaction; this runs inside `BEGIN IMMEDIATE`
/// and leaves committing to the caller.
pub fn initialize_schema(conn: &Connection, database_id: &str, user_id: &str) -> Result<()> {
    let version = user_version(conn)?;
    match version {
        0 => {
            if schema_object_count(conn)? != 0 {
                return Err(Error::new(ErrorKind::Corrupt));
            }
            for statement in SCHEMA_STATEMENTS {
                conn.execute_batch(statement)
                    .map_err(super::map_sqlite_error)?;
            }
            for (key, value) in [
                ("database_id", database_id),
                ("user_scope_id", user_id),
                ("generation", "0"),
                ("schema_fingerprint", COMPILED_SCHEMA_FINGERPRINT),
            ] {
                conn.execute(
                    "INSERT INTO memory_meta(key,value) VALUES(?,?)",
                    rusqlite::params![key, value],
                )
                .map_err(super::map_sqlite_error)?;
            }
            conn.execute_batch("PRAGMA user_version=1")
                .map_err(super::map_sqlite_error)?;
            Ok(())
        }
        v if v == SCHEMA_VERSION => Ok(()),
        v if v > SCHEMA_VERSION => Err(Error::new(ErrorKind::IncompatibleSchema)),
        _ => Err(Error::new(ErrorKind::Corrupt)),
    }
}

/// Every deviation from the compiled manifest is [`ErrorKind::Corrupt`]; a
/// newer schema version is [`ErrorKind::IncompatibleSchema`].
pub fn verify_schema(conn: &Connection) -> Result<StoreIdentity> {
    let version = user_version(conn)?;
    if version != SCHEMA_VERSION {
        return Err(Error::new(if version > SCHEMA_VERSION {
            ErrorKind::IncompatibleSchema
        } else {
            ErrorKind::Corrupt
        }));
    }

    let applications = expected_application_objects();
    let application_tables = expected_application_object_tables();
    let shadows = expected_fts_shadow_tables();
    let autoindexes = expected_sqlite_autoindexes();
    let expected_count = (applications.len() + shadows.len() + autoindexes.len()) as i64;
    if schema_object_count(conn)? != expected_count {
        return Err(Error::new(ErrorKind::Corrupt));
    }

    let mut objects: BTreeMap<String, (String, String, Option<String>)> = BTreeMap::new();
    let mut statement = conn
        .prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE sql IS NULL OR length(sql)<=131072")
        .map_err(super::map_sqlite_error)?;
    let mut rows = statement.query([]).map_err(super::map_sqlite_error)?;
    while let Some(row) = rows.next().map_err(super::map_sqlite_error)? {
        let name: String = row.get(1).map_err(super::map_sqlite_error)?;
        let entry = (
            row.get::<_, String>(0).map_err(super::map_sqlite_error)?,
            row.get::<_, String>(2).map_err(super::map_sqlite_error)?,
            row.get::<_, Option<String>>(3)
                .map_err(super::map_sqlite_error)?,
        );
        if objects.insert(name, entry).is_some() {
            return Err(Error::new(ErrorKind::Corrupt));
        }
    }
    drop(rows);
    if objects.len() as i64 != expected_count {
        return Err(Error::new(ErrorKind::Corrupt));
    }

    for (key, expected) in &applications {
        let (kind, name) = key
            .split_once(':')
            .ok_or_else(|| Error::new(ErrorKind::Corrupt))?;
        let Some((actual_kind, actual_table, Some(sql))) = objects.get(name) else {
            return Err(Error::new(ErrorKind::Corrupt));
        };
        if actual_kind != kind
            || Some(actual_table.as_str()) != application_tables.get(key).copied()
            || normalize_schema_sql(sql) != normalize_schema_sql(expected)
        {
            return Err(Error::new(ErrorKind::Corrupt));
        }
    }
    for (table, expected) in &shadows {
        let Some((kind, actual_table, Some(sql))) = objects.get(*table) else {
            return Err(Error::new(ErrorKind::Corrupt));
        };
        if kind != "table"
            || actual_table != table
            || normalize_schema_sql(sql) != normalize_schema_sql(expected)
        {
            return Err(Error::new(ErrorKind::Corrupt));
        }
    }
    for (name, table) in &autoindexes {
        let Some((kind, actual_table, sql)) = objects.get(*name) else {
            return Err(Error::new(ErrorKind::Corrupt));
        };
        if kind != "index" || actual_table != table || sql.is_some() {
            return Err(Error::new(ErrorKind::Corrupt));
        }
    }

    let meta_count: i64 = conn
        .query_row("SELECT count(*) FROM memory_meta", [], |row| row.get(0))
        .map_err(super::map_sqlite_error)?;
    if meta_count != 4 {
        return Err(Error::new(ErrorKind::Corrupt));
    }
    let mut meta_statement = conn
        .prepare(
            "SELECT key,value FROM memory_meta
             WHERE key IN ('database_id','user_scope_id','generation','schema_fingerprint')
               AND typeof(key)='text' AND length(key) BETWEEN 1 AND 64
               AND typeof(value)='text' AND length(value)<=256
             LIMIT 5",
        )
        .map_err(super::map_sqlite_error)?;
    let mut meta_rows = meta_statement.query([]).map_err(super::map_sqlite_error)?;
    let mut meta: BTreeMap<String, String> = BTreeMap::new();
    while let Some(row) = meta_rows.next().map_err(super::map_sqlite_error)? {
        let key: String = row.get(0).map_err(super::map_sqlite_error)?;
        let value: String = row.get(1).map_err(super::map_sqlite_error)?;
        if meta.len() >= 4 || meta.insert(key, value).is_some() {
            return Err(Error::new(ErrorKind::Corrupt));
        }
    }
    drop(meta_rows);
    if meta.len() != 4
        || meta.get("schema_fingerprint").map(String::as_str) != Some(COMPILED_SCHEMA_FINGERPRINT)
    {
        return Err(Error::new(ErrorKind::Corrupt));
    }
    let database_id = meta["database_id"].clone();
    let user_id = meta["user_scope_id"].clone();
    if !valid_database_id(&database_id) || !valid_database_id(&user_id) || database_id == user_id {
        return Err(Error::new(ErrorKind::Corrupt));
    }
    let generation_text = &meta["generation"];
    let generation: u64 = generation_text
        .parse()
        .map_err(|_| Error::new(ErrorKind::Corrupt))?;
    if generation.to_string() != *generation_text {
        return Err(Error::new(ErrorKind::Corrupt));
    }

    Ok(StoreIdentity {
        database_id,
        user_scope: Scope {
            namespace: crate::memory::NAMESPACE_USER.to_string(),
            id: user_id,
        },
        schema_version: SCHEMA_VERSION,
        generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_fingerprint_matches_the_source_constant() {
        assert_eq!(
            fingerprint_manifest(&schema_manifest()),
            COMPILED_SCHEMA_FINGERPRINT,
            "a schema statement changed; an existing database file would no \
             longer open"
        );
    }

    #[test]
    fn normalization_collapses_whitespace_and_drops_the_terminator() {
        assert_eq!(
            normalize_schema_sql("  CREATE  TABLE\n  t (a);  "),
            "CREATE TABLE t (a)"
        );
    }

    #[test]
    fn database_ids_are_thirty_two_lowercase_hex_digits() {
        assert!(valid_database_id(&"0123456789abcdef".repeat(2)));
        assert!(!valid_database_id("0123456789ABCDEF0123456789abcdef"));
        assert!(!valid_database_id("short"));
    }
}
