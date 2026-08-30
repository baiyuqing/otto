package sqlite

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

type blockingGuard struct {
	base    memory.ContentGuard
	block   atomic.Bool
	entered chan struct{}
	release chan struct{}
	once    sync.Once
}

func (guard *blockingGuard) Check(ctx context.Context, input memory.GuardInput) error {
	if guard.block.Load() {
		guard.once.Do(func() { close(guard.entered) })
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-guard.release:
		}
	}
	return guard.base.Check(ctx, input)
}

func TestConcurrentCloseWaitsForGuardCallbackAndSharesResult(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	guard := &blockingGuard{base: testGuard(t), entered: make(chan struct{}), release: make(chan struct{})}
	store := openTestStore(t, path, func(options *Options) { options.Guard = guard })
	record := sqliteTestRecord("close-guard-record", "close-guard-key", time.Date(2026, 8, 30, 0, 0, 0, 0, time.UTC))
	created := mustSQLiteCreate(t, store, record)
	guard.block.Store(true)
	closeWaits := make(chan bool, 10)
	resourcesReady := make(chan struct{})
	resourcesRelease := make(chan struct{})
	installTestHooks(t, testHooks{
		closeWait:            func(owner bool) { closeWaits <- owner },
		beforeCloseResources: func() { close(resourcesReady); <-resourcesRelease },
	})

	operation := make(chan struct {
		record memory.Record
		err    error
	}, 1)
	go func() {
		got, err := store.Get(context.Background(), memory.RecordRef{Scope: created.Scope, ID: created.ID})
		operation <- struct {
			record memory.Record
			err    error
		}{got, err}
	}()
	<-guard.entered
	if got := len(store.connections); got != retainedConnectionCount {
		t.Fatalf("connections while guard blocks = %d, want %d", got, retainedConnectionCount)
	}
	if got := len(store.writeGate); got != 1 {
		t.Fatalf("write gate while guard blocks = %d, want 1", got)
	}
	if !store.lifecycleMu.TryLock() {
		t.Fatal("guard callback ran with lifecycle mutex held")
	}
	store.lifecycleMu.Unlock()

	const closers = 10
	results := make(chan error, closers)
	start := make(chan struct{})
	for index := 0; index < closers; index++ {
		go func() {
			<-start
			results <- store.Close()
		}()
	}
	close(start)

	// Every closer must have entered either the owner or waiter path before the
	// callback is released; post-close idempotence is not sufficient coverage.
	owners := 0
	for index := 0; index < closers; index++ {
		if <-closeWaits {
			owners++
		}
	}
	if owners != 1 {
		t.Fatalf("Close owner wait paths = %d, want 1", owners)
	}
	store.lifecycleMu.Lock()
	active := store.active
	store.lifecycleMu.Unlock()
	if active != 1 {
		t.Fatalf("accounted operations while guard blocks = %d, want 1", active)
	}
	select {
	case err := <-results:
		t.Fatalf("Close returned while guard callback blocked: %v", err)
	default:
	}
	if _, err := store.Identity(context.Background()); !errors.Is(err, memory.ErrClosed) {
		t.Fatalf("operation admitted after Close began = %v", err)
	}

	close(guard.release)
	got := <-operation
	if got.err != nil || got.record.ID != created.ID {
		t.Fatalf("admitted Get = %#v, %v", got.record, got.err)
	}
	<-resourcesReady
	if len(store.connections) != retainedConnectionCount || len(store.writeGate) != 1 || store.quarantined.Load() != 0 {
		t.Fatalf("guard Close accounting: connections=%d gate=%d quarantined=%d", len(store.connections), len(store.writeGate), store.quarantined.Load())
	}
	close(resourcesRelease)
	for index := 0; index < closers; index++ {
		if err := <-results; err != nil {
			t.Fatalf("Close %d = %v", index, err)
		}
	}
	if got := store.database.Stats().OpenConnections; got != 0 {
		t.Fatalf("physical connections after shared Close = %d", got)
	}
}

func TestConcurrentClosePublicOperationSeams(t *testing.T) {
	type seamCase struct {
		name    string
		prepare func(*testing.T, *Store, chan struct{}, <-chan struct{}) (testHooks, func() error)
		blocked func(*testing.T, *Store)
	}
	cases := []seamCase{
		{"Identity SQL response", func(_ *testing.T, store *Store, entered chan struct{}, release <-chan struct{}) (testHooks, func() error) {
			return testHooks{identityReadGeneration: func(query func() (uint64, error)) (uint64, error) {
					generation, err := query()
					close(entered)
					<-release
					return generation, err
				}}, func() error {
					identity, err := store.Identity(context.Background())
					if err == nil && identity != store.identity {
						return fmt.Errorf("Identity = %#v, want %#v", identity, store.identity)
					}
					return err
				}
		}, func(t *testing.T, store *Store) {
			store.lifecycleMu.Lock()
			active := store.active
			store.lifecycleMu.Unlock()
			if active != 1 || len(store.connections) != retainedConnectionCount-1 || len(store.writeGate) != 1 || store.quarantined.Load() != 0 {
				t.Fatalf("Identity SQL accounting: active=%d connections=%d gate=%d quarantined=%d", active, len(store.connections), len(store.writeGate), store.quarantined.Load())
			}
			if !store.lifecycleMu.TryLock() {
				t.Fatal("Identity SQL response hook ran with lifecycle mutex held")
			}
			store.lifecycleMu.Unlock()
		}},
		{"precommit", func(t *testing.T, store *Store, entered chan struct{}, release <-chan struct{}) (testHooks, func() error) {
			record := sqliteTestRecord("close-precommit-record", "close-precommit-key", time.Date(2026, 8, 30, 0, 40, 0, 0, time.UTC))
			return testHooks{beforeCommitCheck: func() { close(entered); <-release }}, func() error {
				_, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: record})
				return err
			}
		}, nil},
		{"COMMIT response", func(t *testing.T, store *Store, entered chan struct{}, release <-chan struct{}) (testHooks, func() error) {
			record := sqliteTestRecord("close-commit-record", "close-commit-key", time.Date(2026, 8, 30, 0, 41, 0, 0, time.UTC))
			return testHooks{driverExec: func(statement string, exec func() error) error {
					if statement != "COMMIT" {
						return exec()
					}
					if err := exec(); err != nil {
						return err
					}
					close(entered)
					<-release
					return nil
				}}, func() error {
					_, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: record})
					return err
				}
		}, nil},
		{"postcommit publication", func(t *testing.T, store *Store, entered chan struct{}, release <-chan struct{}) (testHooks, func() error) {
			candidate := sqlitePendingCandidate("close-publication-candidate", memory.Scope{Namespace: "user", ID: "close-publication-scope"}, time.Date(2026, 8, 30, 0, 42, 0, 0, time.UTC))
			return testHooks{afterCandidateCommit: func() error {
					assertCallbackResourcesFree(t, store)
					close(entered)
					<-release
					return nil
				}}, func() error {
					_, err := store.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{candidate}})
					return err
				}
		}, nil},
		{"blocking estimator", func(t *testing.T, store *Store, entered chan struct{}, release <-chan struct{}) (testHooks, func() error) {
			now := time.Date(2026, 8, 30, 0, 43, 0, 0, time.UTC)
			scope := memory.Scope{Namespace: "user", ID: "close-estimator-scope"}
			record := sqliteTestRecord("close-estimator-record", "close-estimator-key", now)
			record.Scope, record.Text = scope, "close estimator needle"
			mustSQLiteCreate(t, store, record)
			request := memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{scope}, Limit: 1, TokenBudget: 10, Now: now, EstimateTokens: func(string) int {
				assertCallbackResourcesFree(t, store)
				close(entered)
				<-release
				return 1
			}}
			return testHooks{}, func() error {
				_, err := store.Retrieve(context.Background(), request)
				return err
			}
		}, nil},
	}
	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
			entered := make(chan struct{})
			release := make(chan struct{})
			hooks, operation := test.prepare(t, store, entered, release)
			closeWaiting := make(chan bool, 10)
			resourcesReady := make(chan struct{})
			resourcesRelease := make(chan struct{})
			hooks.closeWait = func(owner bool) { closeWaiting <- owner }
			hooks.beforeCloseResources = func() { close(resourcesReady); <-resourcesRelease }
			installTestHooks(t, hooks)

			operationResult := make(chan error, 1)
			go func() { operationResult <- operation() }()
			<-entered
			if test.blocked != nil {
				test.blocked(t, store)
			}
			const closers = 10
			closeResult := make(chan error, closers)
			for index := 0; index < closers; index++ {
				go func() { closeResult <- store.Close() }()
			}
			owners := 0
			for index := 0; index < closers; index++ {
				if <-closeWaiting {
					owners++
				}
			}
			if owners != 1 {
				t.Fatalf("Close owner wait paths = %d, want 1", owners)
			}
			if _, err := store.Identity(context.Background()); err != memory.ErrClosed {
				t.Fatalf("later operation = %v, want exact ErrClosed", err)
			}
			select {
			case err := <-closeResult:
				t.Fatalf("Close returned while admitted operation blocked: %v", err)
			default:
			}
			close(release)
			if err := <-operationResult; err != nil {
				t.Fatalf("admitted operation = %v", err)
			}
			<-resourcesReady
			if got := len(store.connections); got != retainedConnectionCount || store.quarantined.Load() != 0 || len(store.writeGate) != 1 {
				t.Fatalf("pre-Close accounting: connections=%d quarantined=%d gate=%d", got, store.quarantined.Load(), len(store.writeGate))
			}
			close(resourcesRelease)
			for index := 0; index < closers; index++ {
				if err := <-closeResult; err != nil {
					t.Fatalf("Close %d = %v", index, err)
				}
			}
		})
	}
}

func assertCallbackResourcesFree(t *testing.T, store *Store) {
	t.Helper()
	store.lifecycleMu.Lock()
	active := store.active
	store.lifecycleMu.Unlock()
	if active != 1 || len(store.connections) != retainedConnectionCount || len(store.writeGate) != 1 {
		t.Errorf("callback resources: active=%d connections=%d gate=%d", active, len(store.connections), len(store.writeGate))
	}
	if !store.lifecycleMu.TryLock() {
		t.Error("callback ran with lifecycle mutex held")
	} else {
		store.lifecycleMu.Unlock()
	}
}

func TestConcurrentCloseWaitsForConnectionAcquisition(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
	entered := make(chan struct{}, retainedConnectionCount+1)
	admitted := make(chan struct{}, retainedConnectionCount+1)
	release := make(chan struct{})
	closeWaiting := make(chan bool, 1)
	resourcesReady := make(chan struct{})
	resourcesRelease := make(chan struct{})
	var closeWaitOnce sync.Once
	installTestHooks(t, testHooks{
		operationAdmitted:       func() { admitted <- struct{}{} },
		afterConnectionAcquired: func() { entered <- struct{}{}; <-release },
		closeWait:               func(owner bool) { closeWaitOnce.Do(func() { closeWaiting <- owner }) },
		beforeCloseResources:    func() { close(resourcesReady); <-resourcesRelease },
	})
	results := make(chan error, retainedConnectionCount+1)
	for index := 0; index < retainedConnectionCount+1; index++ {
		go func() { _, err := store.Identity(context.Background()); results <- err }()
	}
	for index := 0; index < retainedConnectionCount+1; index++ {
		<-admitted
	}
	for index := 0; index < retainedConnectionCount; index++ {
		<-entered
	}
	select {
	case <-entered:
		t.Fatal("fifth public operation did not wait for connection acquisition")
	default:
	}
	closeResult := make(chan error, 1)
	go func() { closeResult <- store.Close() }()
	if owner := <-closeWaiting; !owner {
		t.Fatal("Close waiter was not owner")
	}
	close(release)
	for index := 0; index < retainedConnectionCount+1; index++ {
		if err := <-results; err != nil {
			t.Fatalf("admitted Identity %d = %v", index, err)
		}
	}
	<-resourcesReady
	if got := len(store.connections); got != retainedConnectionCount {
		t.Fatalf("connections before physical Close = %d", got)
	}
	close(resourcesRelease)
	if err := <-closeResult; err != nil {
		t.Fatal(err)
	}
}

func TestConcurrentCommitPoisonReturnsHealthyHolder(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path)
	holderEntered := make(chan struct{})
	holderRelease := make(chan struct{})
	var acquisitions atomic.Int64
	installTestHooks(t, testHooks{
		afterConnectionAcquired: func() {
			if acquisitions.Add(1) == 1 {
				close(holderEntered)
				<-holderRelease
			}
		},
		driverExec: func(statement string, exec func() error) error {
			if statement != "COMMIT" {
				return exec()
			}
			if err := exec(); err != nil {
				return err
			}
			return sqliteCodeError{code: 10}
		},
	})
	holderResult := make(chan error, 1)
	go func() {
		_, err := store.Identity(context.Background())
		holderResult <- err
	}()
	<-holderEntered
	record := sqliteTestRecord("concurrent-poison-record", "concurrent-poison-key", time.Date(2026, 8, 30, 0, 30, 0, 0, time.UTC))
	writerResult := make(chan error, 1)
	go func() {
		_, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: record})
		writerResult <- err
	}()
	if err := <-writerResult; !errors.Is(err, memory.ErrCommitUnknown) {
		t.Fatalf("ambiguous writer = %v", err)
	}
	if got := len(store.connections); got != retainedConnectionCount-2 {
		t.Fatalf("connections while healthy holder remains borrowed = %d, want %d", got, retainedConnectionCount-2)
	}
	if got := store.quarantined.Load(); got != 1 {
		t.Fatalf("quarantined connections = %d, want 1", got)
	}
	close(holderRelease)
	if err := <-holderResult; err != nil {
		t.Fatalf("already admitted healthy holder = %v", err)
	}
	if got := len(store.connections); got != retainedConnectionCount-1 {
		t.Fatalf("healthy connections returned after poison = %d, want %d", got, retainedConnectionCount-1)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	if stats := store.database.Stats(); stats.OpenConnections != 0 {
		t.Fatalf("physical connections after Close = %d", stats.OpenConnections)
	}
}

func TestConcurrentTwoHandleAtomicityAndAccounting(t *testing.T) {
	if testing.Short() {
		t.Skip("100-repetition concurrency stress")
	}
	directory := t.TempDir()
	if err := os.Chmod(directory, 0o700); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(directory, "memory.db")
	leftComponents, err := NewFactory(path, testOptions(t)).Open(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	rightComponents, err := NewFactory(path, testOptions(t)).Open(context.Background())
	if err != nil {
		_ = leftComponents.Store.Close()
		t.Fatal(err)
	}
	left := leftComponents.Store.(*Store)
	right := rightComponents.Store.(*Store)
	defer left.Close()
	defer right.Close()

	leftIdentity, err := left.Identity(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	rightIdentity, err := right.Identity(context.Background())
	if err != nil || leftIdentity != rightIdentity {
		t.Fatalf("identities differ: %#v %#v, %v", leftIdentity, rightIdentity, err)
	}

	for repetition := 0; repetition < 100; repetition++ {
		at := time.Date(2026, 8, 30, 1, 0, 0, repetition, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: fmt.Sprintf("two-handle-%03d", repetition)}
		key := fmt.Sprintf("shared-key-%03d", repetition)
		first := sqliteTestRecord(fmt.Sprintf("create-left-%03d", repetition), key, at)
		first.Scope = scope
		second := sqliteTestRecord(fmt.Sprintf("create-right-%03d", repetition), key, at)
		second.Scope = scope
		createErrors := runTwoAtWriteSeam(t, memory.CommitUpsert, func(ids []string) bool {
			return len(ids) == 1 && (ids[0] == first.ID || ids[0] == second.ID)
		}, nil,
			func() error {
				_, err := left.Upsert(context.Background(), memory.UpsertRequest{Record: first})
				return err
			},
			func() error {
				_, err := right.Upsert(context.Background(), memory.UpsertRequest{Record: second})
				return err
			},
		)
		assertOneSuccessOneConflict(t, "create", createErrors)
		created, err := left.GetByKey(context.Background(), memory.RecordKey{Scope: scope, Kind: first.Kind, Key: key})
		if err != nil {
			t.Fatal(err)
		}
		loserID := first.ID
		if created.ID == first.ID {
			loserID = second.ID
		} else if created.ID != second.ID {
			t.Fatalf("durable create winner = %q", created.ID)
		}
		if _, err := right.Get(context.Background(), memory.RecordRef{Scope: scope, ID: loserID}); !errors.Is(err, memory.ErrNotFound) {
			t.Fatalf("losing create %q became durable: %v", loserID, err)
		}

		expected := uint64(1)
		leftUpdate := memory.CloneRecord(created)
		leftUpdate.Text = "complete left update"
		leftUpdate.UpdatedAt = at.Add(time.Second)
		rightUpdate := memory.CloneRecord(created)
		rightUpdate.Text = "complete right update"
		rightUpdate.UpdatedAt = at.Add(time.Second)
		var updateResults [2]memory.Record
		updateErrors := runTwoAtWriteSeam(t, memory.CommitUpsert, exactWriteIDs(created.ID), func() {
			old, readErr := right.Get(context.Background(), memory.RecordRef{Scope: scope, ID: created.ID})
			if readErr != nil || !reflect.DeepEqual(old, created) {
				t.Fatalf("overlapped reader did not see complete old record: %#v, %v", old, readErr)
			}
		},
			func() error {
				var callErr error
				updateResults[0], callErr = left.Upsert(context.Background(), memory.UpsertRequest{Record: leftUpdate, ExpectedRevision: &expected})
				return callErr
			},
			func() error {
				var callErr error
				updateResults[1], callErr = right.Upsert(context.Background(), memory.UpsertRequest{Record: rightUpdate, ExpectedRevision: &expected})
				return callErr
			},
		)
		assertOneSuccessOneConflict(t, "update", updateErrors)
		winner := 0
		if updateErrors[0] != nil {
			winner = 1
		}
		if updateResults[winner].Revision != 2 {
			t.Fatalf("typed update winner = %#v", updateResults[winner])
		}
		loser := 1 - winner
		var conflict *memory.ConflictError
		if !errors.As(updateErrors[loser], &conflict) || conflict.ExpectedRevision != 1 || conflict.ActualRevision != 2 || conflict.ID != created.ID || conflict.EntityKind != "record" {
			t.Fatalf("typed update conflict = %#v, %v", conflict, updateErrors[loser])
		}
		updated, err := right.Get(context.Background(), memory.RecordRef{Scope: scope, ID: created.ID})
		if err != nil || !reflect.DeepEqual(updated, updateResults[winner]) || updated.Revision != 2 || updated.Text != leftUpdate.Text && updated.Text != rightUpdate.Text {
			t.Fatalf("complete durable updated record = %#v, winner %#v, %v", updated, updateResults[winner], err)
		}

		observationID := fmt.Sprintf("observation-%03d", repetition)
		observedLeft := sqlitePendingCandidate(fmt.Sprintf("observed-left-%03d", repetition), scope, at.Add(2*time.Second))
		observedLeft.Proposed.Source.ObservationID = observationID
		observedRight := sqlitePendingCandidate(fmt.Sprintf("observed-right-%03d", repetition), scope, at.Add(2*time.Second))
		observedRight.Proposed.Source.ObservationID = observationID
		var receipts [2]memory.ObservationReceipt
		observationErrors := runTwoAtWriteSeam(t, memory.CommitObserve, func(ids []string) bool {
			return len(ids) == 2 && ids[0] == observationID && (ids[1] == observedLeft.ID || ids[1] == observedRight.ID)
		}, nil,
			func() error {
				var callErr error
				receipts[0], callErr = left.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: observationID, Candidates: []memory.Candidate{observedLeft}, CreatedAt: at.Add(2 * time.Second)})
				return callErr
			},
			func() error {
				var callErr error
				receipts[1], callErr = right.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: observationID, Candidates: []memory.Candidate{observedRight}, CreatedAt: at.Add(2 * time.Second)})
				return callErr
			},
		)
		if observationErrors[0] != nil || observationErrors[1] != nil || receipts[0].Existing == receipts[1].Existing || len(receipts[0].CandidateIDs) != 1 || fmt.Sprint(receipts[0].CandidateIDs) != fmt.Sprint(receipts[1].CandidateIDs) {
			t.Fatalf("observation convergence = %#v %#v, errors %v", receipts[0], receipts[1], observationErrors)
		}
		observationWinner := observedLeft
		observationLoser := observedRight
		if receipts[0].CandidateIDs[0] == observedRight.ID {
			observationWinner, observationLoser = observedRight, observedLeft
		} else if receipts[0].CandidateIDs[0] != observedLeft.ID {
			t.Fatalf("observation winner ID = %q", receipts[0].CandidateIDs[0])
		}
		if got, err := left.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: observationWinner.ID}); err != nil || got.ID != observationWinner.ID {
			t.Fatalf("durable observation winner = %#v, %v", got, err)
		}
		if _, err := right.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: observationLoser.ID}); !errors.Is(err, memory.ErrNotFound) {
			t.Fatalf("observation loser became durable: %v", err)
		}

		reviewCandidate := sqlitePendingCandidate(fmt.Sprintf("review-%03d", repetition), scope, at.Add(3*time.Second))
		if _, err := left.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{reviewCandidate}}); err != nil {
			t.Fatal(err)
		}
		review := memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: reviewCandidate.ID}, Decision: memory.ReviewReject, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(4 * time.Second)}
		beforeReview, err := left.Identity(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		var reviewResults [2]memory.ReviewResult
		reviewErrors := runTwoAtWriteSeam(t, memory.CommitReview, exactWriteIDs(reviewCandidate.ID), nil,
			func() error {
				var callErr error
				reviewResults[0], callErr = left.Review(context.Background(), review)
				return callErr
			},
			func() error {
				var callErr error
				reviewResults[1], callErr = right.Review(context.Background(), review)
				return callErr
			},
		)
		assertOneSuccessOneConflict(t, "review", reviewErrors)
		reviewWinner := 0
		if reviewErrors[0] != nil {
			reviewWinner = 1
		}
		if reviewResults[reviewWinner].Candidate.State != memory.CandidateRejected || reviewResults[reviewWinner].Record != nil || reviewResults[reviewWinner].Tombstone != nil {
			t.Fatalf("review decision = %#v", reviewResults[reviewWinner])
		}
		durableCandidate, err := right.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: reviewCandidate.ID})
		if err != nil || durableCandidate.State != memory.CandidateRejected || durableCandidate.DecisionSource != memory.OriginHuman || durableCandidate.DecidedAt == nil || !durableCandidate.DecidedAt.Equal(review.DecidedAt) {
			t.Fatalf("durable review target state = %#v, %v", durableCandidate, err)
		}
		afterReview, err := right.Identity(context.Background())
		if err != nil || afterReview.Generation != beforeReview.Generation+1 {
			t.Fatalf("review generation = %d -> %d, %v", beforeReview.Generation, afterReview.Generation, err)
		}

		for name, store := range map[string]*Store{"left": left, "right": right} {
			if got := len(store.connections); got != retainedConnectionCount {
				t.Fatalf("repetition %d %s connections = %d, want %d", repetition, name, got, retainedConnectionCount)
			}
		}
	}

	if err := left.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := right.Identity(context.Background()); err != nil {
		t.Fatalf("closing left closed right: %v", err)
	}
}

func exactWriteIDs(ids ...string) func([]string) bool {
	return func(got []string) bool { return reflect.DeepEqual(got, ids) }
}

func runTwoAtWriteSeam(
	t *testing.T,
	operation memory.CommitOperation,
	matchIDs func([]string) bool,
	whileBlocked func(),
	left, right func() error,
) [2]error {
	t.Helper()
	arrived := make(chan struct{}, 2)
	release := make(chan struct{})
	released := false
	defer func() {
		if !released {
			close(release)
		}
	}()
	var matched int
	var matchMu sync.Mutex
	restoreHooks := setTestHooks(testHooks{beforeWriteBegin: func(gotOperation memory.CommitOperation, ids []string) {
		if gotOperation != operation || !matchIDs(ids) {
			return
		}
		matchMu.Lock()
		if matched >= 2 {
			matchMu.Unlock()
			return
		}
		matched++
		matchMu.Unlock()
		arrived <- struct{}{}
		<-release
	}})
	defer restoreHooks()

	var result [2]error
	var wait sync.WaitGroup
	wait.Add(2)
	go func() { defer wait.Done(); result[0] = left() }()
	go func() { defer wait.Done(); result[1] = right() }()
	<-arrived
	<-arrived
	if whileBlocked != nil {
		whileBlocked()
	}
	close(release)
	released = true
	wait.Wait()
	return result
}

func assertOneSuccessOneConflict(t *testing.T, operation string, result [2]error) {
	t.Helper()
	successes, conflicts := 0, 0
	for _, err := range result {
		switch {
		case err == nil:
			successes++
		case errors.Is(err, memory.ErrConflict):
			conflicts++
		default:
			t.Fatalf("%s error = %v", operation, err)
		}
	}
	if successes != 1 || conflicts != 1 {
		t.Fatalf("%s outcomes = %v", operation, result)
	}
}
