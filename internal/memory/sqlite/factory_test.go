package sqlite

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

var _ memory.Factory = Factory{}

func TestFactoryComponentsCapabilitiesAndMaintenance(t *testing.T) {
	directory := t.TempDir()
	if err := os.Chmod(directory, 0o700); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(directory, "memory.db")
	options := testOptions(t)
	factory := NewFactory(path, options)
	// NewFactory must retain its own value copy.
	options.Guard = nil
	options.BusyTimeout = -time.Second

	components, err := factory.Open(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if components.Store == nil || components.Retriever == nil || components.Maintenance == nil {
		t.Fatalf("incomplete components: %#v", components)
	}
	wantCapabilities := memory.Capabilities{
		LexicalSearch:       true,
		SemanticSearch:      false,
		OnlineBackup:        false,
		EncryptionAtRest:    false,
		ConcurrentProcesses: false,
	}
	if components.Capabilities != wantCapabilities {
		t.Fatalf("capabilities = %#v, want %#v", components.Capabilities, wantCapabilities)
	}
	if _, err := components.Store.Identity(context.Background()); err != nil {
		t.Fatalf("Identity: %v", err)
	}

	maintenanceCalls := []struct {
		name string
		call func(context.Context) error
	}{
		{"Backup", func(ctx context.Context) error {
			_, err := components.Maintenance.Backup(ctx, memory.BackupRequest{Class: "opaque"})
			return err
		}},
		{"ListBackups", func(ctx context.Context) error { _, err := components.Maintenance.ListBackups(ctx); return err }},
		{"VerifyBackup", func(ctx context.Context) error {
			_, err := components.Maintenance.VerifyBackup(ctx, "opaque-id")
			return err
		}},
		{"Restore", func(ctx context.Context) error {
			return components.Maintenance.Restore(ctx, memory.RestoreRequest{Backup: "opaque-id"})
		}},
		{"PurgeForgotten", func(ctx context.Context) error {
			_, err := components.Maintenance.PurgeForgotten(ctx, memory.PurgeForgottenRequest{})
			return err
		}},
	}
	for _, test := range maintenanceCalls {
		t.Run(test.name, func(t *testing.T) {
			if err := test.call(context.Background()); err != memory.ErrUnsupported {
				t.Fatalf("error = %v, want exact ErrUnsupported", err)
			}
			canceled, cancel := context.WithCancel(context.Background())
			cancel()
			if err := test.call(canceled); !errors.Is(err, context.Canceled) {
				t.Fatalf("canceled error = %v", err)
			}
		})
	}

	if err := components.Store.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := components.Store.Identity(context.Background()); !errors.Is(err, memory.ErrClosed) {
		t.Fatalf("Store after Close = %v", err)
	}
	if _, err := components.Retriever.Retrieve(context.Background(), memory.RetrievalRequest{}); !errors.Is(err, memory.ErrClosed) {
		t.Fatalf("Retriever after Close = %v", err)
	}
	if err := maintenanceCalls[0].call(context.Background()); err != memory.ErrUnsupported {
		t.Fatalf("Maintenance after Close = %v", err)
	}
}

func TestFactoryAndDirectOpenRejectCorruption(t *testing.T) {
	mutations := []struct {
		name   string
		mutate func(*testing.T, string, memory.Record)
	}{
		{"missing FTS", func(t *testing.T, path string, record memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`DELETE FROM memory_records_fts WHERE record_id=?`, record.ID); err != nil {
				t.Fatal(err)
			}
		}},
		{"duplicate FTS", func(t *testing.T, path string, record memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES(?,?,?,?,?)`, record.ID, record.Text, record.Kind, record.Key, "alpha beta"); err != nil {
				t.Fatal(err)
			}
		}},
		{"orphan FTS", func(t *testing.T, path string, _ memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES('orphan-record','opaque','note','orphan','alpha')`); err != nil {
				t.Fatal(err)
			}
		}},
		{"stale FTS id", func(t *testing.T, path string, record memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`UPDATE memory_records_fts SET record_id='stale-index-id' WHERE record_id=?`, record.ID); err != nil {
				t.Fatal(err)
			}
		}},
		{"stale FTS text", func(t *testing.T, path string, record memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`UPDATE memory_records_fts SET text_value='stale-index-value' WHERE record_id=?`, record.ID); err != nil {
				t.Fatal(err)
			}
		}},
		{"stale FTS kind", func(t *testing.T, path string, record memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`UPDATE memory_records_fts SET kind='stale-kind' WHERE record_id=?`, record.ID); err != nil {
				t.Fatal(err)
			}
		}},
		{"stale FTS key", func(t *testing.T, path string, record memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`UPDATE memory_records_fts SET semantic_key='stale-key' WHERE record_id=?`, record.ID); err != nil {
				t.Fatal(err)
			}
		}},
		{"stale FTS labels", func(t *testing.T, path string, record memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`UPDATE memory_records_fts SET labels='stale-labels' WHERE record_id=?`, record.ID); err != nil {
				t.Fatal(err)
			}
		}},
		{"record domain", func(t *testing.T, path string, record memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`UPDATE memory_records SET source_json='{}' WHERE id=?`, record.ID); err != nil {
				t.Fatal(err)
			}
		}},
	}

	for index, test := range mutations {
		t.Run(test.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "memory.db")
			store := openTestStore(t, path)
			record := sqliteTestRecord(fmt.Sprintf("integrity-%d", index), fmt.Sprintf("integrity-key-%d", index), time.Date(2026, 8, 29, 23, 0, index, 0, time.UTC))
			mustSQLiteCreate(t, store, record)
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			test.mutate(t, path, record)

			var reopened *Store
			var err error
			if index%2 == 0 {
				reopened, err = Open(context.Background(), path, testOptions(t))
			} else {
				components, openErr := NewFactory(path, testOptions(t)).Open(context.Background())
				err = openErr
				if components.Store != nil {
					reopened = components.Store.(*Store)
				}
			}
			if reopened != nil {
				_ = reopened.Close()
			}
			if !errors.Is(err, memory.ErrCorrupt) {
				t.Fatalf("Open error = %v, want ErrCorrupt", err)
			}
		})
	}
}

func TestFactoryRejectsObservationCorrespondenceCorruption(t *testing.T) {
	mutations := []struct {
		name string
		sql  string
	}{
		{"receipt missing candidate", `UPDATE memory_observations SET candidate_ids_json='[]' WHERE id='integrity-observation'`},
		{"duplicate receipt candidate", `UPDATE memory_observations SET candidate_ids_json='["integrity-candidate","integrity-candidate"]' WHERE id='integrity-observation'`},
		{"candidate missing receipt reference", `UPDATE memory_candidates SET observation_id=NULL WHERE id='integrity-candidate'`},
	}
	for _, test := range mutations {
		t.Run(test.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "memory.db")
			store := openTestStore(t, path)
			at := time.Date(2026, 8, 29, 23, 5, 0, 0, time.UTC)
			candidate := sqlitePendingCandidate("integrity-candidate", memory.Scope{Namespace: "user", ID: "integrity-observation-scope"}, at)
			candidate.Proposed.Source.ObservationID = "integrity-observation"
			if _, err := store.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: "integrity-observation", Candidates: []memory.Candidate{candidate}, CreatedAt: at}); err != nil {
				t.Fatal(err)
			}
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			db := openExternalSQLite(t, path)
			if _, err := db.Exec(test.sql); err != nil {
				t.Fatal(err)
			}
			if err := db.Close(); err != nil {
				t.Fatal(err)
			}
			if reopened, err := Open(context.Background(), path, testOptions(t)); !errors.Is(err, memory.ErrCorrupt) {
				if reopened != nil {
					_ = reopened.Close()
				}
				t.Fatalf("Open error = %v, want ErrCorrupt", err)
			}
		})
	}
}

func TestFactoryRejectsMalformedPersistedDomains(t *testing.T) {
	mutations := []struct {
		name, statement, retained string
	}{
		{"tombstone timestamp", `UPDATE memory_records SET created_at='2026-99-99T99:99:99.000000000Z' WHERE id='domain-tombstone'`, `SELECT count(*) FROM memory_records WHERE id='domain-tombstone' AND created_at='2026-99-99T99:99:99.000000000Z'`},
		{"tombstone canonical fields", `UPDATE memory_records SET text_value='not-cleared' WHERE id='domain-tombstone'`, `SELECT count(*) FROM memory_records WHERE id='domain-tombstone' AND text_value='not-cleared'`},
		{"candidate base revision", `UPDATE memory_candidates SET base_revision=-1 WHERE id='domain-candidate'`, `SELECT count(*) FROM memory_candidates WHERE id='domain-candidate' AND base_revision=-1`},
		{"candidate proposed JSON", `UPDATE memory_candidates SET proposed_json='{}' WHERE id='domain-candidate'`, `SELECT count(*) FROM memory_candidates WHERE id='domain-candidate' AND proposed_json='{}'`},
		{"receipt timestamp", `UPDATE memory_observations SET created_at='2026-99-99T99:99:99.000000000Z' WHERE id='domain-observation'`, `SELECT count(*) FROM memory_observations WHERE id='domain-observation' AND created_at='2026-99-99T99:99:99.000000000Z'`},
		{"receipt IDs JSON", `UPDATE memory_observations SET candidate_ids_json='[1]' WHERE id='domain-observation'`, `SELECT count(*) FROM memory_observations WHERE id='domain-observation' AND candidate_ids_json='[1]'`},
		{"canonical generation", `UPDATE memory_meta SET value='00' WHERE key='generation'`, `SELECT count(*) FROM memory_meta WHERE key='generation' AND value='00'`},
	}
	for _, test := range mutations {
		t.Run(test.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "memory.db")
			store := openTestStore(t, path)
			at := time.Date(2026, 8, 30, 0, 10, 0, 0, time.UTC)
			record := sqliteTestRecord("domain-tombstone", "domain-tombstone-key", at)
			created := mustSQLiteCreate(t, store, record)
			if _, err := store.Forget(context.Background(), memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: created.Scope, ID: created.ID}, ExpectedRevision: 1, ForgottenAt: at.Add(time.Second)}); err != nil {
				t.Fatal(err)
			}
			candidate := sqlitePendingCandidate("domain-candidate", memory.Scope{Namespace: "user", ID: "domain-scope"}, at.Add(2*time.Second))
			if _, err := store.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{candidate}}); err != nil {
				t.Fatal(err)
			}
			observed := sqlitePendingCandidate("domain-observed-candidate", candidate.Proposed.Scope, at.Add(3*time.Second))
			observed.Proposed.Source.ObservationID = "domain-observation"
			if _, err := store.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: "domain-observation", Candidates: []memory.Candidate{observed}, CreatedAt: at.Add(3 * time.Second)}); err != nil {
				t.Fatal(err)
			}
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			db := openExternalSQLite(t, path)
			if _, err := db.Exec(`PRAGMA ignore_check_constraints=ON`); err != nil {
				t.Fatal(err)
			}
			if _, err := db.Exec(test.statement); err != nil {
				t.Fatal(err)
			}
			if err := db.Close(); err != nil {
				t.Fatal(err)
			}
			reopened, err := Open(context.Background(), path, testOptions(t))
			if reopened != nil {
				_ = reopened.Close()
			}
			if !errors.Is(err, memory.ErrCorrupt) {
				t.Fatalf("Open error = %v, want ErrCorrupt", err)
			}
			db = openExternalSQLite(t, path)
			defer db.Close()
			var retained int
			if err := db.QueryRow(test.retained).Scan(&retained); err != nil || retained != 1 {
				t.Fatalf("failed Open repaired logical fault: retained=%d err=%v", retained, err)
			}
		})
	}
}

func TestFactoryQuickCheckRejectsTargetedBTreeCorruption(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path)
	record := sqliteTestRecord("quick-check-record", "quick-check-key", time.Date(2026, 8, 30, 0, 15, 0, 0, time.UTC))
	mustSQLiteCreate(t, store, record)
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	db := openExternalSQLite(t, path)
	if _, err := db.Exec(`PRAGMA wal_checkpoint(TRUNCATE)`); err != nil {
		t.Fatal(err)
	}
	var page, pageSize, cells int64
	if err := db.QueryRow(`SELECT pageno,pgsize,ncell FROM dbstat WHERE name='memory_records' AND pagetype='leaf' AND ncell>0 LIMIT 1`).Scan(&page, &pageSize, &cells); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	if page <= 1 || pageSize <= 512 || cells < 1 {
		t.Fatalf("unexpected target page metadata: page=%d size=%d cells=%d", page, pageSize, cells)
	}
	file, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		t.Fatal(err)
	}
	// A leaf-table page's first cell pointer is at header offset 8. Pointing it
	// outside the usable page is a targeted b-tree fault, not random truncation.
	if _, err := file.WriteAt([]byte{0, 0}, (page-1)*pageSize+8); err != nil {
		_ = file.Close()
		t.Fatal(err)
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}
	probe := openExternalSQLite(t, path)
	var quickResult string
	probeErr := probe.QueryRow(`PRAGMA quick_check(1)`).Scan(&quickResult)
	_ = probe.Close()
	if probeErr == nil && quickResult == "ok" {
		t.Fatal("targeted page mutation did not corrupt quick_check")
	}
	var quickCalls, ftsCalls int
	installTestHooks(t, testHooks{
		beforeQuickCheck:    func() { quickCalls++ },
		beforeFTS5Integrity: func(*sql.Conn) { ftsCalls++ },
	})
	reopened, err := Open(context.Background(), path, testOptions(t))
	if reopened != nil {
		_ = reopened.Close()
	}
	if !errors.Is(err, memory.ErrCorrupt) || quickCalls != 1 || ftsCalls != 0 {
		t.Fatalf("quick-check reopen=%v quick=%d fts=%d", err, quickCalls, ftsCalls)
	}
}

func TestFactoryFTS5IntegrityCommandRejectsShadowIndexCorruption(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path)
	record := sqliteTestRecord("fts-integrity-record", "fts-integrity-key", time.Date(2026, 8, 30, 0, 20, 0, 0, time.UTC))
	mustSQLiteCreate(t, store, record)
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	corruptor := openExternalSQLite(t, path)
	defer corruptor.Close()
	var integrityCalls int
	installTestHooks(t, testHooks{beforeFTS5Integrity: func(*sql.Conn) {
		integrityCalls++
		result, err := corruptor.Exec(`UPDATE memory_records_fts_docsize SET sz=x'00'
			WHERE id=(SELECT id FROM memory_records_fts_docsize LIMIT 1)`)
		if err != nil {
			t.Errorf("corrupt FTS shadow index: %v", err)
			return
		}
		if changed, _ := result.RowsAffected(); changed != 1 {
			t.Errorf("corrupted FTS index rows = %d", changed)
		}
	}})
	reopened, err := Open(context.Background(), path, testOptions(t))
	if reopened != nil {
		_ = reopened.Close()
	}
	if !errors.Is(err, memory.ErrCorrupt) || integrityCalls != 1 {
		t.Fatalf("FTS5 integrity reopen = %v, command calls=%d", err, integrityCalls)
	}
	var retained []byte
	if err := corruptor.QueryRow(`SELECT sz FROM memory_records_fts_docsize LIMIT 1`).Scan(&retained); err != nil || !reflect.DeepEqual(retained, []byte{0}) {
		t.Fatalf("failed FTS integrity Open repaired shadow fault: %x, %v", retained, err)
	}
}

func TestFactoryReceiptIDsMayOutliveCandidates(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path)
	at := time.Date(2026, 8, 29, 23, 10, 0, 0, time.UTC)
	candidate := sqlitePendingCandidate("retained-receipt-candidate", memory.Scope{Namespace: "user", ID: "retained-receipt-scope"}, at)
	candidate.Proposed.Source.ObservationID = "retained-receipt"
	if _, err := store.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: "retained-receipt", Candidates: []memory.Candidate{candidate}, CreatedAt: at}); err != nil {
		t.Fatal(err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	db := openExternalSQLite(t, path)
	if _, err := db.Exec(`DELETE FROM memory_candidates WHERE id=?`, candidate.ID); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, err := Open(context.Background(), path, testOptions(t))
	if err != nil {
		t.Fatalf("receipt surviving removed candidate: %v", err)
	}
	defer reopened.Close()
	receipt, err := reopened.GetObservationReceipt(context.Background(), "retained-receipt")
	if err != nil || !reflect.DeepEqual(receipt.CandidateIDs, []string{candidate.ID}) {
		t.Fatalf("receipt = %#v, %v", receipt, err)
	}
}

func TestOpenAllowsGenerationAdvanceBetweenIdentityChecks(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	left := openTestStore(t, path)
	at := time.Date(2026, 8, 30, 0, 0, 0, 0, time.UTC)
	first := sqliteTestRecord("open-generation-first", "open-generation-key-first", at)
	second := sqliteTestRecord("open-generation-second", "open-generation-key-second", at.Add(time.Second))
	var hookErr error
	var preflightOnce, retainedOnce bool
	installTestHooks(t, testHooks{
		beforePreflightRecheck: func() {
			if preflightOnce {
				return
			}
			preflightOnce = true
			_, hookErr = left.Upsert(context.Background(), memory.UpsertRequest{Record: first})
		},
		path: func(event pathEvent) {
			if event != pathAfterRetainedConnection2DriverOpen || retainedOnce || hookErr != nil {
				return
			}
			retainedOnce = true
			_, hookErr = left.Upsert(context.Background(), memory.UpsertRequest{Record: second})
		},
	})
	right, err := Open(context.Background(), path, testOptions(t))
	if err != nil {
		t.Fatalf("Open across healthy generation advances = %v", err)
	}
	defer right.Close()
	if hookErr != nil || !preflightOnce || !retainedOnce {
		t.Fatalf("deterministic mutations: preflight=%v retained=%v err=%v", preflightOnce, retainedOnce, hookErr)
	}
	identity, err := right.Identity(context.Background())
	if err != nil || identity.Generation != 2 || identity.DatabaseID != testDatabaseID || identity.UserScope.ID != testUserID || identity.SchemaVersion != schemaVersion {
		t.Fatalf("healthy identity = %#v, %v", identity, err)
	}
	for _, record := range []memory.Record{first, second} {
		got, err := right.Get(context.Background(), memory.RecordRef{Scope: record.Scope, ID: record.ID})
		if err != nil || got.Text != record.Text {
			t.Fatalf("healthy data %s = %#v, %v", record.ID, got, err)
		}
	}
}

func TestFactoryRejectsNoncanonicalFTSShadowSQL(t *testing.T) {
	mutations := map[string]string{
		"memory_records_fts_config":  `CREATE TABLE 'memory_records_fts_config'(k PRIMARY KEY, v)`,
		"memory_records_fts_content": `CREATE TABLE 'memory_records_fts_content'(id PRIMARY KEY, c0, c1, c2, c3, c4)`,
		"memory_records_fts_data":    `CREATE TABLE 'memory_records_fts_data'(id INTEGER PRIMARY KEY, block TEXT)`,
		"memory_records_fts_docsize": `CREATE TABLE 'memory_records_fts_docsize'(id INTEGER PRIMARY KEY, sz TEXT)`,
		"memory_records_fts_idx":     `CREATE TABLE 'memory_records_fts_idx'(segid, term, pgno, PRIMARY KEY(segid, term))`,
	}
	if len(mutations) != len(expectedFTSShadowTables) {
		t.Fatalf("shadow SQL mutation coverage = %d, want %d", len(mutations), len(expectedFTSShadowTables))
	}
	for table := range expectedFTSShadowTables {
		if _, ok := mutations[table]; !ok {
			t.Fatalf("missing shadow SQL mutation for %s", table)
		}
	}
	for table, mutated := range mutations {
		t.Run(table, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "memory.db")
			store := openTestStore(t, path)
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			db := openExternalSQLite(t, path)
			if _, err := db.Exec(`PRAGMA writable_schema=ON`); err != nil {
				t.Fatal(err)
			}
			if result, err := db.Exec(`UPDATE sqlite_schema SET sql=? WHERE type='table' AND name=?`, mutated, table); err != nil {
				t.Fatal(err)
			} else if changed, _ := result.RowsAffected(); changed != 1 {
				t.Fatalf("schema rows changed = %d", changed)
			}
			if _, err := db.Exec(`PRAGMA writable_schema=OFF`); err != nil {
				t.Fatal(err)
			}
			if err := db.Close(); err != nil {
				t.Fatal(err)
			}
			reopened, err := Open(context.Background(), path, testOptions(t))
			if reopened != nil {
				_ = reopened.Close()
			}
			if !errors.Is(err, memory.ErrCorrupt) {
				t.Fatalf("Open error = %v, want ErrCorrupt", err)
			}
			db = openExternalSQLite(t, path)
			defer db.Close()
			var retained string
			if err := db.QueryRow(`SELECT sql FROM sqlite_schema WHERE name=?`, table).Scan(&retained); err != nil || retained != mutated {
				t.Fatalf("failed Open repaired shadow SQL: got %q, err %v", retained, err)
			}
		})
	}
}

type capturingGuard struct {
	base    memory.ContentGuard
	capture bool
	ctx     context.Context
}

func (guard *capturingGuard) Check(ctx context.Context, input memory.GuardInput) error {
	if guard.capture {
		guard.ctx = ctx
	}
	return guard.base.Check(ctx, input)
}

func TestCapturedCallbackAdmissionExpires(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	guard := &capturingGuard{base: testGuard(t)}
	store := openTestStore(t, path, func(options *Options) { options.Guard = guard })
	record := sqliteTestRecord("captured-context-record", "captured-context-key", time.Date(2026, 8, 30, 0, 5, 0, 0, time.UTC))
	created := mustSQLiteCreate(t, store, record)
	guard.capture = true
	if _, err := store.Get(context.Background(), memory.RecordRef{Scope: created.Scope, ID: created.ID}); err != nil {
		t.Fatal(err)
	}
	captured := guard.ctx
	if captured == nil {
		t.Fatal("guard did not capture operation context")
	}
	// Admission precedes validation: an expired callback capability cannot be
	// reused as a fresh operation even while the Store otherwise remains open.
	if _, err := store.Get(captured, memory.RecordRef{}); err != memory.ErrClosed {
		t.Fatalf("captured callback context after outer completion = %v, want exact ErrClosed", err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := store.Get(captured, memory.RecordRef{}); err != memory.ErrClosed {
		t.Fatalf("captured callback context after Close = %v, want exact ErrClosed", err)
	}
}

type panicOnFinalOpenGuard struct {
	base      memory.ContentGuard
	panicNext bool
}

func (guard *panicOnFinalOpenGuard) Check(ctx context.Context, input memory.GuardInput) error {
	if guard.panicNext {
		panic("synthetic final identity guard panic")
	}
	return guard.base.Check(ctx, input)
}

func TestOpenFinalGuardPanicCleansAllResources(t *testing.T) {
	directory := t.TempDir()
	if err := os.Chmod(directory, 0o700); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(directory, "memory.db")
	guard := &panicOnFinalOpenGuard{base: testGuard(t)}
	var ready *Store
	installTestHooks(t, testHooks{storeReady: func(store *Store) {
		ready = store
		guard.panicNext = true
	}})
	var recovered any
	var openErr error
	func() {
		defer func() { recovered = recover() }()
		_, openErr = Open(context.Background(), path, Options{Guard: guard, NewID: testOptions(t).NewID})
	}()
	if recovered == nil {
		t.Fatalf("Open did not propagate final guard panic: err=%v ready=%v armed=%v", openErr, ready != nil, guard.panicNext)
	}
	if ready == nil {
		t.Fatal("Store did not reach final guard")
	}
	ready.lifecycleMu.Lock()
	state, active := ready.state, ready.active
	ready.lifecycleMu.Unlock()
	if state != storeClosed || active != 0 || ready.quarantined.Load() != 0 {
		t.Fatalf("panic cleanup state=%v active=%d quarantined=%d", state, active, ready.quarantined.Load())
	}
	if got := processFDCountForPath(t, path); got != 0 {
		t.Fatalf("database descriptors after recovered panic = %d", got)
	}
	reopened, err := Open(context.Background(), path, testOptions(t))
	if err != nil {
		t.Fatalf("reopen after recovered panic = %v", err)
	}
	if err := reopened.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestFactoryOpenValidation(t *testing.T) {
	for name, factory := range map[string]Factory{
		"empty path":    NewFactory("", testOptions(t)),
		"empty options": NewFactory(filepath.Join(t.TempDir(), "memory.db"), Options{}),
	} {
		t.Run(name, func(t *testing.T) {
			components, err := factory.Open(context.Background())
			if !errors.Is(err, memory.ErrInvalidRequest) {
				t.Fatalf("error = %v", err)
			}
			if !reflect.DeepEqual(components, memory.Components{}) {
				t.Fatalf("components on failure = %#v", components)
			}
		})
	}
}
