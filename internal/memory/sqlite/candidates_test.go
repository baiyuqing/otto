package sqlite

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

func sqlitePendingCandidate(id string, scope memory.Scope, at time.Time) memory.Candidate {
	return memory.Candidate{
		ID: id, Action: memory.CandidateCreate, State: memory.CandidatePending, CreatedAt: at, Reason: "candidate reason",
		Proposed: memory.Record{
			Scope: scope, Kind: "note", Key: id, Text: "candidate proposal text", Labels: []string{"candidate"},
			Metadata: map[string]string{"purpose": "candidate-test"}, Confidence: 0.8,
			Source: memory.Provenance{Origin: memory.OriginModel, SessionID: "candidate-session", MessageIDs: []string{"candidate-message"}},
		},
	}
}

func mustSQLitePropose(t *testing.T, store *Store, candidate memory.Candidate) memory.Candidate {
	t.Helper()
	batch, err := store.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{candidate}})
	if err != nil {
		t.Fatal(err)
	}
	return batch.Candidates[0]
}

func TestCandidatePrivateWireAndDecisionClearing(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path)
	scope := memory.Scope{Namespace: "user", ID: "private-wire"}
	at := time.Date(2026, 8, 29, 21, 0, 0, 0, time.UTC)
	candidate := sqlitePendingCandidate("wire-candidate", scope, at)
	mustSQLitePropose(t, store, candidate)

	database := openExternalSQLite(t, path)
	defer database.Close()
	var raw string
	if err := database.QueryRow(`SELECT proposed_json FROM memory_candidates WHERE id=?`, candidate.ID).Scan(&raw); err != nil {
		t.Fatal(err)
	}
	if strings.Contains(raw, "Scope") || strings.Contains(raw, "ExpiresAt") || !strings.Contains(raw, `"scope_namespace":"user"`) || !strings.Contains(raw, `"observation_id":""`) {
		t.Fatalf("candidate used non-private wire shape: %s", raw)
	}
	var object map[string]json.RawMessage
	if err := json.Unmarshal([]byte(raw), &object); err != nil {
		t.Fatal(err)
	}
	wantKeys := []string{"confidence", "expiry", "key", "kind", "labels", "metadata", "scope_id", "scope_namespace", "source", "text"}
	gotKeys := make([]string, 0, len(object))
	for key := range object {
		gotKeys = append(gotKeys, key)
	}
	sortStrings(gotKeys)
	if !reflect.DeepEqual(gotKeys, wantKeys) {
		t.Fatalf("wire keys = %v, want %v", gotKeys, wantKeys)
	}

	result, err := store.Review(context.Background(), memory.StoreReviewRequest{
		Ref: memory.CandidateRef{Scope: scope, ID: candidate.ID}, Decision: memory.ReviewReject,
		DecisionSource: memory.OriginMigration, DecidedAt: at.Add(time.Second),
	})
	if err != nil {
		t.Fatal(err)
	}
	if result.Candidate.Proposed.Text != "" || result.Candidate.Reason != "" {
		t.Fatalf("returned candidate retained content: %#v", result.Candidate)
	}
	var reason string
	if err := database.QueryRow(`SELECT proposed_json,reason FROM memory_candidates WHERE id=?`, candidate.ID).Scan(&raw, &reason); err != nil {
		t.Fatal(err)
	}
	const cleared = `{"scope_namespace":"user","scope_id":"private-wire","kind":"","key":"","text":"","labels":[],"metadata":{},"source":{},"confidence":0,"expiry":null}`
	if raw != cleared || reason != "" {
		t.Fatalf("durable clear = %s reason=%q", raw, reason)
	}
}

func TestCandidateObservationOrphanAndMismatchAreCorrupt(t *testing.T) {
	for _, test := range []struct {
		name   string
		mutate func(*testing.T, *sql.DB)
		read   func(*Store) error
	}{
		{
			name: "receipt orphan",
			mutate: func(t *testing.T, database *sql.DB) {
				if _, err := database.Exec(`UPDATE memory_observations SET candidate_ids_json='["missing-candidate"]' WHERE id='corrupt-observation'`); err != nil {
					t.Fatal(err)
				}
			},
			read: func(store *Store) error {
				_, err := store.GetObservationReceipt(context.Background(), "corrupt-observation")
				return err
			},
		},
		{
			name: "candidate receipt mismatch",
			mutate: func(t *testing.T, database *sql.DB) {
				if _, err := database.Exec(`PRAGMA ignore_check_constraints=ON`); err != nil {
					t.Fatal(err)
				}
				if _, err := database.Exec(`UPDATE memory_candidates SET observation_id='other-observation' WHERE id='corrupt-candidate'`); err != nil {
					t.Fatal(err)
				}
			},
			read: func(store *Store) error {
				_, err := store.GetCandidate(context.Background(), memory.CandidateRef{Scope: memory.Scope{Namespace: "user", ID: "corruption"}, ID: "corrupt-candidate"})
				return err
			},
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "memory.db")
			store := openTestStore(t, path)
			at := time.Date(2026, 8, 29, 21, 10, 0, 0, time.UTC)
			scope := memory.Scope{Namespace: "user", ID: "corruption"}
			candidate := sqlitePendingCandidate("corrupt-candidate", scope, at)
			candidate.Proposed.Source.ObservationID = "corrupt-observation"
			if _, err := store.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: "corrupt-observation", Candidates: []memory.Candidate{candidate}, CreatedAt: at}); err != nil {
				t.Fatal(err)
			}
			if _, err := store.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: "other-observation", Candidates: []memory.Candidate{}, CreatedAt: at}); err != nil {
				t.Fatal(err)
			}
			database := openExternalSQLite(t, path)
			test.mutate(t, database)
			_ = database.Close()
			err := test.read(store)
			if !errors.Is(err, memory.ErrCorrupt) || strings.Contains(err.Error(), candidate.Proposed.Text) {
				t.Fatalf("corrupt read = %v", err)
			}
		})
	}
}

func TestCandidateFaultSeamsRollbackAndCommitReconciliation(t *testing.T) {
	t.Run("before candidate decision rolls back materialized record", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "memory.db")
		store := openTestStore(t, path)
		at := time.Date(2026, 8, 29, 21, 20, 0, 0, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: "decision-fault"}
		candidate := sqlitePendingCandidate("decision-fault-candidate", scope, at)
		mustSQLitePropose(t, store, candidate)
		installTestHooks(t, testHooks{beforeCandidateDecision: func() error { return errors.New("synthetic decision failure") }})
		_, err := store.Review(context.Background(), memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: candidate.ID}, ResultRecordID: "decision-fault-record", Decision: memory.ReviewAccept, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(time.Second)})
		if !errors.Is(err, memory.ErrUnavailable) {
			t.Fatalf("decision fault = %v", err)
		}
		got, err := store.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: candidate.ID})
		if err != nil || got.State != memory.CandidatePending {
			t.Fatalf("candidate after rollback = %#v, %v", got, err)
		}
		if _, err := store.Get(context.Background(), memory.RecordRef{Scope: scope, ID: "decision-fault-record"}); !errors.Is(err, memory.ErrNotFound) {
			t.Fatalf("record residue = %v", err)
		}
	})

	t.Run("real commit response loss reconciles review", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "memory.db")
		store := openTestStore(t, path)
		at := time.Date(2026, 8, 29, 21, 30, 0, 0, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: "review-loss"}
		candidate := sqlitePendingCandidate("review-loss-candidate", scope, at)
		mustSQLitePropose(t, store, candidate)
		restore := setTestHooks(testHooks{driverExec: func(statement string, exec func() error) error {
			if statement != "COMMIT" {
				return exec()
			}
			if err := exec(); err != nil {
				return err
			}
			return errors.New("synthetic response loss")
		}})
		_, err := store.Review(context.Background(), memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: candidate.ID}, Decision: memory.ReviewReject, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(time.Second)})
		restore()
		var unknown *memory.CommitUnknownError
		if !errors.As(err, &unknown) || unknown.Operation() != memory.CommitReview || !reflect.DeepEqual(unknown.EntityIDs(), []string{candidate.ID}) {
			t.Fatalf("commit loss = %#v, %v", unknown, err)
		}
		if _, err := store.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: candidate.ID}); !errors.Is(err, memory.ErrUnavailable) {
			t.Fatalf("old store after unknown = %v", err)
		}
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		options := testOptions(t)
		reopened, err := Open(context.Background(), path, options)
		if err != nil {
			t.Fatal(err)
		}
		defer reopened.Close()
		reconciled, err := reopened.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: candidate.ID})
		if err != nil || reconciled.State != memory.CandidateRejected || reconciled.Proposed.Text != "" {
			t.Fatalf("reconciliation = %#v, %v", reconciled, err)
		}
	})
}

func TestCandidateCommitHooksAndObservationRace(t *testing.T) {
	t.Run("before commit rolls back proposal", func(t *testing.T) {
		store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
		at := time.Date(2026, 8, 29, 21, 35, 0, 0, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: "before-commit"}
		candidate := sqlitePendingCandidate("before-commit-candidate", scope, at)
		installTestHooks(t, testHooks{beforeCandidateCommit: func() error { return errors.New("synthetic precommit failure") }})
		if _, err := store.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{candidate}}); !errors.Is(err, memory.ErrUnavailable) {
			t.Fatalf("precommit proposal = %v", err)
		}
		if _, err := store.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: candidate.ID}); !errors.Is(err, memory.ErrNotFound) {
			t.Fatalf("precommit residue = %v", err)
		}
		identity, err := store.Identity(context.Background())
		if err != nil || identity.Generation != 0 {
			t.Fatalf("precommit generation = %d, %v", identity.Generation, err)
		}
	})

	t.Run("postcommit publication failure reconciles proposal", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "memory.db")
		store := openTestStore(t, path)
		at := time.Date(2026, 8, 29, 21, 36, 0, 0, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: "after-commit"}
		candidate := sqlitePendingCandidate("after-commit-candidate", scope, at)
		installTestHooks(t, testHooks{afterCandidateCommit: func() error { return errors.New("synthetic publication failure") }})
		_, err := store.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{candidate}})
		var unknown *memory.CommitUnknownError
		if !errors.As(err, &unknown) || unknown.Operation() != memory.CommitPropose || !reflect.DeepEqual(unknown.EntityIDs(), []string{candidate.ID}) {
			t.Fatalf("postcommit proposal = %#v, %v", unknown, err)
		}
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		options := testOptions(t)
		reopened, err := Open(context.Background(), path, options)
		if err != nil {
			t.Fatal(err)
		}
		defer reopened.Close()
		page, err := reopened.ListCandidates(context.Background(), memory.CandidateListRequest{Scopes: []memory.Scope{scope}, States: []memory.CandidateState{memory.CandidatePending}, Limit: 10})
		if err != nil || len(page.Candidates) != 1 || page.Candidates[0].ID != candidate.ID {
			t.Fatalf("postcommit reconciliation = %#v, %v", page, err)
		}
	})

	t.Run("two handles converge on one observation receipt", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "memory.db")
		first := openTestStore(t, path)
		options := testOptions(t)
		second, err := Open(context.Background(), path, options)
		if err != nil {
			t.Fatal(err)
		}
		defer second.Close()
		at := time.Date(2026, 8, 29, 21, 37, 0, 0, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: "observe-race"}
		commits := make([]memory.ObservationCommit, 2)
		for index, id := range []string{"observe-race-a", "observe-race-b"} {
			candidate := sqlitePendingCandidate(id, scope, at)
			candidate.Proposed.Source.ObservationID = "observe-race"
			commits[index] = memory.ObservationCommit{ObservationID: "observe-race", Candidates: []memory.Candidate{candidate}, CreatedAt: at}
		}
		type outcome struct {
			receipt memory.ObservationReceipt
			err     error
		}
		outcomes := make(chan outcome, 2)
		start := make(chan struct{})
		for index, store := range []*Store{first, second} {
			index, store := index, store
			go func() {
				<-start
				receipt, err := store.CommitObservation(context.Background(), commits[index])
				outcomes <- outcome{receipt: receipt, err: err}
			}()
		}
		close(start)
		left, right := <-outcomes, <-outcomes
		if left.err != nil || right.err != nil || !reflect.DeepEqual(left.receipt.CandidateIDs, right.receipt.CandidateIDs) || left.receipt.Existing == right.receipt.Existing {
			t.Fatalf("observation race = (%#v, %v), (%#v, %v)", left.receipt, left.err, right.receipt, right.err)
		}
		identity, err := first.Identity(context.Background())
		if err != nil || identity.Generation != 1 {
			t.Fatalf("observation race generation = %d, %v", identity.Generation, err)
		}
	})
}

func TestCandidateSensitiveEditAndPendingSameKeyAfterTombstone(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path, func(options *Options) { options.Guard = testGuard(t, testForbidden) })
	ctx := context.Background()
	at := time.Date(2026, 8, 29, 21, 40, 0, 0, time.UTC)
	scope := memory.Scope{Namespace: "user", ID: "candidate-edit"}
	target := sqliteTestRecord("candidate-target", "reusable-key", at)
	target.Scope = scope
	created := mustSQLiteCreate(t, store, target)

	update := sqlitePendingCandidate("sensitive-edit-candidate", scope, at.Add(time.Second))
	update.Action, update.TargetID, update.BaseRevision = memory.CandidateUpdate, created.ID, created.Revision
	mustSQLitePropose(t, store, update)
	edited := memory.CloneRecord(update.Proposed)
	edited.Text = testForbidden
	beforeIdentity, _ := store.Identity(ctx)
	_, err := store.Review(ctx, memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: update.ID}, Decision: memory.ReviewAccept, Edited: &edited, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(2 * time.Second)})
	if !errors.Is(err, memory.ErrSensitiveMemory) {
		t.Fatalf("sensitive edit = %v", err)
	}
	afterTarget, _ := store.Get(ctx, memory.RecordRef{Scope: scope, ID: created.ID})
	afterCandidate, _ := store.GetCandidate(ctx, memory.CandidateRef{Scope: scope, ID: update.ID})
	afterIdentity, _ := store.Identity(ctx)
	if !reflect.DeepEqual(afterTarget, created) || afterCandidate.State != memory.CandidatePending || beforeIdentity.Generation != afterIdentity.Generation {
		t.Fatalf("sensitive edit changed state: target=%#v candidate=%#v generation=%d/%d", afterTarget, afterCandidate, beforeIdentity.Generation, afterIdentity.Generation)
	}

	if _, err := store.Forget(ctx, memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: scope, ID: created.ID}, ExpectedRevision: 1, ForgottenAt: at.Add(3 * time.Second)}); err != nil {
		t.Fatal(err)
	}
	create := sqlitePendingCandidate("same-key-candidate", scope, at.Add(4*time.Second))
	create.Proposed.Key = created.Key
	mustSQLitePropose(t, store, create)
	accepted, err := store.Review(ctx, memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: create.ID}, ResultRecordID: "replacement-record", Decision: memory.ReviewAccept, DecisionSource: memory.OriginMigration, DecidedAt: at.Add(5 * time.Second)})
	if err != nil || accepted.Record == nil || accepted.Record.Key != created.Key {
		t.Fatalf("same key after tombstone = %#v, %v", accepted, err)
	}
	database := openExternalSQLite(t, path)
	defer database.Close()
	var active, tombstones, fts int
	var generation string
	if err := database.QueryRow(`SELECT count(*) FROM memory_records WHERE state='active'`).Scan(&active); err != nil {
		t.Fatal(err)
	}
	if err := database.QueryRow(`SELECT count(*) FROM memory_records WHERE state='tombstone'`).Scan(&tombstones); err != nil {
		t.Fatal(err)
	}
	if err := database.QueryRow(`SELECT count(*) FROM memory_records_fts`).Scan(&fts); err != nil {
		t.Fatal(err)
	}
	if err := database.QueryRow(`SELECT value FROM memory_meta WHERE key='generation'`).Scan(&generation); err != nil {
		t.Fatal(err)
	}
	if active != 1 || tombstones != 1 || fts != 1 || generation != "5" {
		t.Fatalf("row/FTS/generation parity = active %d tombstones %d fts %d generation %s", active, tombstones, fts, generation)
	}
}

func sortStrings(values []string) {
	for index := 1; index < len(values); index++ {
		for position := index; position > 0 && values[position] < values[position-1]; position-- {
			values[position], values[position-1] = values[position-1], values[position]
		}
	}
}
