package sqlite

import (
	"context"
	"database/sql"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

func TestRecordCorruptionIsLengthGatedAndGuarded(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path)
	now := time.Date(2026, 8, 29, 14, 0, 0, 123, time.UTC)
	base := sqliteTestRecord("safe-record", "safe", now)
	mustSQLiteCreate(t, store, base)

	external := openExternalSQLite(t, path)
	corruptions := []struct {
		id     string
		mutate func(*rawRecord)
		want   error
		secret string
	}{
		{"oversize-text", func(row *rawRecord) { row.text = strings.Repeat("x", memory.MaxRecordTextBytes+1) }, memory.ErrCorrupt, ""},
		{"oversize-json", func(row *rawRecord) { row.metadata = `{"x":"` + strings.Repeat("m", memory.MaxMetadataBytes) + `"}` }, memory.ErrCorrupt, ""},
		{"invalid-json", func(row *rawRecord) { row.labels = `{"invalid":true}` }, memory.ErrCorrupt, ""},
		{"bad-timestamp", func(row *rawRecord) { row.updated = strings.TrimSuffix(row.updated, "Z") + "z" }, memory.ErrCorrupt, ""},
		{"redaction-row", func(row *rawRecord) { row.text = "prefix [REDACTED] suffix" }, memory.ErrSensitiveMemory, "[REDACTED]"},
		{"huge-row", func(row *rawRecord) { row.text = strings.Repeat("HUGE-PROJECTION-", (1<<20)/16+1) }, memory.ErrCorrupt, "HUGE-PROJECTION"},
	}
	for index, corruption := range corruptions {
		record := sqliteTestRecord(corruption.id, corruption.id, now.Add(time.Duration(index+1)*time.Nanosecond))
		row := makeRawRecord(t, record)
		corruption.mutate(&row)
		insertRawRecord(t, external, row)
	}
	tombstone := makeRawRecord(t, sqliteTestRecord("hidden-tombstone", "hidden", now.Add(time.Hour)))
	tombstone.state = "tombstone"
	insertRawRecord(t, external, tombstone)
	if err := external.Close(); err != nil {
		t.Fatal(err)
	}

	for _, corruption := range corruptions {
		_, err := store.Get(context.Background(), memory.RecordRef{Scope: base.Scope, ID: corruption.id})
		assertRecordSafeError(t, err, corruption.want, corruption.secret)
	}
	_, err := store.List(context.Background(), memory.ListRequest{Scopes: []memory.Scope{base.Scope}, Limit: memory.MaxPageSize, IncludeExpired: true})
	if !errors.Is(err, memory.ErrCorrupt) && !errors.Is(err, memory.ErrSensitiveMemory) {
		t.Fatalf("List corruption error = %v", err)
	}
	if strings.Contains(fmt.Sprint(err), "[REDACTED]") || strings.Contains(fmt.Sprint(err), "HUGE-PROJECTION") {
		t.Fatalf("List echoed corrupt content: %v", err)
	}
	if _, err := store.Get(context.Background(), memory.RecordRef{Scope: base.Scope, ID: tombstone.id}); !errors.Is(err, memory.ErrNotFound) {
		t.Fatalf("tombstone exact Get = %v", err)
	}
}

func TestRecordCallbacksRunWithoutStoreResources(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	if err := os.Chmod(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	guard := &resourceCheckingGuard{t: t}
	options := testOptions(t)
	options.Guard = guard
	store, err := Open(context.Background(), path, options)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	guard.store.Store(store)
	record := sqliteTestRecord("callback-record", "callback", time.Date(2026, 8, 29, 14, 30, 0, 0, time.UTC))
	mustSQLiteCreate(t, store, record)
	if _, err := store.Get(context.Background(), memory.RecordRef{Scope: record.Scope, ID: record.ID}); err != nil {
		t.Fatal(err)
	}
	if _, err := store.List(context.Background(), memory.ListRequest{Scopes: []memory.Scope{record.Scope}, Limit: 1, IncludeExpired: true}); err != nil {
		t.Fatal(err)
	}
	if guard.recordCalls.Load() < 3 {
		t.Fatalf("record guard calls = %d", guard.recordCalls.Load())
	}
}

func TestRecordGuardRejectsSemanticOpaqueAndExactValuesBeforeSQL(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	if err := os.Chmod(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	options := testOptions(t)
	options.Guard = testGuard(t, testForbidden)
	store, err := Open(context.Background(), path, options)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()

	now := time.Date(2026, 8, 29, 15, 0, 0, 0, time.UTC)
	cases := []struct {
		name   string
		mutate func(*memory.Record)
	}{
		{"redaction", func(record *memory.Record) { record.Text = "contains [REDACTED] marker" }},
		{"credential", func(record *memory.Record) { record.Text = "api_key=synthetic-credential" }},
		{"exact semantic", func(record *memory.Record) { record.Metadata["exact"] = testForbidden }},
		{"exact opaque ID", func(record *memory.Record) { record.ID = testForbidden }},
		{"exact opaque source", func(record *memory.Record) { record.Source.SessionID = testForbidden }},
	}
	for index, testCase := range cases {
		record := sqliteTestRecord("sensitive-"+strconv.Itoa(index), "key-"+strconv.Itoa(index), now)
		testCase.mutate(&record)
		_, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: record})
		assertRecordSafeError(t, err, memory.ErrSensitiveMemory, "[REDACTED]", "synthetic-credential", testForbidden)
	}
	identity, err := store.Identity(context.Background())
	if err != nil || identity.Generation != 0 {
		t.Fatalf("generation = %d, %v", identity.Generation, err)
	}
	external := openExternalSQLite(t, path)
	defer external.Close()
	for _, query := range []string{`SELECT count(*) FROM memory_records`, `SELECT count(*) FROM memory_records_fts`} {
		var count int
		if err := external.QueryRow(query).Scan(&count); err != nil || count != 0 {
			t.Fatalf("%s = %d, %v", query, count, err)
		}
	}
}

func TestIdentityRejectsNoncanonicalGenerationAndEmptyListHonorsClose(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path)
	external := openExternalSQLite(t, path)
	if _, err := external.Exec(`UPDATE memory_meta SET value='01' WHERE key='generation'`); err != nil {
		t.Fatal(err)
	}
	if err := external.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := store.Identity(context.Background()); !errors.Is(err, memory.ErrCorrupt) {
		t.Fatalf("noncanonical generation = %v", err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := store.List(context.Background(), memory.ListRequest{Limit: 1, IncludeExpired: true}); !errors.Is(err, memory.ErrClosed) {
		t.Fatalf("closed empty-scope List = %v", err)
	}
}

func TestRecordCursorRefusalsAndGenerationSnapshot(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
	now := time.Date(2026, 8, 29, 16, 0, 0, 500, time.UTC)
	scope := memory.Scope{Namespace: "user", ID: "cursor-user"}
	for index := range 4 {
		record := sqliteTestRecord("cursor-"+strconv.Itoa(index), "cursor-key-"+strconv.Itoa(index), now.Add(time.Duration(index)*time.Nanosecond))
		record.Scope = scope
		mustSQLiteCreate(t, store, record)
	}
	request := memory.ListRequest{Scopes: []memory.Scope{scope}, Kinds: []string{"note"}, Labels: []string{"alpha"}, Limit: 1, Now: now.Add(time.Hour)}
	first, err := store.List(context.Background(), request)
	if err != nil || first.NextCursor == "" {
		t.Fatalf("first page = %#v, %v", first, err)
	}
	request.Cursor, request.Limit = first.NextCursor, 2 // Limit is excluded from the fingerprint.
	if page, err := store.List(context.Background(), request); err != nil || len(page.Records) != 2 {
		t.Fatalf("limit-cross page = %#v, %v", page, err)
	}

	bad := []struct {
		name   string
		cursor string
		change func(*memory.ListRequest)
	}{
		{"malformed", "not_base64_$secret", nil},
		{"oversized", strings.Repeat("A", memory.MaxCursorBytes+1), nil},
		{"version", mutateCursor(t, first.NextCursor, func(cursor *recordCursor) { cursor.Version++ }), nil},
		{"unknown field", addCursorField(t, first.NextCursor), nil},
		{"cross scope", first.NextCursor, func(request *memory.ListRequest) {
			request.Scopes = []memory.Scope{{Namespace: "user", ID: "other-cursor-user"}}
		}},
		{"cross kind", first.NextCursor, func(request *memory.ListRequest) { request.Kinds = []string{"fact"} }},
		{"cross labels", first.NextCursor, func(request *memory.ListRequest) { request.Labels = []string{"beta"} }},
		{"cross now", first.NextCursor, func(request *memory.ListRequest) { request.Now = request.Now.Add(time.Nanosecond) }},
		{"cross expiry", first.NextCursor, func(request *memory.ListRequest) { request.IncludeExpired = true; request.Now = time.Time{} }},
	}
	for _, testCase := range bad {
		candidate := request
		candidate.Cursor = testCase.cursor
		if testCase.change != nil {
			testCase.change(&candidate)
		}
		_, err := store.List(context.Background(), candidate)
		if !errors.Is(err, memory.ErrInvalidCursor) {
			t.Fatalf("%s error = %v", testCase.name, err)
		}
		if strings.Contains(fmt.Sprint(err), testCase.cursor) {
			t.Fatalf("%s echoed cursor", testCase.name)
		}
	}

	mutation := sqliteTestRecord("cursor-mutation", "cursor-mutation", now.Add(time.Hour))
	mutation.Scope = scope
	mustSQLiteCreate(t, store, mutation)
	stale := request
	stale.Cursor = first.NextCursor
	if _, err := store.List(context.Background(), stale); !errors.Is(err, memory.ErrConflict) {
		t.Fatalf("stale cursor error = %v", err)
	}
}

func TestRecordCommitCancellationUnknownAndOverflowRollback(t *testing.T) {
	t.Run("canceled", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "memory.db")
		store := openTestStore(t, path)
		ctx, cancel := context.WithCancel(context.Background())
		installTestHooks(t, testHooks{beforeCommitCheck: cancel})
		record := sqliteTestRecord("canceled-create", "canceled", time.Date(2026, 8, 29, 17, 0, 0, 0, time.UTC))
		if _, err := store.Upsert(ctx, memory.UpsertRequest{Record: record}); !errors.Is(err, context.Canceled) {
			t.Fatalf("canceled Upsert = %v", err)
		}
		if _, err := store.Get(context.Background(), memory.RecordRef{Scope: record.Scope, ID: record.ID}); !errors.Is(err, memory.ErrNotFound) {
			t.Fatalf("canceled row Get = %v", err)
		}
		identity, _ := store.Identity(context.Background())
		if identity.Generation != 0 {
			t.Fatalf("canceled generation = %d", identity.Generation)
		}
		external := openExternalSQLite(t, path)
		defer external.Close()
		for _, query := range []string{
			`SELECT count(*) FROM memory_records WHERE id='canceled-create'`,
			`SELECT count(*) FROM memory_records_fts WHERE record_id='canceled-create'`,
		} {
			var count int
			if err := external.QueryRow(query).Scan(&count); err != nil || count != 0 {
				t.Fatalf("canceled residue for %s = %d, %v", query, count, err)
			}
		}
	})

	t.Run("post commit response loss", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "memory.db")
		store := openTestStore(t, path)
		installTestHooks(t, testHooks{driverExec: func(statement string, exec func() error) error {
			if statement != "COMMIT" {
				return exec()
			}
			if err := exec(); err != nil {
				return err
			}
			return errors.New("synthetic response loss")
		}})
		record := sqliteTestRecord("unknown-create", "unknown", time.Date(2026, 8, 29, 17, 1, 0, 0, time.UTC))
		_, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: record})
		var unknown *memory.CommitUnknownError
		if !errors.As(err, &unknown) || unknown.Operation() != memory.CommitUpsert || !reflect.DeepEqual(unknown.EntityIDs(), []string{record.ID}) {
			t.Fatalf("commit unknown = %#v, %v", unknown, err)
		}
		if strings.Contains(fmt.Sprint(err), record.Text) || strings.Contains(fmt.Sprint(err), path) {
			t.Fatalf("unsafe unknown error = %v", err)
		}
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		reopened, err := Open(context.Background(), path, testOptions(t))
		if err != nil {
			t.Fatal(err)
		}
		defer reopened.Close()
		got, err := reopened.Get(context.Background(), memory.RecordRef{Scope: record.Scope, ID: record.ID})
		if err != nil || got.ID != record.ID || got.Revision != 1 {
			t.Fatalf("commit reconciliation = %#v, %v", got, err)
		}
	})

	t.Run("generation overflow", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "memory.db")
		store := openTestStore(t, path)
		external := openExternalSQLite(t, path)
		if _, err := external.Exec(`UPDATE memory_meta SET value=? WHERE key='generation'`, strconv.FormatUint(math.MaxUint64, 10)); err != nil {
			t.Fatal(err)
		}
		if err := external.Close(); err != nil {
			t.Fatal(err)
		}
		record := sqliteTestRecord("overflow-create", "overflow", time.Date(2026, 8, 29, 17, 2, 0, 0, time.UTC))
		if _, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: record}); !errors.Is(err, memory.ErrCorrupt) {
			t.Fatalf("overflow Upsert = %v", err)
		}
		external = openExternalSQLite(t, path)
		defer external.Close()
		for _, query := range []string{
			`SELECT count(*) FROM memory_records WHERE id='overflow-create'`,
			`SELECT count(*) FROM memory_records_fts WHERE record_id='overflow-create'`,
		} {
			var count int
			if err := external.QueryRow(query).Scan(&count); err != nil || count != 0 {
				t.Fatalf("overflow residue for %s = %d, %v", query, count, err)
			}
		}
	})
}

func TestRecordEncodingFTSConflictAndCallerIDs(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	if err := os.Chmod(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	ids := []string{testDatabaseID, testUserID}
	var calls atomic.Int64
	options := Options{Guard: testGuard(t), NewID: func() (string, error) {
		index := calls.Add(1) - 1
		if index >= int64(len(ids)) {
			return "", errors.New("record ID must be caller supplied")
		}
		return ids[index], nil
	}}
	store, err := Open(context.Background(), path, options)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	record := sqliteTestRecord("caller-record", "unique-key", time.Date(2026, 8, 29, 18, 0, 0, 999999999, time.UTC))
	record.Labels = []string{"zeta", "alpha", "middle"}
	record.Metadata = map[string]string{"z": "last", "a": "first", "m": "middle"}
	mustSQLiteCreate(t, store, record)
	if calls.Load() != 2 {
		t.Fatalf("NewID calls after record create = %d", calls.Load())
	}

	external := openExternalSQLite(t, path)
	var labelsJSON, metadataJSON, fts string
	if err := external.QueryRow(`SELECT labels_json,metadata_json FROM memory_records WHERE id=?`, record.ID).Scan(&labelsJSON, &metadataJSON); err != nil {
		t.Fatal(err)
	}
	if err := external.QueryRow(`SELECT labels FROM memory_records_fts WHERE record_id=?`, record.ID).Scan(&fts); err != nil {
		t.Fatal(err)
	}
	if labelsJSON != `["zeta","alpha","middle"]` || metadataJSON != `{"a":"first","m":"middle","z":"last"}` || fts != "alpha\nmiddle\nzeta" {
		t.Fatalf("encoding labels/metadata/fts = %q / %q / %q", labelsJSON, metadataJSON, fts)
	}
	if err := external.Close(); err != nil {
		t.Fatal(err)
	}

	duplicate := sqliteTestRecord("conflicting-record", record.Key, record.UpdatedAt.Add(time.Nanosecond))
	if _, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: duplicate}); !errors.Is(err, memory.ErrConflict) {
		t.Fatalf("duplicate error = %v", err)
	}
	identity, err := store.Identity(context.Background())
	if err != nil || identity.Generation != 1 {
		t.Fatalf("conflict generation = %d, %v", identity.Generation, err)
	}
	external = openExternalSQLite(t, path)
	defer external.Close()
	for _, query := range []string{
		`SELECT count(*) FROM memory_records WHERE id='conflicting-record'`,
		`SELECT count(*) FROM memory_records_fts WHERE record_id='conflicting-record'`,
	} {
		var count int
		if err := external.QueryRow(query).Scan(&count); err != nil || count != 0 {
			t.Fatalf("conflict changed DB for %s = %d, %v", query, count, err)
		}
	}
}

type resourceCheckingGuard struct {
	t           *testing.T
	store       atomic.Pointer[Store]
	recordCalls atomic.Int64
}

func (guard *resourceCheckingGuard) Check(ctx context.Context, input memory.GuardInput) error {
	isRecord := false
	for _, field := range input.Fields {
		if strings.HasPrefix(field.Name, "record ") {
			isRecord = true
			break
		}
	}
	if isRecord {
		guard.recordCalls.Add(1)
		store := guard.store.Load()
		if store != nil {
			store.lifecycleMu.Lock()
			active := store.active
			store.lifecycleMu.Unlock()
			if active != 0 || len(store.writeGate) != 1 || len(store.connections) != retainedConnectionCount {
				guard.t.Errorf("guard held resources: active=%d write=%d connections=%d", active, len(store.writeGate), len(store.connections))
			}
			if _, err := store.Identity(ctx); err != nil {
				guard.t.Errorf("guard reentrant Identity: %v", err)
			}
		}
	}
	return (memory.DefaultGuard{}).Check(ctx, input)
}

type rawRecord struct {
	id, namespace, scopeID, kind, key, text, labels, metadata, source string
	confidence                                                        float64
	revision                                                          int64
	created, updated                                                  string
	expires                                                           any
	state, forgotten                                                  any
}

func makeRawRecord(t *testing.T, record memory.Record) rawRecord {
	t.Helper()
	record.Revision = 1
	encoded, err := encodeRecord(record)
	if err != nil {
		t.Fatal(err)
	}
	return rawRecord{
		id: record.ID, namespace: record.Scope.Namespace, scopeID: record.Scope.ID, kind: record.Kind, key: record.Key,
		text: record.Text, labels: string(encoded.labels), metadata: string(encoded.metadata), source: string(encoded.source),
		confidence: record.Confidence, revision: 1, created: encoded.created, updated: encoded.updated,
		expires: encoded.expires, state: "active", forgotten: nil,
	}
}

func insertRawRecord(t *testing.T, database *sql.DB, row rawRecord) {
	t.Helper()
	_, err := database.Exec(`INSERT INTO memory_records(
		id,scope_namespace,scope_id,kind,semantic_key,text_value,labels_json,metadata_json,source_json,
		confidence,revision,created_at,updated_at,expires_at,state,forgotten_at
	) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
		row.id, row.namespace, row.scopeID, row.kind, row.key, row.text, row.labels, row.metadata, row.source,
		row.confidence, row.revision, row.created, row.updated, row.expires, row.state, row.forgotten,
	)
	if err != nil {
		t.Fatal(err)
	}
}

func openExternalSQLite(t *testing.T, path string) *sql.DB {
	t.Helper()
	database, err := sql.Open(driverName, path)
	if err != nil {
		t.Fatal(err)
	}
	database.SetMaxOpenConns(1)
	if _, err := database.Exec(`PRAGMA ignore_check_constraints=ON`); err != nil {
		_ = database.Close()
		t.Fatal(err)
	}
	return database
}

func sqliteTestRecord(id, key string, at time.Time) memory.Record {
	decision := at
	expires := at.Add(24 * time.Hour)
	return memory.Record{
		ID: id, Scope: memory.Scope{Namespace: "user", ID: "sqlite-record-user"}, Kind: "note", Key: key,
		Text: "SQLite persistent record", Labels: []string{"alpha", "beta"}, Metadata: map[string]string{"a": "one", "b": "two"},
		Source:     memory.Provenance{Origin: memory.OriginModel, SessionID: "session-sqlite", MessageIDs: []string{"message-sqlite"}, ObservationID: "observation-sqlite", DecisionAt: &decision, DecisionSource: memory.OriginHuman},
		Confidence: 0.625, CreatedAt: at, UpdatedAt: at, ExpiresAt: &expires,
	}
}

func mustSQLiteCreate(t *testing.T, store *Store, record memory.Record) memory.Record {
	t.Helper()
	created, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: record})
	if err != nil {
		t.Fatal(err)
	}
	return created
}

func mutateCursor(t *testing.T, value string, mutate func(*recordCursor)) string {
	t.Helper()
	raw, err := base64.RawURLEncoding.DecodeString(value)
	if err != nil {
		t.Fatal(err)
	}
	var cursor recordCursor
	if err := json.Unmarshal(raw, &cursor); err != nil {
		t.Fatal(err)
	}
	mutate(&cursor)
	raw, err = json.Marshal(cursor)
	if err != nil {
		t.Fatal(err)
	}
	return base64.RawURLEncoding.EncodeToString(raw)
}

func addCursorField(t *testing.T, value string) string {
	t.Helper()
	raw, err := base64.RawURLEncoding.DecodeString(value)
	if err != nil {
		t.Fatal(err)
	}
	var fields map[string]any
	if err := json.Unmarshal(raw, &fields); err != nil {
		t.Fatal(err)
	}
	fields["unknown"] = true
	raw, err = json.Marshal(fields)
	if err != nil {
		t.Fatal(err)
	}
	return base64.RawURLEncoding.EncodeToString(raw)
}

func assertRecordSafeError(t *testing.T, err, category error, secrets ...string) {
	t.Helper()
	if !errors.Is(err, category) {
		t.Fatalf("error = %v, want %v", err, category)
	}
	for _, secret := range secrets {
		if secret != "" && strings.Contains(fmt.Sprint(err), secret) {
			t.Fatalf("safe error contains secret %q: %v", secret, err)
		}
	}
}
