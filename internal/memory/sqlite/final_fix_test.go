//go:build darwin || linux

package sqlite

import (
	"context"
	"database/sql"
	"database/sql/driver"
	"errors"
	"math"
	"path/filepath"
	"reflect"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

func TestOpenSynchronizesNewPathBeforeDriverUse(t *testing.T) {
	path := filepath.Join(t.TempDir(), "first", "second", "memory.db")
	var mu sync.Mutex
	var events []string
	appendEvent := func(event string) {
		mu.Lock()
		events = append(events, event)
		mu.Unlock()
	}
	installTestHooks(t, testHooks{
		fsync: func(resource string, sync func() error) error {
			appendEvent(resource)
			return sync()
		},
		beforeDirectoryInstall: func(string) { appendEvent("directory-install") },
		path: func(event pathEvent) {
			if event == pathBeforePreflightDriverOpen {
				appendEvent("driver-open")
			}
		},
	})
	store := openTestStore(t, path)
	_ = store
	want := []string{
		"ancestor-parent",
		"prepared-directory", "directory-install", "directory-parent",
		"prepared-directory", "directory-install", "directory-parent",
		"database-file", "database-parent", "driver-open",
	}
	mu.Lock()
	got := append([]string(nil), events...)
	mu.Unlock()
	if len(got) < len(want) {
		t.Fatalf("durability order = %v, want prefix %v", got, want)
	}
	if !reflect.DeepEqual(got[:len(want)], want) {
		t.Fatalf("durability order = %v, want prefix %v", got, want)
	}
}

func TestOpenRepairsFailedDirectoryInstallSyncOnRetry(t *testing.T) {
	t.Run("repair precedes successful retry", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "new", "memory.db")
		var mu sync.Mutex
		var events []string
		failedInstall := false
		installTestHooks(t, testHooks{fsync: func(resource string, syncDescriptor func() error) error {
			mu.Lock()
			events = append(events, resource)
			shouldFail := resource == "directory-parent" && !failedInstall
			if shouldFail {
				failedInstall = true
			}
			mu.Unlock()
			if shouldFail {
				return errors.New("injected install sync failure")
			}
			return syncDescriptor()
		}})
		store, err := Open(context.Background(), path, testOptions(t))
		if store != nil {
			_ = store.Close()
		}
		if !errors.Is(err, memory.ErrUnavailable) || !failedInstall {
			t.Fatalf("first Open = %v, failedInstall=%v", err, failedInstall)
		}

		mu.Lock()
		events = nil
		mu.Unlock()
		store, err = Open(context.Background(), path, testOptions(t))
		if err != nil {
			t.Fatalf("retry Open = %v", err)
		}
		defer store.Close()
		mu.Lock()
		got := append([]string(nil), events...)
		mu.Unlock()
		wantPrefix := []string{"ancestor-parent", "database-file", "database-parent"}
		if len(got) < len(wantPrefix) || !reflect.DeepEqual(got[:len(wantPrefix)], wantPrefix) {
			t.Fatalf("retry durability order = %v, want prefix %v", got, wantPrefix)
		}
	})

	t.Run("persistent repair failure remains unavailable", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "new", "memory.db")
		failedInstall := false
		failRepair := false
		installTestHooks(t, testHooks{fsync: func(resource string, syncDescriptor func() error) error {
			if resource == "directory-parent" && !failedInstall {
				failedInstall = true
				return errors.New("injected install sync failure")
			}
			if resource == "ancestor-parent" && failRepair {
				return errors.New("injected repair sync failure")
			}
			return syncDescriptor()
		}})
		store, err := Open(context.Background(), path, testOptions(t))
		if store != nil {
			_ = store.Close()
		}
		if !errors.Is(err, memory.ErrUnavailable) || !failedInstall {
			t.Fatalf("first Open = %v, failedInstall=%v", err, failedInstall)
		}
		failRepair = true
		store, err = Open(context.Background(), path, testOptions(t))
		if store != nil {
			_ = store.Close()
		}
		assertSafeError(t, err, memory.ErrUnavailable, path, "injected")
	})
}

func TestOpenFsyncFailuresFailClosed(t *testing.T) {
	for _, resource := range []string{"prepared-directory", "directory-parent", "database-file", "database-parent"} {
		t.Run(resource, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "new", "memory.db")
			failed := false
			installTestHooks(t, testHooks{fsync: func(got string, sync func() error) error {
				if got == resource && !failed {
					failed = true
					return errors.New("injected fsync failure")
				}
				return sync()
			}})
			store, err := Open(context.Background(), path, testOptions(t))
			if store != nil {
				_ = store.Close()
			}
			assertSafeError(t, err, memory.ErrUnavailable, path, "injected")
			if !failed {
				t.Fatalf("%s seam was not reached", resource)
			}
		})
	}
}

func TestBootstrapCancellationCommitPoint(t *testing.T) {
	t.Run("immediately before commit rolls back", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "precommit", "memory.db")
		ctx, cancel := context.WithCancel(context.Background())
		restoreHooks := setTestHooks(testHooks{beforeInitializeCommit: cancel})
		store, err := Open(ctx, path, testOptions(t))
		restoreHooks()
		if store != nil {
			_ = store.Close()
		}
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("Open = %v", err)
		}
		reopened, err := Open(context.Background(), path, testOptions(t))
		if err != nil {
			t.Fatalf("reopen after rollback: %v", err)
		}
		defer reopened.Close()
		identity, err := reopened.Identity(context.Background())
		if err != nil || identity.Generation != 0 {
			t.Fatalf("identity = %#v, %v", identity, err)
		}
	})

	t.Run("cancellation after known commit success does not mask success", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "commit-succeeded", "memory.db")
		ctx, cancel := context.WithCancel(context.Background())
		commitSucceeded := make(chan struct{})
		releaseResponse := make(chan struct{})
		installTestHooks(t, testHooks{bootstrapDriverCommit: func(exec func() error) error {
			if err := exec(); err != nil {
				return err
			}
			close(commitSucceeded)
			<-releaseResponse
			return nil
		}})
		type openResult struct {
			store *Store
			err   error
		}
		result := make(chan openResult, 1)
		options := testOptions(t)
		go func() {
			store, err := Open(ctx, path, options)
			result <- openResult{store: store, err: err}
		}()
		<-commitSucceeded
		cancel()
		close(releaseResponse)
		opened := <-result
		if opened.err != nil {
			t.Fatalf("Open = %v", opened.err)
		}
		defer opened.store.Close()
		identity, err := opened.store.Identity(context.Background())
		if err != nil || identity.Generation != 0 {
			t.Fatalf("identity = %#v, %v", identity, err)
		}
	})

	t.Run("already initialized verification keeps cancellation", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "already-initialized", "memory.db")
		initialized := openTestStore(t, path)
		if err := initialized.Close(); err != nil {
			t.Fatal(err)
		}
		ctx, cancel := context.WithCancel(context.Background())
		installTestHooks(t, testHooks{initializeCommitStarted: cancel})
		store, err := Open(ctx, path, testOptions(t))
		if store != nil {
			_ = store.Close()
		}
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("Open = %v, want context cancellation", err)
		}
	})

	t.Run("cancellation with ambiguous commit preserves commit unknown", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "commit-ambiguous", "memory.db")
		ctx, cancel := context.WithCancel(context.Background())
		commitSucceeded := make(chan struct{})
		releaseResponse := make(chan struct{})
		installTestHooks(t, testHooks{bootstrapDriverCommit: func(exec func() error) error {
			if err := exec(); err != nil {
				return err
			}
			close(commitSucceeded)
			<-releaseResponse
			return errors.New("injected response loss")
		}})
		result := make(chan error, 1)
		options := testOptions(t)
		go func() {
			store, err := Open(ctx, path, options)
			if store != nil {
				_ = store.Close()
			}
			result <- err
		}()
		<-commitSucceeded
		cancel()
		close(releaseResponse)
		err := <-result
		if !errors.Is(err, memory.ErrCommitUnknown) || errors.Is(err, context.Canceled) {
			t.Fatalf("Open = %v, want ErrCommitUnknown", err)
		}
	})
}

func TestBorrowedReadBadConnectionsAreQuarantined(t *testing.T) {
	now := time.Date(2026, 8, 30, 1, 0, 0, 0, time.UTC)
	tests := []struct {
		name      string
		operation string
		prepare   func(*testing.T, *Store) func() error
	}{
		{"identity", "identity", func(_ *testing.T, store *Store) func() error {
			return func() error { _, err := store.Identity(context.Background()); return err }
		}},
		{"exact record", "record-exact", func(t *testing.T, store *Store) func() error {
			record := mustSQLiteCreate(t, store, sqliteTestRecord("bad-exact", "bad-exact", now))
			return func() error {
				_, err := store.Get(context.Background(), memory.RecordRef{Scope: record.Scope, ID: record.ID})
				return err
			}
		}},
		{"record list", "record-list", func(t *testing.T, store *Store) func() error {
			record := mustSQLiteCreate(t, store, sqliteTestRecord("bad-list", "bad-list", now))
			return func() error {
				_, err := store.List(context.Background(), memory.ListRequest{Scopes: []memory.Scope{record.Scope}, Limit: 1, IncludeExpired: true})
				return err
			}
		}},
		{"candidate", "candidate-exact", func(t *testing.T, store *Store) func() error {
			scope := memory.Scope{Namespace: memory.NamespaceUser, ID: "bad-candidate-scope"}
			candidate := mustSQLitePropose(t, store, sqlitePendingCandidate("bad-candidate", scope, now))
			return func() error {
				_, err := store.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: candidate.ID})
				return err
			}
		}},
		{"receipt", "receipt", func(t *testing.T, store *Store) func() error {
			receipt, err := store.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: "bad-receipt", CreatedAt: now})
			if err != nil || receipt.ObservationID == "" {
				t.Fatalf("commit receipt = %#v, %v", receipt, err)
			}
			return func() error {
				_, err := store.GetObservationReceipt(context.Background(), receipt.ObservationID)
				return err
			}
		}},
		{"retrieval", "retrieval", func(t *testing.T, store *Store) func() error {
			record := mustSQLiteCreate(t, store, sqliteTestRecord("bad-retrieval", "bad-retrieval", now))
			return func() error {
				_, err := store.Retrieve(context.Background(), memory.RetrievalRequest{Query: "SQLite", Scopes: []memory.Scope{record.Scope}, Limit: 1, TokenBudget: 100, IncludeExpired: true})
				return err
			}
		}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
			action := test.prepare(t, store)
			var calls int
			installTestHooks(t, testHooks{borrowedRead: func(operation string, conn *sql.Conn) {
				if operation != test.operation {
					return
				}
				calls++
				if err := conn.Raw(func(any) error { return driver.ErrBadConn }); err != driver.ErrBadConn {
					t.Fatalf("Raw discard = %v", err)
				}
			}})
			err := action()
			if !errors.Is(err, memory.ErrUnavailable) || errors.Is(err, memory.ErrClosed) {
				t.Fatalf("read error = %v", err)
			}
			if calls != 1 || store.quarantined.Load() != 1 || len(store.connections) != retainedConnectionCount-1 || store.database.Stats().OpenConnections != retainedConnectionCount-1 {
				t.Fatalf("accounting calls=%d quarantined=%d retained=%d physical=%d", calls, store.quarantined.Load(), len(store.connections), store.database.Stats().OpenConnections)
			}
			if _, err := store.Identity(context.Background()); !errors.Is(err, memory.ErrUnavailable) {
				t.Fatalf("poisoned identity = %v", err)
			}
		})
	}
}

func TestReviewRejectAfterIndependentTombstone(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
	now := time.Date(2026, 8, 30, 2, 0, 0, 0, time.UTC)
	target := mustSQLiteCreate(t, store, sqliteTestRecord("review-target", "review-target", now))
	candidate := sqlitePendingCandidate("reject-after-tombstone", target.Scope, now.Add(time.Second))
	candidate.Action, candidate.TargetID, candidate.BaseRevision = memory.CandidateUpdate, target.ID, target.Revision
	candidate.Proposed.Key = target.Key
	candidate = mustSQLitePropose(t, store, candidate)
	if _, err := store.Forget(context.Background(), memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: target.Scope, ID: target.ID}, ExpectedRevision: target.Revision, ForgottenAt: now.Add(2 * time.Second)}); err != nil {
		t.Fatal(err)
	}
	result, err := store.Review(context.Background(), memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: target.Scope, ID: candidate.ID}, Decision: memory.ReviewReject, DecisionSource: memory.OriginHuman, DecidedAt: now.Add(3 * time.Second)})
	if err != nil {
		t.Fatalf("reject = %v", err)
	}
	if result.Candidate.State != memory.CandidateRejected || result.Candidate.Reason != "" || result.Candidate.Proposed.Text != "" || result.Record != nil || result.Tombstone != nil {
		t.Fatalf("result = %#v", result)
	}
}

func TestOrdinaryCreateRejectsOrphanFTSWithoutMutation(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path)
	now := time.Date(2026, 8, 30, 3, 0, 0, 0, time.UTC)
	record := sqliteTestRecord("orphan-create", "orphan-create", now)
	database := openExternalSQLite(t, path)
	if _, err := database.Exec(`INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES(?,?,?,?,?)`, record.ID, "orphan", record.Kind, record.Key, ""); err != nil {
		t.Fatal(err)
	}
	_ = database.Close()
	before, err := store.Identity(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	_, err = store.Upsert(context.Background(), memory.UpsertRequest{Record: record})
	if !errors.Is(err, memory.ErrCorrupt) {
		t.Fatalf("Upsert = %v", err)
	}
	after, identityErr := store.Identity(context.Background())
	if identityErr != nil || after.Generation != before.Generation {
		t.Fatalf("generation before=%d after=%d err=%v", before.Generation, after.Generation, identityErr)
	}
	if _, err := store.Get(context.Background(), memory.RecordRef{Scope: record.Scope, ID: record.ID}); !errors.Is(err, memory.ErrNotFound) {
		t.Fatalf("rolled-back record = %v", err)
	}
}

func TestRankedRetrievalRejectsStaleAndDuplicateSelectedFTS(t *testing.T) {
	for _, test := range []struct {
		name   string
		mutate func(*testing.T, *sql.DB, memory.Record)
	}{
		{"stale", func(t *testing.T, database *sql.DB, record memory.Record) {
			if _, err := database.Exec(`UPDATE memory_records_fts SET text_value='stale needle' WHERE record_id=?`, record.ID); err != nil {
				t.Fatal(err)
			}
		}},
		{"duplicate", func(t *testing.T, database *sql.DB, record memory.Record) {
			if _, err := database.Exec(`INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES(?,?,?,?,?)`, record.ID, record.Text, record.Kind, record.Key, ftsLabels(record.Labels)); err != nil {
				t.Fatal(err)
			}
		}},
	} {
		t.Run(test.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "memory.db")
			store := openTestStore(t, path)
			now := time.Date(2026, 8, 30, 4, 0, 0, 0, time.UTC)
			record := sqliteTestRecord("fts-"+test.name, "fts-"+test.name, now)
			record.Text = "needle retrieval text"
			record = mustSQLiteCreate(t, store, record)
			database := openExternalSQLite(t, path)
			test.mutate(t, database, record)
			_ = database.Close()
			_, err := store.Retrieve(context.Background(), memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{record.Scope}, Limit: 10, TokenBudget: 100, IncludeExpired: true})
			if !errors.Is(err, memory.ErrCorrupt) {
				t.Fatalf("Retrieve = %v", err)
			}
		})
	}
}

func TestSQLiteRevisionInputsRejectMaxInt64PlusOne(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
	now := time.Date(2026, 8, 30, 5, 0, 0, 0, time.UTC)
	record := mustSQLiteCreate(t, store, sqliteTestRecord("revision-record", "revision-record", now))
	maximum := uint64(math.MaxInt64)
	over := maximum + 1

	updated := memory.CloneRecord(record)
	updated.Revision, updated.UpdatedAt = maximum, now.Add(time.Second)
	if _, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: updated, ExpectedRevision: &maximum}); !errors.Is(err, memory.ErrConflict) || errors.Is(err, memory.ErrUnavailable) {
		t.Fatalf("upsert max = %v", err)
	}
	if _, err := store.Forget(context.Background(), memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: record.Scope, ID: record.ID}, ExpectedRevision: maximum, ForgottenAt: now.Add(time.Second)}); !errors.Is(err, memory.ErrConflict) || errors.Is(err, memory.ErrUnavailable) {
		t.Fatalf("forget max = %v", err)
	}

	updated.Revision = over
	if _, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: updated, ExpectedRevision: &over}); !errors.Is(err, memory.ErrInvalidRequest) {
		t.Fatalf("upsert over = %v", err)
	}
	if _, err := store.Forget(context.Background(), memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: record.Scope, ID: record.ID}, ExpectedRevision: over, ForgottenAt: now.Add(time.Second)}); !errors.Is(err, memory.ErrInvalidRequest) {
		t.Fatalf("forget over = %v", err)
	}
	candidate := sqlitePendingCandidate("revision-candidate", record.Scope, now.Add(time.Second))
	candidate.Action, candidate.TargetID, candidate.BaseRevision = memory.CandidateUpdate, record.ID, maximum
	if _, err := store.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{candidate}}); err != nil {
		t.Fatalf("propose max = %v", err)
	}
	candidate.ID, candidate.BaseRevision = "revision-candidate-over", over
	if _, err := store.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{candidate}}); !errors.Is(err, memory.ErrInvalidRecord) {
		t.Fatalf("propose over = %v", err)
	}
}
