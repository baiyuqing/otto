package sqlite

import (
	"context"
	"errors"
	"fmt"
	"path/filepath"
	"runtime"
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

	// Synchronize on the lifecycle transition rather than elapsed time.
	var state storeState
	for {
		store.lifecycleMu.Lock()
		state = store.state
		store.lifecycleMu.Unlock()
		if state != storeOpen {
			break
		}
		runtime.Gosched()
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
	for index := 0; index < closers; index++ {
		if err := <-results; err != nil {
			t.Fatalf("Close %d = %v", index, err)
		}
	}
}

func TestConcurrentTwoHandleAtomicityAndAccounting(t *testing.T) {
	if testing.Short() {
		t.Skip("100-repetition concurrency stress")
	}
	path := filepath.Join(t.TempDir(), "memory.db")
	left := openTestStore(t, path)
	right, err := Open(context.Background(), path, testOptions(t))
	if err != nil {
		t.Fatal(err)
	}
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
		createErrors := runTwo(t,
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

		expected := uint64(1)
		leftUpdate := memory.CloneRecord(created)
		leftUpdate.Text = "complete left update"
		leftUpdate.UpdatedAt = at.Add(time.Second)
		rightUpdate := memory.CloneRecord(created)
		rightUpdate.Text = "complete right update"
		rightUpdate.UpdatedAt = at.Add(time.Second)
		updateErrors := runTwo(t,
			func() error {
				_, err := left.Upsert(context.Background(), memory.UpsertRequest{Record: leftUpdate, ExpectedRevision: &expected})
				return err
			},
			func() error {
				_, err := right.Upsert(context.Background(), memory.UpsertRequest{Record: rightUpdate, ExpectedRevision: &expected})
				return err
			},
		)
		assertOneSuccessOneConflict(t, "update", updateErrors)
		updated, err := right.Get(context.Background(), memory.RecordRef{Scope: scope, ID: created.ID})
		if err != nil || updated.Revision != 2 || updated.Text != leftUpdate.Text && updated.Text != rightUpdate.Text {
			t.Fatalf("complete updated record = %#v, %v", updated, err)
		}

		observationID := fmt.Sprintf("observation-%03d", repetition)
		observedLeft := sqlitePendingCandidate(fmt.Sprintf("observed-left-%03d", repetition), scope, at.Add(2*time.Second))
		observedLeft.Proposed.Source.ObservationID = observationID
		observedRight := sqlitePendingCandidate(fmt.Sprintf("observed-right-%03d", repetition), scope, at.Add(2*time.Second))
		observedRight.Proposed.Source.ObservationID = observationID
		var receipts [2]memory.ObservationReceipt
		observationErrors := runTwo(t,
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

		reviewCandidate := sqlitePendingCandidate(fmt.Sprintf("review-%03d", repetition), scope, at.Add(3*time.Second))
		if _, err := left.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{reviewCandidate}}); err != nil {
			t.Fatal(err)
		}
		review := memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: reviewCandidate.ID}, Decision: memory.ReviewReject, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(4 * time.Second)}
		reviewErrors := runTwo(t,
			func() error { _, err := left.Review(context.Background(), review); return err },
			func() error { _, err := right.Review(context.Background(), review); return err },
		)
		assertOneSuccessOneConflict(t, "review", reviewErrors)

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

func runTwo(t *testing.T, left, right func() error) [2]error {
	t.Helper()
	start := make(chan struct{})
	var result [2]error
	var wait sync.WaitGroup
	wait.Add(2)
	go func() { defer wait.Done(); <-start; result[0] = left() }()
	go func() { defer wait.Done(); <-start; result[1] = right() }()
	close(start)
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
