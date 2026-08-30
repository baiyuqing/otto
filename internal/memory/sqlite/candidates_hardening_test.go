package sqlite

import (
	"context"
	"errors"
	"path/filepath"
	"reflect"
	"strconv"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

type receiptTrackingGuard struct {
	guarded atomic.Bool
}

func (guard *receiptTrackingGuard) Check(ctx context.Context, input memory.GuardInput) error {
	if err := (memory.DefaultGuard{}).Check(ctx, input); err != nil {
		return err
	}
	if len(input.Fields) >= 1 && input.Fields[0].Name == "observation ID" {
		receipt := true
		for _, field := range input.Fields[1:] {
			if field.Name != "candidate ID" {
				receipt = false
				break
			}
		}
		if receipt {
			guard.guarded.Store(true)
		}
	}
	return nil
}

func TestCandidateReceiptCorrespondenceIsBoundedAndPreservesOperationalErrors(t *testing.T) {
	t.Run("more than maximum associated candidates is corrupt", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "memory.db")
		store := openTestStore(t, path)
		at := time.Date(2026, 8, 30, 9, 0, 0, 0, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: "bounded-receipt"}
		candidate := sqlitePendingCandidate("bounded-original", scope, at)
		candidate.Proposed.Source.ObservationID = "bounded-observation"
		if _, err := store.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: "bounded-observation", Candidates: []memory.Candidate{candidate}, CreatedAt: at}); err != nil {
			t.Fatal(err)
		}
		database := openExternalSQLite(t, path)
		for index := 0; index < memory.MaxCandidateBatch; index++ {
			id := "bounded-extra-" + strconv.Itoa(index)
			if _, err := database.Exec(`INSERT INTO memory_candidates(
				id,scope_namespace,scope_id,action,target_id,base_revision,observation_id,proposed_json,reason,state,
				created_at,decided_at,decision_source,result_record_id,result_revision)
				SELECT ?,scope_namespace,scope_id,action,target_id,base_revision,observation_id,proposed_json,reason,state,
				created_at,decided_at,decision_source,result_record_id,result_revision FROM memory_candidates WHERE id=?`, id, candidate.ID); err != nil {
				t.Fatal(err)
			}
		}
		if err := database.Close(); err != nil {
			t.Fatal(err)
		}
		if _, err := store.GetObservationReceipt(context.Background(), "bounded-observation"); !errors.Is(err, memory.ErrCorrupt) {
			t.Fatalf("oversubscribed receipt = %v", err)
		}
	})

	for _, event := range []observationCorrespondenceEvent{observationCandidateCorrespondence, observationAssociationCorrespondence} {
		event := event
		t.Run(event.String()+" cancellation", func(t *testing.T) {
			store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
			at := time.Date(2026, 8, 30, 9, 5, 0, 0, time.UTC)
			scope := memory.Scope{Namespace: "user", ID: "receipt-cancel"}
			candidate := sqlitePendingCandidate("receipt-cancel-candidate", scope, at)
			candidate.Proposed.Source.ObservationID = "receipt-cancel-observation"
			if _, err := store.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: "receipt-cancel-observation", Candidates: []memory.Candidate{candidate}, CreatedAt: at}); err != nil {
				t.Fatal(err)
			}
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			installTestHooks(t, testHooks{beforeObservationCorrespondence: func(got observationCorrespondenceEvent) {
				if got == event {
					cancel()
				}
			}})
			if _, err := store.GetObservationReceipt(ctx, "receipt-cancel-observation"); !errors.Is(err, context.Canceled) {
				t.Fatalf("correspondence cancellation = %v", err)
			}
		})
	}
}

func TestCandidateObservationReceiptIsGuardedBeforeCommit(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	guard := &receiptTrackingGuard{}
	store := openTestStore(t, path, func(options *Options) { options.Guard = testGuardWith(t, guard) })
	at := time.Date(2026, 8, 30, 9, 10, 0, 0, time.UTC)
	scope := memory.Scope{Namespace: "user", ID: "receipt-pre-guard"}
	candidate := sqlitePendingCandidate("receipt-pre-guard-candidate", scope, at)
	candidate.Proposed.Source.ObservationID = "receipt-pre-guard-observation"
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	var guardedAtCommit atomic.Bool
	installTestHooks(t, testHooks{commitStarted: func() {
		guardedAtCommit.Store(guard.guarded.Load())
		cancel()
	}})
	receipt, err := store.CommitObservation(ctx, memory.ObservationCommit{ObservationID: "receipt-pre-guard-observation", Candidates: []memory.Candidate{candidate}, CreatedAt: at})
	if err != nil || !guardedAtCommit.Load() || receipt.ObservationID != "receipt-pre-guard-observation" || receipt.Existing {
		t.Fatalf("guarded receipt commit = %#v, %v, guarded before commit=%t", receipt, err, guardedAtCommit.Load())
	}
}

func observationMissBarrier() (func(*Store), *atomic.Int32) {
	var arrived atomic.Int32
	release := make(chan struct{})
	var once sync.Once
	return func(*Store) {
		if arrived.Add(1) == 2 {
			once.Do(func() { close(release) })
		}
		<-release
	}, &arrived
}

func runObservationRace(t *testing.T, rollbackFailure bool) ([]struct {
	receipt memory.ObservationReceipt
	err     error
}, []*Store, *atomic.Int32) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "memory.db")
	first := openTestStore(t, path)
	second, err := Open(context.Background(), path, testOptions(t))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = second.Close() })
	at := time.Date(2026, 8, 30, 9, 15, 0, 0, time.UTC)
	scope := memory.Scope{Namespace: "user", ID: "deterministic-observe-race"}
	commits := make([]memory.ObservationCommit, 2)
	for index := range commits {
		candidate := sqlitePendingCandidate("deterministic-observe-"+strconv.Itoa(index), scope, at)
		candidate.Proposed.Source.ObservationID = "deterministic-observation"
		commits[index] = memory.ObservationCommit{ObservationID: "deterministic-observation", Candidates: []memory.Candidate{candidate}, CreatedAt: at}
	}
	barrier, arrived := observationMissBarrier()
	hooks := testHooks{afterObservationInitialMiss: barrier}
	if rollbackFailure {
		hooks.driverExec = func(statement string, exec func() error) error {
			if statement == "ROLLBACK" {
				return errors.New("synthetic rollback failure")
			}
			return exec()
		}
	}
	restore := setTestHooks(hooks)
	defer restore()
	type outcome struct {
		index   int
		receipt memory.ObservationReceipt
		err     error
	}
	results := make(chan outcome, 2)
	for index, store := range []*Store{first, second} {
		index, store := index, store
		go func() {
			receipt, err := store.CommitObservation(context.Background(), commits[index])
			results <- outcome{index: index, receipt: receipt, err: err}
		}()
	}
	ordered := make([]struct {
		receipt memory.ObservationReceipt
		err     error
	}, 2)
	for range 2 {
		select {
		case result := <-results:
			ordered[result.index] = struct {
				receipt memory.ObservationReceipt
				err     error
			}{result.receipt, result.err}
		case <-time.After(5 * time.Second):
			t.Fatal("observation race timed out")
		}
	}
	return ordered, []*Store{first, second}, arrived
}

func TestCandidateObservationTransactionRecheckAndRollbackFailure(t *testing.T) {
	t.Run("both initial misses converge through transaction recheck", func(t *testing.T) {
		outcomes, stores, arrived := runObservationRace(t, false)
		if arrived.Load() != 2 || outcomes[0].err != nil || outcomes[1].err != nil ||
			!reflect.DeepEqual(outcomes[0].receipt.CandidateIDs, outcomes[1].receipt.CandidateIDs) ||
			outcomes[0].receipt.Existing == outcomes[1].receipt.Existing {
			t.Fatalf("deterministic race = %#v, arrivals=%d", outcomes, arrived.Load())
		}
		identity, err := stores[0].Identity(context.Background())
		if err != nil || identity.Generation != 1 {
			t.Fatalf("race generation = %d, %v", identity.Generation, err)
		}
	})

	t.Run("raced receipt cannot hide rollback failure", func(t *testing.T) {
		outcomes, _, arrived := runObservationRace(t, true)
		if arrived.Load() != 2 {
			t.Fatalf("initial misses = %d", arrived.Load())
		}
		var successes, unavailable int
		for _, outcome := range outcomes {
			switch {
			case outcome.err == nil:
				successes++
			case errors.Is(outcome.err, memory.ErrUnavailable):
				unavailable++
				if outcome.receipt.ObservationID != "" {
					t.Fatalf("rollback failure returned receipt: %#v", outcome.receipt)
				}
			default:
				t.Fatalf("unexpected race outcome: %#v", outcome)
			}
		}
		if successes != 1 || unavailable != 1 {
			t.Fatalf("rollback race outcomes = %#v", outcomes)
		}
	})
}

func TestCandidatePublicationHookRunsAfterResourcesAreReleased(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
	at := time.Date(2026, 8, 30, 9, 20, 0, 0, time.UTC)
	scope := memory.Scope{Namespace: "user", ID: "reentrant-publication"}
	outer := sqlitePendingCandidate("publication-outer", scope, at)
	inner := sqlitePendingCandidate("publication-inner", scope, at.Add(time.Second))
	var entered atomic.Bool
	var nestedErr error
	installTestHooks(t, testHooks{afterCandidateCommit: func() error {
		if !entered.CompareAndSwap(false, true) {
			return nil
		}
		ctx, cancel := context.WithTimeout(context.Background(), time.Second)
		defer cancel()
		_, nestedErr = store.Propose(ctx, memory.ProposalBatch{Candidates: []memory.Candidate{inner}})
		return nestedErr
	}})
	if _, err := store.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{outer}}); err != nil {
		t.Fatalf("outer proposal = %v (nested %v)", err, nestedErr)
	}
	identity, err := store.Identity(context.Background())
	if err != nil || identity.Generation != 2 {
		t.Fatalf("reentrant publication generation = %d, %v", identity.Generation, err)
	}
}

func TestCandidateReviewRebaseRacePreservesConflictRevisions(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path)
	second, err := Open(context.Background(), path, testOptions(t))
	if err != nil {
		t.Fatal(err)
	}
	defer second.Close()
	at := time.Date(2026, 8, 30, 9, 25, 0, 0, time.UTC)
	scope := memory.Scope{Namespace: "user", ID: "review-rebase-race"}
	target := sqliteTestRecord("review-rebase-target", "review-rebase-target", at)
	target.Scope = scope
	created := mustSQLiteCreate(t, store, target)
	expectedOne := uint64(1)
	revisionTwo := memory.CloneRecord(created)
	revisionTwo.Text, revisionTwo.UpdatedAt = "revision two", at.Add(time.Second)
	revisionTwo, err = second.Upsert(context.Background(), memory.UpsertRequest{Record: revisionTwo, ExpectedRevision: &expectedOne})
	if err != nil {
		t.Fatal(err)
	}
	candidate := sqlitePendingCandidate("review-rebase-candidate", scope, at.Add(2*time.Second))
	candidate.Action, candidate.TargetID, candidate.BaseRevision = memory.CandidateUpdate, created.ID, 1
	mustSQLitePropose(t, store, candidate)
	edited := memory.CloneRecord(candidate.Proposed)
	edited.Text = "losing rebased edit"
	revisionThree := memory.CloneRecord(revisionTwo)
	revisionThree.Text, revisionThree.UpdatedAt = "winning revision three", at.Add(3*time.Second)
	expectedTwo := uint64(2)
	var raced atomic.Bool
	var raceErr error
	installTestHooks(t, testHooks{beforeBegin: func() {
		if !raced.CompareAndSwap(false, true) {
			return
		}
		_, raceErr = second.Upsert(context.Background(), memory.UpsertRequest{Record: revisionThree, ExpectedRevision: &expectedTwo})
	}})
	_, err = store.Review(context.Background(), memory.StoreReviewRequest{
		Ref: memory.CandidateRef{Scope: scope, ID: candidate.ID}, Decision: memory.ReviewAccept, Edited: &edited,
		TargetRevision: &expectedTwo, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(4 * time.Second),
	})
	if raceErr != nil {
		t.Fatalf("winning update = %v", raceErr)
	}
	requireConflictRevisions(t, err, 2, 3)
	pending, getErr := store.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: candidate.ID})
	if getErr != nil || pending.State != memory.CandidatePending {
		t.Fatalf("candidate after target race = %#v, %v", pending, getErr)
	}
}

func TestCandidateAcceptedCreateRejectsOrphanAndDuplicateFTSRows(t *testing.T) {
	for _, rows := range []int{1, 2} {
		rows := rows
		t.Run(strconv.Itoa(rows)+" orphan rows", func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "memory.db")
			store := openTestStore(t, path)
			at := time.Date(2026, 8, 30, 9, 30, 0, 0, time.UTC)
			scope := memory.Scope{Namespace: "user", ID: "create-fts-corrupt"}
			candidate := sqlitePendingCandidate("create-fts-candidate", scope, at)
			mustSQLitePropose(t, store, candidate)
			database := openExternalSQLite(t, path)
			for range rows {
				if _, err := database.Exec(`INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES('create-fts-result','orphan','note','orphan','')`); err != nil {
					t.Fatal(err)
				}
			}
			if err := database.Close(); err != nil {
				t.Fatal(err)
			}
			_, err := store.Review(context.Background(), memory.StoreReviewRequest{
				Ref: memory.CandidateRef{Scope: scope, ID: candidate.ID}, ResultRecordID: "create-fts-result",
				Decision: memory.ReviewAccept, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(time.Second),
			})
			if !errors.Is(err, memory.ErrCorrupt) {
				t.Fatalf("accepted create over orphan FTS = %v", err)
			}
			pending, err := store.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: candidate.ID})
			if err != nil || pending.State != memory.CandidatePending {
				t.Fatalf("candidate after FTS corruption = %#v, %v", pending, err)
			}
			if _, err := store.Get(context.Background(), memory.RecordRef{Scope: scope, ID: "create-fts-result"}); !errors.Is(err, memory.ErrNotFound) {
				t.Fatalf("record after FTS corruption = %v", err)
			}
			identity, err := store.Identity(context.Background())
			if err != nil || identity.Generation != 1 {
				t.Fatalf("generation after FTS corruption = %d, %v", identity.Generation, err)
			}
		})
	}
}

func TestCandidateObservationFaultReopenParity(t *testing.T) {
	for _, seam := range []string{"precommit", "postcommit"} {
		seam := seam
		t.Run(seam, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "memory.db")
			store := openTestStore(t, path)
			at := time.Date(2026, 8, 30, 9, 35, 0, 0, time.UTC)
			scope := memory.Scope{Namespace: "user", ID: "observation-fault-" + seam}
			candidate := sqlitePendingCandidate("observation-fault-candidate-"+seam, scope, at)
			candidate.Proposed.Source.ObservationID = "observation-fault-" + seam
			hooks := testHooks{}
			if seam == "precommit" {
				hooks.beforeCandidateCommit = func() error { return errors.New("synthetic observation precommit failure") }
			} else {
				hooks.afterCandidateCommit = func() error { return errors.New("synthetic observation publication failure") }
			}
			restore := setTestHooks(hooks)
			receipt, commitErr := store.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: candidate.Proposed.Source.ObservationID, Candidates: []memory.Candidate{candidate}, CreatedAt: at})
			restore()
			if seam == "precommit" {
				if !errors.Is(commitErr, memory.ErrUnavailable) || receipt.ObservationID != "" {
					t.Fatalf("precommit observation = %#v, %v", receipt, commitErr)
				}
			} else {
				var unknown *memory.CommitUnknownError
				if !errors.As(commitErr, &unknown) || unknown.Operation() != memory.CommitObserve || !reflect.DeepEqual(unknown.EntityIDs(), []string{candidate.Proposed.Source.ObservationID, candidate.ID}) {
					t.Fatalf("postcommit observation = %#v, %v", unknown, commitErr)
				}
				if _, err := store.Identity(context.Background()); !errors.Is(err, memory.ErrUnavailable) {
					t.Fatalf("store after publication loss = %v", err)
				}
			}
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			reopened, err := Open(context.Background(), path, testOptions(t))
			if err != nil {
				t.Fatal(err)
			}
			defer reopened.Close()
			got, err := reopened.GetObservationReceipt(context.Background(), candidate.Proposed.Source.ObservationID)
			identity, identityErr := reopened.Identity(context.Background())
			if seam == "precommit" {
				if !errors.Is(err, memory.ErrNotFound) || identityErr != nil || identity.Generation != 0 {
					t.Fatalf("precommit reopen = %#v, %v, generation=%d/%v", got, err, identity.Generation, identityErr)
				}
				if _, err := reopened.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: candidate.ID}); !errors.Is(err, memory.ErrNotFound) {
					t.Fatalf("precommit candidate residue = %v", err)
				}
			} else if err != nil || !got.Existing || !reflect.DeepEqual(got.CandidateIDs, []string{candidate.ID}) || identityErr != nil || identity.Generation != 1 {
				t.Fatalf("postcommit reconciliation = %#v, %v, generation=%d/%v", got, err, identity.Generation, identityErr)
			}
		})
	}
}

type candidateFaultParity struct {
	recordRows    int
	activeRows    int
	tombstoneRows int
	ftsRows       int
	revision      int
	generation    uint64
	candidate     memory.CandidateState
	cleared       bool
}

func readCandidateFaultParity(t *testing.T, store *Store, path, recordID string, ref memory.CandidateRef) candidateFaultParity {
	t.Helper()
	var parity candidateFaultParity
	database := openExternalSQLite(t, path)
	defer database.Close()
	if err := database.QueryRow(`SELECT count(*),count(*) FILTER (WHERE state='active'),count(*) FILTER (WHERE state='tombstone'),COALESCE(max(revision),0) FROM memory_records WHERE id=?`, recordID).
		Scan(&parity.recordRows, &parity.activeRows, &parity.tombstoneRows, &parity.revision); err != nil {
		t.Fatal(err)
	}
	if err := database.QueryRow(`SELECT count(*) FROM memory_records_fts WHERE record_id=?`, recordID).Scan(&parity.ftsRows); err != nil {
		t.Fatal(err)
	}
	candidate, err := store.GetCandidate(context.Background(), ref)
	if err != nil {
		t.Fatal(err)
	}
	parity.candidate = candidate.State
	parity.cleared = candidate.Proposed.Text == "" && candidate.Reason == ""
	identity, err := store.Identity(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	parity.generation = identity.Generation
	return parity
}

func TestCandidateReviewFaultMatrixReopenParity(t *testing.T) {
	for _, action := range []memory.CandidateAction{memory.CandidateCreate, memory.CandidateUpdate, memory.CandidateForget} {
		action := action
		for _, seam := range []string{"predecision", "precommit", "postcommit"} {
			seam := seam
			t.Run(string(action)+"/"+seam, func(t *testing.T) {
				path := filepath.Join(t.TempDir(), "memory.db")
				store := openTestStore(t, path)
				at := time.Date(2026, 8, 30, 9, 40, 0, 0, time.UTC)
				scope := memory.Scope{Namespace: "user", ID: string(action) + "-" + seam}
				recordID := "fault-record-" + string(action) + "-" + seam
				baselineGeneration := uint64(0)
				if action != memory.CandidateCreate {
					target := sqliteTestRecord(recordID, recordID, at)
					target.Scope = scope
					mustSQLiteCreate(t, store, target)
					baselineGeneration++
				}
				candidate := sqlitePendingCandidate("fault-candidate-"+string(action)+"-"+seam, scope, at.Add(time.Second))
				candidate.Action = action
				if action != memory.CandidateCreate {
					candidate.TargetID, candidate.BaseRevision = recordID, 1
				}
				if action == memory.CandidateForget {
					source := candidate.Proposed.Source
					candidate.Proposed = clearedProposed(scope)
					candidate.Proposed.Source = source
				}
				mustSQLitePropose(t, store, candidate)
				baselineGeneration++
				request := memory.StoreReviewRequest{
					Ref: memory.CandidateRef{Scope: scope, ID: candidate.ID}, Decision: memory.ReviewAccept,
					DecisionSource: memory.OriginHuman, DecidedAt: at.Add(2 * time.Second),
				}
				if action == memory.CandidateCreate {
					request.ResultRecordID = recordID
				}
				hooks := testHooks{}
				switch seam {
				case "predecision":
					hooks.beforeCandidateDecision = func() error { return errors.New("synthetic predecision failure") }
				case "precommit":
					hooks.beforeCandidateCommit = func() error { return errors.New("synthetic precommit failure") }
				case "postcommit":
					hooks.afterCandidateCommit = func() error { return errors.New("synthetic postcommit failure") }
				}
				restore := setTestHooks(hooks)
				_, reviewErr := store.Review(context.Background(), request)
				restore()
				committed := seam == "postcommit"
				if committed {
					var unknown *memory.CommitUnknownError
					if !errors.As(reviewErr, &unknown) || unknown.Operation() != memory.CommitReview {
						t.Fatalf("postcommit review = %#v, %v", unknown, reviewErr)
					}
				} else if !errors.Is(reviewErr, memory.ErrUnavailable) {
					t.Fatalf("rollback review = %v", reviewErr)
				}
				if err := store.Close(); err != nil {
					t.Fatal(err)
				}
				reopened, err := Open(context.Background(), path, testOptions(t))
				if err != nil {
					t.Fatal(err)
				}
				defer reopened.Close()
				got := readCandidateFaultParity(t, reopened, path, recordID, request.Ref)
				want := candidateFaultParity{generation: baselineGeneration, candidate: memory.CandidatePending}
				if action != memory.CandidateCreate {
					want.recordRows, want.activeRows, want.ftsRows, want.revision = 1, 1, 1, 1
				}
				if committed {
					want.generation++
					want.candidate, want.cleared = memory.CandidateAccepted, true
					switch action {
					case memory.CandidateCreate:
						want.recordRows, want.activeRows, want.ftsRows, want.revision = 1, 1, 1, 1
					case memory.CandidateUpdate:
						want.revision = 2
					case memory.CandidateForget:
						want.activeRows, want.tombstoneRows, want.ftsRows, want.revision = 0, 1, 0, 2
					}
				}
				if got != want {
					t.Fatalf("reopened parity = %#v, want %#v", got, want)
				}
			})
		}
	}
}

var _ memory.ContentGuard = (*receiptTrackingGuard)(nil)
