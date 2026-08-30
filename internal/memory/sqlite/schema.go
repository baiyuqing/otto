package sqlite

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"strconv"
	"strings"

	"github.com/baiyuqing/otto/internal/memory"
)

const schemaVersion = 1

const createMemoryMeta = `CREATE TABLE memory_meta (
    key TEXT PRIMARY KEY CHECK (length(key) BETWEEN 1 AND 64),
    value TEXT NOT NULL CHECK (length(value) <= 256)
) STRICT`

const createMemoryRecords = `CREATE TABLE memory_records (
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
) STRICT`

const createMemoryRecordsKeyActive = `CREATE UNIQUE INDEX memory_records_key_active
ON memory_records(scope_namespace, scope_id, kind, semantic_key)
WHERE state = 'active' AND semantic_key <> ''`

const createMemoryRecordsList = `CREATE INDEX memory_records_list
ON memory_records(scope_namespace, scope_id, state, updated_at DESC, id ASC)`

const createMemoryObservations = `CREATE TABLE memory_observations (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 64),
    candidate_ids_json TEXT NOT NULL CHECK (length(CAST(candidate_ids_json AS BLOB)) <= 1024 AND json_valid(candidate_ids_json) AND json_type(candidate_ids_json) = 'array'),
    created_at TEXT NOT NULL CHECK (length(created_at) = 30)
) STRICT`

const createMemoryCandidates = `CREATE TABLE memory_candidates (
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
) STRICT`

const createMemoryCandidatesList = `CREATE INDEX memory_candidates_list
ON memory_candidates(scope_namespace, scope_id, state, created_at DESC, id ASC)`

const createMemoryCandidatesObservation = `CREATE INDEX memory_candidates_observation
ON memory_candidates(observation_id)
WHERE observation_id IS NOT NULL`

const createMemoryRecordsFTS = `CREATE VIRTUAL TABLE memory_records_fts USING fts5(
    record_id UNINDEXED,
    text_value,
    kind,
    semantic_key,
    labels,
    tokenize = 'unicode61'
)`

var schemaStatements = []string{
	createMemoryMeta,
	createMemoryRecords,
	createMemoryRecordsKeyActive,
	createMemoryRecordsList,
	createMemoryObservations,
	createMemoryCandidates,
	createMemoryCandidatesList,
	createMemoryCandidatesObservation,
	createMemoryRecordsFTS,
}

var schemaManifest = strings.Join(schemaStatements, ";\n")

// compiledSchemaFingerprint is SHA-256(schemaManifest). It is deliberately a
// source constant rather than a value learned from an opened database.
const compiledSchemaFingerprint = "f927b04baf82340748b4af92984d0f165f734acb4ea3e539f190e79dd54847e9"

var expectedApplicationObjects = map[string]string{
	"table:memory_meta":                   createMemoryMeta,
	"table:memory_records":                createMemoryRecords,
	"table:memory_observations":           createMemoryObservations,
	"table:memory_candidates":             createMemoryCandidates,
	"table:memory_records_fts":            createMemoryRecordsFTS,
	"index:memory_records_key_active":     createMemoryRecordsKeyActive,
	"index:memory_records_list":           createMemoryRecordsList,
	"index:memory_candidates_list":        createMemoryCandidatesList,
	"index:memory_candidates_observation": createMemoryCandidatesObservation,
}

var expectedApplicationObjectTables = map[string]string{
	"table:memory_meta":                   "memory_meta",
	"table:memory_records":                "memory_records",
	"table:memory_observations":           "memory_observations",
	"table:memory_candidates":             "memory_candidates",
	"table:memory_records_fts":            "memory_records_fts",
	"index:memory_records_key_active":     "memory_records",
	"index:memory_records_list":           "memory_records",
	"index:memory_candidates_list":        "memory_candidates",
	"index:memory_candidates_observation": "memory_candidates",
}

// These are the only SQLite-created indexes sanctioned by schema v1. Their
// sqlite_schema SQL must remain NULL and their owning table is pinned.
var expectedSQLiteAutoindexes = map[string]string{
	"sqlite_autoindex_memory_meta_1":         "memory_meta",
	"sqlite_autoindex_memory_records_1":      "memory_records",
	"sqlite_autoindex_memory_observations_1": "memory_observations",
	"sqlite_autoindex_memory_candidates_1":   "memory_candidates",
}

var expectedFTSShadowTables = []string{
	"memory_records_fts_config",
	"memory_records_fts_content",
	"memory_records_fts_data",
	"memory_records_fts_docsize",
	"memory_records_fts_idx",
}

var expectedFTSShadowColumns = map[string][]string{
	"memory_records_fts_config":  {"k", "v"},
	"memory_records_fts_content": {"id", "c0", "c1", "c2", "c3", "c4"},
	"memory_records_fts_data":    {"id", "block"},
	"memory_records_fts_docsize": {"id", "sz"},
	"memory_records_fts_idx":     {"segid", "term", "pgno"},
}

func fingerprintManifest(manifest string) string {
	sum := sha256.Sum256([]byte(manifest))
	return hex.EncodeToString(sum[:])
}

func normalizeSchemaSQL(statement string) string {
	statement = strings.TrimSuffix(strings.TrimSpace(statement), ";")
	return strings.Join(strings.Fields(statement), " ")
}

func inspectPreflight(ctx context.Context, conn *sql.Conn) (memory.StoreIdentity, bool, error) {
	version, err := readUserVersion(ctx, conn)
	if err != nil {
		return memory.StoreIdentity{}, false, safeSQLiteError(ctx, err)
	}
	switch {
	case version > schemaVersion:
		return memory.StoreIdentity{}, false, memory.ErrIncompatibleSchema
	case version < 0:
		return memory.StoreIdentity{}, false, memory.ErrCorrupt
	case version == 0:
		var count int
		if err := conn.QueryRowContext(ctx, `SELECT count(*) FROM sqlite_schema`).Scan(&count); err != nil {
			return memory.StoreIdentity{}, false, safeSQLiteError(ctx, err)
		}
		if count != 0 {
			return memory.StoreIdentity{}, false, memory.ErrCorrupt
		}
		return memory.StoreIdentity{}, true, nil
	case version == schemaVersion:
		identity, err := verifySchema(ctx, conn)
		return identity, false, err
	default:
		return memory.StoreIdentity{}, false, memory.ErrIncompatibleSchema
	}
}

func initializeOrVerifySchema(ctx context.Context, conn *sql.Conn, databaseID, userID string) (identity memory.StoreIdentity, err error) {
	if _, err = conn.ExecContext(ctx, "BEGIN IMMEDIATE"); err != nil {
		return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
	}
	committed := false
	defer func() {
		if !committed {
			_, _ = conn.ExecContext(context.Background(), "ROLLBACK")
		}
	}()

	version, err := readUserVersion(ctx, conn)
	if err != nil {
		return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
	}
	switch version {
	case 0:
		var count int
		if err := conn.QueryRowContext(ctx, `SELECT count(*) FROM sqlite_schema`).Scan(&count); err != nil {
			return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
		}
		if count != 0 {
			return memory.StoreIdentity{}, memory.ErrCorrupt
		}
		for _, statement := range schemaStatements {
			if _, err := conn.ExecContext(ctx, statement); err != nil {
				return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
			}
		}
		for _, pair := range [][2]string{
			{"database_id", databaseID},
			{"user_scope_id", userID},
			{"generation", "0"},
			{"schema_fingerprint", compiledSchemaFingerprint},
		} {
			if _, err := conn.ExecContext(ctx, `INSERT INTO memory_meta(key,value) VALUES(?,?)`, pair[0], pair[1]); err != nil {
				return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
			}
		}
		if _, err := conn.ExecContext(ctx, "PRAGMA user_version=1"); err != nil {
			return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
		}
	case schemaVersion:
		// A competing initializer won. Verification below reuses its identity.
	default:
		if version > schemaVersion {
			return memory.StoreIdentity{}, memory.ErrIncompatibleSchema
		}
		return memory.StoreIdentity{}, memory.ErrCorrupt
	}
	if _, err := conn.ExecContext(context.Background(), "COMMIT"); err != nil {
		return memory.StoreIdentity{}, memory.ErrCommitUnknown
	}
	committed = true
	return verifySchema(ctx, conn)
}

func readUserVersion(ctx context.Context, conn *sql.Conn) (int, error) {
	var version int
	err := conn.QueryRowContext(ctx, "PRAGMA user_version").Scan(&version)
	return version, err
}

func verifySchema(ctx context.Context, conn *sql.Conn) (memory.StoreIdentity, error) {
	version, err := readUserVersion(ctx, conn)
	if err != nil {
		return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
	}
	if version != schemaVersion {
		if version > schemaVersion {
			return memory.StoreIdentity{}, memory.ErrIncompatibleSchema
		}
		return memory.StoreIdentity{}, memory.ErrCorrupt
	}

	expectedObjectCount := len(expectedApplicationObjects) + len(expectedFTSShadowTables) + len(expectedSQLiteAutoindexes)
	var objectCount int
	if err := conn.QueryRowContext(ctx, `SELECT count(*) FROM sqlite_schema`).Scan(&objectCount); err != nil {
		return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
	}
	if objectCount != expectedObjectCount {
		return memory.StoreIdentity{}, memory.ErrCorrupt
	}

	expectedNames := make([]string, 0, expectedObjectCount)
	for key := range expectedApplicationObjects {
		_, name, ok := strings.Cut(key, ":")
		if !ok {
			return memory.StoreIdentity{}, memory.ErrCorrupt
		}
		expectedNames = append(expectedNames, name)
	}
	expectedNames = append(expectedNames, expectedFTSShadowTables...)
	for name := range expectedSQLiteAutoindexes {
		expectedNames = append(expectedNames, name)
	}
	placeholders := strings.TrimSuffix(strings.Repeat("?,", len(expectedNames)), ",")
	arguments := make([]any, len(expectedNames)+1)
	for index, name := range expectedNames {
		arguments[index] = name
	}
	arguments[len(expectedNames)] = expectedObjectCount + 1
	rows, err := conn.QueryContext(ctx, `SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE name IN (`+placeholders+`) AND (sql IS NULL OR length(sql)<=131072) LIMIT ?`, arguments...)
	if err != nil {
		return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
	}
	type schemaObject struct {
		typ, table string
		statement  sql.NullString
	}
	objects := make(map[string]schemaObject, expectedObjectCount)
	for rows.Next() {
		if len(objects) >= expectedObjectCount {
			_ = rows.Close()
			return memory.StoreIdentity{}, memory.ErrCorrupt
		}
		var typ, name, table string
		var statement sql.NullString
		if err := rows.Scan(&typ, &name, &table, &statement); err != nil {
			_ = rows.Close()
			return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
		}
		if _, duplicate := objects[name]; duplicate {
			_ = rows.Close()
			return memory.StoreIdentity{}, memory.ErrCorrupt
		}
		objects[name] = schemaObject{typ: typ, table: table, statement: statement}
	}
	if err := rows.Err(); err != nil {
		_ = rows.Close()
		return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
	}
	if err := rows.Close(); err != nil {
		return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
	}
	if len(objects) != expectedObjectCount {
		return memory.StoreIdentity{}, memory.ErrCorrupt
	}
	for key, expected := range expectedApplicationObjects {
		typ, name, _ := strings.Cut(key, ":")
		actual, ok := objects[name]
		if !ok || actual.typ != typ || actual.table != expectedApplicationObjectTables[key] || !actual.statement.Valid || normalizeSchemaSQL(actual.statement.String) != normalizeSchemaSQL(expected) {
			return memory.StoreIdentity{}, memory.ErrCorrupt
		}
	}
	for _, table := range expectedFTSShadowTables {
		actual, ok := objects[table]
		if !ok || actual.typ != "table" || actual.table != table || !actual.statement.Valid {
			return memory.StoreIdentity{}, memory.ErrCorrupt
		}
		if err := verifyTableColumns(ctx, conn, table, expectedFTSShadowColumns[table]); err != nil {
			return memory.StoreIdentity{}, err
		}
	}
	for name, table := range expectedSQLiteAutoindexes {
		actual, ok := objects[name]
		if !ok || actual.typ != "index" || actual.table != table || actual.statement.Valid {
			return memory.StoreIdentity{}, memory.ErrCorrupt
		}
	}

	var metaCount int
	if err := conn.QueryRowContext(ctx, `SELECT count(*) FROM memory_meta`).Scan(&metaCount); err != nil {
		return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
	}
	if metaCount != 4 {
		return memory.StoreIdentity{}, memory.ErrCorrupt
	}
	metaRows, err := conn.QueryContext(ctx, `
		SELECT key,value FROM memory_meta
		WHERE key IN ('database_id','user_scope_id','generation','schema_fingerprint')
		  AND typeof(key)='text' AND length(key) BETWEEN 1 AND 64
		  AND typeof(value)='text' AND length(value)<=256
		LIMIT 5`)
	if err != nil {
		return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
	}
	meta := make(map[string]string, 4)
	for metaRows.Next() {
		if len(meta) >= 4 {
			_ = metaRows.Close()
			return memory.StoreIdentity{}, memory.ErrCorrupt
		}
		var key, value string
		if err := metaRows.Scan(&key, &value); err != nil {
			_ = metaRows.Close()
			return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
		}
		if _, duplicate := meta[key]; duplicate {
			_ = metaRows.Close()
			return memory.StoreIdentity{}, memory.ErrCorrupt
		}
		meta[key] = value
	}
	if err := metaRows.Err(); err != nil {
		_ = metaRows.Close()
		return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
	}
	if err := metaRows.Close(); err != nil {
		return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
	}
	if len(meta) != 4 || meta["schema_fingerprint"] != compiledSchemaFingerprint {
		return memory.StoreIdentity{}, memory.ErrCorrupt
	}
	databaseID, userID := meta["database_id"], meta["user_scope_id"]
	if !validDatabaseID(databaseID) || !validDatabaseID(userID) || databaseID == userID {
		return memory.StoreIdentity{}, memory.ErrCorrupt
	}
	generationText := meta["generation"]
	generation, err := strconv.ParseUint(generationText, 10, 64)
	if err != nil || strconv.FormatUint(generation, 10) != generationText {
		return memory.StoreIdentity{}, memory.ErrCorrupt
	}
	return memory.StoreIdentity{
		DatabaseID:    databaseID,
		UserScope:     memory.Scope{Namespace: "user", ID: userID},
		SchemaVersion: schemaVersion,
		Generation:    generation,
	}, nil
}

func verifyTableColumns(ctx context.Context, conn *sql.Conn, table string, expected []string) error {
	// Table names and the row bound come only from the compiled manifest.
	rows, err := conn.QueryContext(ctx, `SELECT name FROM pragma_table_xinfo(?) LIMIT ?`, table, len(expected)+1)
	if err != nil {
		return safeSQLiteError(ctx, err)
	}
	count := 0
	for rows.Next() {
		if count >= len(expected) {
			_ = rows.Close()
			return memory.ErrCorrupt
		}
		var name string
		if err := rows.Scan(&name); err != nil {
			_ = rows.Close()
			return safeSQLiteError(ctx, err)
		}
		if name != expected[count] {
			_ = rows.Close()
			return memory.ErrCorrupt
		}
		count++
	}
	if err := rows.Err(); err != nil {
		_ = rows.Close()
		return safeSQLiteError(ctx, err)
	}
	if err := rows.Close(); err != nil {
		return safeSQLiteError(ctx, err)
	}
	if count != len(expected) {
		return memory.ErrCorrupt
	}
	return nil
}

func validDatabaseID(value string) bool {
	if len(value) != 32 {
		return false
	}
	for _, c := range value {
		if (c < '0' || c > '9') && (c < 'a' || c > 'f') {
			return false
		}
	}
	return true
}
