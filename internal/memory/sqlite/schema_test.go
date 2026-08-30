package sqlite

import (
	"context"
	"database/sql"
	"errors"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/baiyuqing/otto/internal/memory"
)

func TestSchemaV1ManifestFeaturesAndFingerprint(t *testing.T) {
	path := filepath.Join(t.TempDir(), "schema", "memory.db")
	store := openTestStore(t, path)
	conn := borrowTestConn(t, store)

	if got := queryInt(t, conn, "PRAGMA user_version"); got != schemaVersion {
		t.Fatalf("user_version = %d", got)
	}
	rows, err := conn.QueryContext(context.Background(), `
		SELECT type, name, sql FROM sqlite_schema
		WHERE name NOT LIKE 'sqlite_%'
		ORDER BY type, name`)
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()
	objects := make(map[string]string)
	var names []string
	for rows.Next() {
		var typ, name string
		var statement sql.NullString
		if err := rows.Scan(&typ, &name, &statement); err != nil {
			t.Fatal(err)
		}
		objects[typ+":"+name] = statement.String
		names = append(names, typ+":"+name)
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	for key, statement := range expectedApplicationObjects {
		if got, ok := objects[key]; !ok || normalizeSchemaSQL(got) != normalizeSchemaSQL(statement) {
			t.Errorf("schema object %s missing or changed", key)
		}
	}
	for _, shadow := range expectedFTSShadowTables {
		if _, ok := objects["table:"+shadow]; !ok {
			t.Errorf("missing FTS shadow table %s (objects: %v)", shadow, names)
		}
	}
	if got, want := queryString(t, conn, `SELECT value FROM memory_meta WHERE key='schema_fingerprint'`), compiledSchemaFingerprint; got != want {
		t.Fatalf("schema fingerprint = %q, want %q", got, want)
	}
	if calculated := fingerprintManifest(schemaManifest); calculated != compiledSchemaFingerprint {
		t.Fatalf("compiled fingerprint = %q, calculated = %q", compiledSchemaFingerprint, calculated)
	}
	if got := queryInt(t, conn, `SELECT json_valid('{"ok":[1,2]}')`); got != 1 {
		t.Fatalf("json_valid = %d", got)
	}
	if got := queryInt(t, conn, `SELECT count(*) FROM json_each('[1,2,3]')`); got != 3 {
		t.Fatalf("json_each count = %d", got)
	}
	if _, err := conn.ExecContext(context.Background(), `INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels)
		VALUES('fts-record','ordinary searchable text','note','key','label')`); err != nil {
		t.Fatalf("FTS insert: %v", err)
	}
	if got := queryString(t, conn, `SELECT record_id FROM memory_records_fts WHERE memory_records_fts MATCH 'searchable'`); got != "fts-record" {
		t.Fatalf("FTS query = %q", got)
	}

	sorted := append([]string(nil), names...)
	sort.Strings(sorted)
	if len(sorted) != len(objects) {
		t.Fatal("duplicate sqlite_schema object names")
	}
}

func TestSchemaV1RejectsInvalidRows(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
	conn := borrowTestConn(t, store)
	ctx := context.Background()

	validRecord := `INSERT INTO memory_records(
		id,scope_namespace,scope_id,kind,semantic_key,text_value,labels_json,metadata_json,source_json,
		confidence,revision,created_at,updated_at,expires_at,state,forgotten_at
	) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`
	timestamp := "2026-01-01T00:00:00.000000000Z"
	validArgs := []any{"record", "user", "scope", "note", "key", "text", `[]`, `{}`, `{}`, 0.8, 1, timestamp, timestamp, nil, "active", nil}
	if _, err := conn.ExecContext(ctx, validRecord, validArgs...); err != nil {
		t.Fatalf("valid record: %v", err)
	}

	recordCases := []struct {
		name string
		edit func([]any)
	}{
		{"scalar labels JSON", func(a []any) { a[0] = "scalar"; a[6] = `"not-array"` }},
		{"wrong metadata shape", func(a []any) { a[0] = "wrong-shape"; a[7] = `[]` }},
		{"tombstone with content", func(a []any) {
			a[0], a[14], a[15] = "bad-tombstone", "tombstone", timestamp
		}},
	}
	for _, tc := range recordCases {
		t.Run(tc.name, func(t *testing.T) {
			args := append([]any(nil), validArgs...)
			tc.edit(args)
			if _, err := conn.ExecContext(ctx, validRecord, args...); err == nil {
				t.Fatal("invalid record insert succeeded")
			}
		})
	}

	if _, err := conn.ExecContext(ctx, `INSERT INTO memory_observations(id,candidate_ids_json,created_at) VALUES('obs','[]',?)`, timestamp); err != nil {
		t.Fatal(err)
	}
	candidateInsert := `INSERT INTO memory_candidates(
		id,scope_namespace,scope_id,action,target_id,base_revision,observation_id,proposed_json,reason,state,
		created_at,decided_at,decision_source,result_record_id,result_revision
	) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`
	proposed := `{"scope_namespace":"user","scope_id":"scope","kind":"note","key":"key","text":"text","labels":[],"metadata":{},"source":{"observation_id":"obs"},"confidence":1,"expiry":null}`
	validCandidate := []any{"candidate", "user", "scope", "create", "", 0, "obs", proposed, "because", "pending", timestamp, nil, "", "", 0}
	if _, err := conn.ExecContext(ctx, candidateInsert, validCandidate...); err != nil {
		t.Fatalf("valid candidate: %v", err)
	}

	candidateCases := []struct {
		name string
		edit func([]any)
	}{
		{"action target base mismatch", func(a []any) { a[0] = "bad-action"; a[3], a[4], a[5] = "update", "", 0 }},
		{"proposed scope mismatch", func(a []any) {
			a[0] = "bad-scope"
			a[7] = strings.Replace(proposed, `"scope_id":"scope"`, `"scope_id":"other"`, 1)
		}},
		{"observation source mismatch", func(a []any) {
			a[0] = "bad-source"
			a[7] = strings.Replace(proposed, `"observation_id":"obs"`, `"observation_id":"other"`, 1)
		}},
		{"inconsistent accepted state", func(a []any) {
			a[0], a[9], a[11], a[12], a[13], a[14] = "bad-state", "accepted", timestamp, "reviewer", "record", 1
		}},
		{"missing observation foreign key", func(a []any) { a[0], a[6] = "bad-fk", "missing" }},
	}
	for _, tc := range candidateCases {
		t.Run(tc.name, func(t *testing.T) {
			args := append([]any(nil), validCandidate...)
			tc.edit(args)
			if _, err := conn.ExecContext(ctx, candidateInsert, args...); err == nil {
				t.Fatal("invalid candidate insert succeeded")
			}
		})
	}
}

func TestSchemaPreflightRefusesUnknownLayoutsWithoutWrites(t *testing.T) {
	for _, tc := range []struct {
		name      string
		statement string
		category  error
	}{
		{"future version", `PRAGMA journal_mode=DELETE; PRAGMA user_version=2`, memory.ErrIncompatibleSchema},
		{"version zero unknown object", `PRAGMA journal_mode=DELETE; CREATE TABLE unknown_fixture(value TEXT)`, memory.ErrCorrupt},
	} {
		t.Run(tc.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "preflight-refusal-marker", "memory.db")
			createRawFixture(t, path, tc.statement)
			before := fileDigest(t, path)
			var writeOpen atomic.Bool
			installTestHooks(t, testHooks{path: func(event pathEvent) {
				if event == pathBeforeWriteDriverOpen {
					writeOpen.Store(true)
				}
			}})
			_, err := Open(context.Background(), path, testOptions(t))
			assertSafeError(t, err, tc.category, path, "preflight-refusal-marker")
			if writeOpen.Load() {
				t.Fatal("write-capable connection opened before refusal")
			}
			if after := fileDigest(t, path); after != before {
				t.Fatal("refused fixture bytes changed")
			}
			for _, suffix := range []string{"-wal", "-shm"} {
				if _, statErr := os.Lstat(path + suffix); !errors.Is(statErr, os.ErrNotExist) {
					t.Fatalf("preflight created %s: %v", suffix, statErr)
				}
			}
		})
	}
}

func TestSchemaV1CorruptionIsNeverRepaired(t *testing.T) {
	cases := []struct {
		name   string
		mutate func(t *testing.T, db *sql.DB)
	}{
		{"added table", func(t *testing.T, db *sql.DB) {
			if _, err := db.Exec(`CREATE TABLE added_object(value TEXT)`); err != nil {
				t.Fatal(err)
			}
		}},
		{"dropped table", func(t *testing.T, db *sql.DB) {
			if _, err := db.Exec(`DROP TABLE memory_observations`); err != nil {
				t.Fatal(err)
			}
		}},
		{"changed table", func(t *testing.T, db *sql.DB) {
			if _, err := db.Exec(`ALTER TABLE memory_records ADD COLUMN unexpected TEXT`); err != nil {
				t.Fatal(err)
			}
		}},
		{"dropped index", func(t *testing.T, db *sql.DB) {
			if _, err := db.Exec(`DROP INDEX memory_records_list`); err != nil {
				t.Fatal(err)
			}
		}},
		{"changed index", func(t *testing.T, db *sql.DB) {
			if _, err := db.Exec(`DROP INDEX memory_candidates_list; CREATE INDEX memory_candidates_list ON memory_candidates(id)`); err != nil {
				t.Fatal(err)
			}
		}},
		{"changed FTS definition", func(t *testing.T, db *sql.DB) {
			if _, err := db.Exec(`DROP TABLE memory_records_fts; CREATE VIRTUAL TABLE memory_records_fts USING fts5(record_id UNINDEXED,text_value,kind,semantic_key,labels,tokenize='porter')`); err != nil {
				t.Fatal(err)
			}
		}},
		{"changed fingerprint", func(t *testing.T, db *sql.DB) {
			if _, err := db.Exec(`UPDATE memory_meta SET value='ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff' WHERE key='schema_fingerprint'`); err != nil {
				t.Fatal(err)
			}
		}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "schema-corruption-marker", "memory.db")
			store := openTestStore(t, path)
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			db := openRaw(t, path)
			tc.mutate(t, db)
			if err := db.Close(); err != nil {
				t.Fatal(err)
			}
			before := fileDigest(t, path)
			_, err := Open(context.Background(), path, Options{Guard: testGuard(t), NewID: memory.NewID})
			assertSafeError(t, err, memory.ErrCorrupt, path, "schema-corruption-marker")
			if after := fileDigest(t, path); after != before {
				t.Fatal("Open auto-repaired or changed corrupt schema")
			}
		})
	}
}
