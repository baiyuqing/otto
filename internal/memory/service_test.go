package memory_test

import (
	"context"
	"errors"
	"path/filepath"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/memory/sqlite"
)

type funcGuard func(context.Context, memory.GuardInput) error

func (f funcGuard) Check(ctx context.Context, input memory.GuardInput) error { return f(ctx, input) }

type funcExtractor func(context.Context, memory.ExtractRequest) ([]memory.Proposal, error)

func (f funcExtractor) Extract(ctx context.Context, request memory.ExtractRequest) ([]memory.Proposal, error) {
	return f(ctx, request)
}

type funcPolicy func(context.Context, memory.PolicyRequest) (memory.PolicyDecision, error)

func (f funcPolicy) Decide(ctx context.Context, request memory.PolicyRequest) (memory.PolicyDecision, error) {
	return f(ctx, request)
}

func newTestStore(t *testing.T) *sqlite.Store {
	t.Helper()
	path := filepath.Join(t.TempDir(), "memory", "memory.db")
	store, err := sqlite.Open(context.Background(), path, sqlite.Options{
		Guard: memory.NewCompositeGuard(memory.DefaultGuard{}),
	})
	if err != nil {
		t.Fatalf("sqlite.Open: %v", err)
	}
	t.Cleanup(func() { _ = store.Close() })
	return store
}

func newTestService(t *testing.T) memory.Service {
	t.Helper()
	store := newTestStore(t)
	svc, err := memory.NewService(store, store, memory.DefaultPolicy{})
	if err != nil {
		t.Fatalf("NewService: %v", err)
	}
	t.Cleanup(func() { _ = svc.Close() })
	return svc
}

func TestNewServiceRejectsNilDependencies(t *testing.T) {
	store := newTestStore(t)
	if _, err := memory.NewService(nil, store, memory.DefaultPolicy{}); !errors.Is(err, memory.ErrInvalidRequest) {
		t.Fatalf("nil store error = %v, want ErrInvalidRequest", err)
	}
	if _, err := memory.NewService(store, nil, memory.DefaultPolicy{}); !errors.Is(err, memory.ErrInvalidRequest) {
		t.Fatalf("nil retriever error = %v, want ErrInvalidRequest", err)
	}
	if _, err := memory.NewService(store, store, nil); !errors.Is(err, memory.ErrInvalidRequest) {
		t.Fatalf("nil policy error = %v, want ErrInvalidRequest", err)
	}
}

func TestServiceRememberCreateAndUpdate(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	created, err := svc.Remember(ctx, memory.RememberRequest{Scope: userScope, Kind: "preference", Key: "editor", Text: "vim"})
	if err != nil {
		t.Fatalf("Remember create: %v", err)
	}
	if created.ID == "" || created.Revision != 1 || created.Source.Origin != memory.OriginHuman {
		t.Fatalf("created record = %+v", created)
	}

	rev := created.Revision
	updated, err := svc.Remember(ctx, memory.RememberRequest{
		ID: created.ID, Scope: userScope, Kind: "preference", Key: "editor", Text: "neovim", ExpectedRevision: &rev,
	})
	if err != nil {
		t.Fatalf("Remember update: %v", err)
	}
	if updated.Revision != 2 || updated.Text != "neovim" || !updated.CreatedAt.Equal(created.CreatedAt) {
		t.Fatalf("updated record = %+v", updated)
	}

	rev2 := updated.Revision
	updatedByKey, err := svc.Remember(ctx, memory.RememberRequest{
		Scope: userScope, Kind: "preference", Key: "editor", Text: "emacs", ExpectedRevision: &rev2,
	})
	if err != nil {
		t.Fatalf("Remember update by key: %v", err)
	}
	if updatedByKey.ID != created.ID || updatedByKey.Text != "emacs" {
		t.Fatalf("updatedByKey record = %+v", updatedByKey)
	}
}

func TestServiceRememberWithKeyAndNoExpectedRevisionUpdatesExistingRecord(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	created, err := svc.Remember(ctx, memory.RememberRequest{Scope: userScope, Kind: "preference", Key: "editor", Text: "vim"})
	if err != nil {
		t.Fatalf("Remember create: %v", err)
	}

	again, err := svc.Remember(ctx, memory.RememberRequest{Scope: userScope, Kind: "preference", Key: "editor", Text: "neovim"})
	if err != nil {
		t.Fatalf("Remember again: %v", err)
	}
	if again.ID != created.ID {
		t.Fatalf("Remember again created a new record: got ID %q, want existing ID %q", again.ID, created.ID)
	}
	if again.Revision != 2 || again.Text != "neovim" || !again.CreatedAt.Equal(created.CreatedAt) {
		t.Fatalf("updated record = %+v", again)
	}

	list, err := svc.Search(ctx, memory.SearchRequest{Scopes: []memory.Scope{userScope}, Kinds: []string{"preference"}, Limit: 10, TokenBudget: 1000, Now: time.Now().UTC()})
	if err != nil {
		t.Fatalf("Search: %v", err)
	}
	if len(list.Records) != 1 {
		t.Fatalf("records = %#v, want exactly one (no duplicate)", list.Records)
	}
}

func TestServiceRememberRejectsSensitiveContent(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}

	_, err := svc.Remember(context.Background(), memory.RememberRequest{
		Scope: userScope, Kind: "secret", Text: "api_key: sk-abcdef123456",
	})
	if !errors.Is(err, memory.ErrSensitiveMemory) {
		t.Fatalf("error = %v, want ErrSensitiveMemory", err)
	}
}

func TestServiceForget(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	created, err := svc.Remember(ctx, memory.RememberRequest{Scope: userScope, Kind: "preference", Text: "vim"})
	if err != nil {
		t.Fatalf("Remember: %v", err)
	}

	result, err := svc.Forget(ctx, memory.ForgetRequest{Ref: memory.RecordRef{Scope: userScope, ID: created.ID}, ExpectedRevision: created.Revision})
	if err != nil {
		t.Fatalf("Forget: %v", err)
	}
	if result.Tombstone.ID != created.ID {
		t.Fatalf("tombstone = %+v", result.Tombstone)
	}

	if _, err := svc.Get(ctx, memory.RecordRef{Scope: userScope, ID: created.ID}); !errors.Is(err, memory.ErrNotFound) {
		t.Fatalf("Get after forget error = %v, want ErrNotFound", err)
	}
	if _, err := svc.GetTombstone(ctx, memory.RecordRef{Scope: userScope, ID: created.ID}); err != nil {
		t.Fatalf("GetTombstone: %v", err)
	}
}

func TestServiceForgetPurgeBackupsUnsupported(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	created, err := svc.Remember(ctx, memory.RememberRequest{Scope: userScope, Kind: "preference", Text: "vim"})
	if err != nil {
		t.Fatalf("Remember: %v", err)
	}

	_, err = svc.Forget(ctx, memory.ForgetRequest{
		Ref: memory.RecordRef{Scope: userScope, ID: created.ID}, ExpectedRevision: created.Revision,
		PurgeBackups: true, ConfirmPurge: true,
	})
	if !errors.Is(err, memory.ErrUnsupported) {
		t.Fatalf("error = %v, want ErrUnsupported", err)
	}
}

func TestServiceProposeRejectsNonPendingOrigin(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}

	_, err := svc.Propose(context.Background(), memory.ProposeRequest{
		Action: memory.CandidateCreate, Scope: userScope, Kind: "preference", Text: "vim", Source: memory.Provenance{Origin: memory.OriginHuman},
	})
	if !errors.Is(err, memory.ErrInvalidRequest) {
		t.Fatalf("error = %v, want ErrInvalidRequest", err)
	}
}

func TestServiceProposeReturnsErrorWhenPolicyDoesNotPend(t *testing.T) {
	store := newTestStore(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}

	for _, decision := range []memory.PolicyDecision{memory.PolicyReject, memory.PolicyAccept} {
		policy := funcPolicy(func(context.Context, memory.PolicyRequest) (memory.PolicyDecision, error) {
			return decision, nil
		})
		svc, err := memory.NewService(store, store, policy)
		if err != nil {
			t.Fatalf("NewService: %v", err)
		}

		batch, err := svc.Propose(context.Background(), memory.ProposeRequest{
			Action: memory.CandidateCreate, Scope: userScope, Kind: "preference", Text: "vim", Source: memory.Provenance{Origin: memory.OriginModel},
		})
		if err == nil {
			t.Fatalf("decision %q: expected error, got batch=%+v", decision, batch)
		}
		if len(batch.Candidates) != 0 {
			t.Fatalf("decision %q: batch = %+v, want no candidates", decision, batch)
		}
	}
}

func TestServiceProposeAndReviewAcceptCreate(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	batch, err := svc.Propose(ctx, memory.ProposeRequest{
		Action: memory.CandidateCreate, Scope: userScope, Kind: "preference", Key: "editor", Text: "vim", Source: memory.Provenance{Origin: memory.OriginModel},
	})
	if err != nil {
		t.Fatalf("Propose: %v", err)
	}
	if len(batch.Candidates) != 1 || batch.Candidates[0].State != memory.CandidatePending {
		t.Fatalf("batch = %+v", batch)
	}
	candidateID := batch.Candidates[0].ID

	result, err := svc.Review(ctx, memory.ReviewRequest{Ref: memory.CandidateRef{Scope: userScope, ID: candidateID}, Decision: memory.ReviewAccept})
	if err != nil {
		t.Fatalf("Review: %v", err)
	}
	if result.Record == nil || result.Record.Text != "vim" {
		t.Fatalf("review result = %+v", result)
	}

	fetched, err := svc.Get(ctx, memory.RecordRef{Scope: userScope, ID: result.Record.ID})
	if err != nil {
		t.Fatalf("Get: %v", err)
	}
	if fetched.Text != "vim" {
		t.Fatalf("fetched = %+v", fetched)
	}
}

func TestServiceProposeAndReviewReject(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	batch, err := svc.Propose(ctx, memory.ProposeRequest{
		Action: memory.CandidateCreate, Scope: userScope, Kind: "preference", Text: "vim", Source: memory.Provenance{Origin: memory.OriginModel},
	})
	if err != nil {
		t.Fatalf("Propose: %v", err)
	}
	candidateID := batch.Candidates[0].ID

	result, err := svc.Review(ctx, memory.ReviewRequest{Ref: memory.CandidateRef{Scope: userScope, ID: candidateID}, Decision: memory.ReviewReject})
	if err != nil {
		t.Fatalf("Review: %v", err)
	}
	if result.Candidate.State != memory.CandidateRejected {
		t.Fatalf("candidate = %+v", result.Candidate)
	}
	if result.Record != nil {
		t.Fatalf("record = %+v, want nil", result.Record)
	}
}

func TestServiceProposeUpdateAndReviewAccept(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	created, err := svc.Remember(ctx, memory.RememberRequest{Scope: userScope, Kind: "preference", Key: "editor", Text: "vim"})
	if err != nil {
		t.Fatalf("Remember: %v", err)
	}

	batch, err := svc.Propose(ctx, memory.ProposeRequest{
		Action: memory.CandidateUpdate, Scope: userScope, Kind: "preference", Key: "editor", Text: "neovim",
		TargetID: created.ID, BaseRevision: created.Revision, Source: memory.Provenance{Origin: memory.OriginModel},
	})
	if err != nil {
		t.Fatalf("Propose update: %v", err)
	}
	candidateID := batch.Candidates[0].ID

	result, err := svc.Review(ctx, memory.ReviewRequest{Ref: memory.CandidateRef{Scope: userScope, ID: candidateID}, Decision: memory.ReviewAccept})
	if err != nil {
		t.Fatalf("Review: %v", err)
	}
	if result.Record == nil || result.Record.ID != created.ID || result.Record.Text != "neovim" {
		t.Fatalf("review result = %+v", result)
	}
}

func TestServiceSearch(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	created, err := svc.Remember(ctx, memory.RememberRequest{Scope: userScope, Kind: "preference", Key: "editor", Text: "prefers vim for editing"})
	if err != nil {
		t.Fatalf("Remember: %v", err)
	}

	now := time.Now().UTC().Round(0)
	listed, err := svc.Search(ctx, memory.SearchRequest{Scopes: []memory.Scope{userScope}, Limit: 10, TokenBudget: 1000, Now: now})
	if err != nil {
		t.Fatalf("empty-query Search: %v", err)
	}
	if len(listed.Records) != 1 || listed.Records[0].ID != created.ID {
		t.Fatalf("listed records = %+v", listed.Records)
	}

	searched, err := svc.Search(ctx, memory.SearchRequest{Query: "vim", Scopes: []memory.Scope{userScope}, Limit: 10, TokenBudget: 1000, Now: now})
	if err != nil {
		t.Fatalf("text Search: %v", err)
	}
	if len(searched.Records) != 1 || searched.Records[0].ID != created.ID {
		t.Fatalf("searched records = %+v", searched.Records)
	}

	if _, err := svc.Propose(ctx, memory.ProposeRequest{
		Action: memory.CandidateCreate, Scope: userScope, Kind: "preference", Text: "loves emacs", Source: memory.Provenance{Origin: memory.OriginModel},
	}); err != nil {
		t.Fatalf("Propose: %v", err)
	}

	withCandidates, err := svc.Search(ctx, memory.SearchRequest{
		Scopes: []memory.Scope{userScope}, Limit: 10, TokenBudget: 1000, Now: now,
		IncludeCandidates: true, CandidateStates: []memory.CandidateState{memory.CandidatePending},
	})
	if err != nil {
		t.Fatalf("Search with candidates: %v", err)
	}
	if len(withCandidates.Candidates) != 1 {
		t.Fatalf("candidates = %+v", withCandidates.Candidates)
	}
}

func TestServiceGetNotFound(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}

	if _, err := svc.Get(context.Background(), memory.RecordRef{Scope: userScope, ID: "0123456789abcdef"}); !errors.Is(err, memory.ErrNotFound) {
		t.Fatalf("Get error = %v, want ErrNotFound", err)
	}
}

func TestServiceBindRecall(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	workspaceScope := memory.Scope{Namespace: memory.NamespaceWorkspace, ID: "workspace-1"}
	ctx := context.Background()

	created, err := svc.Remember(ctx, memory.RememberRequest{Scope: userScope, Kind: "preference", Key: "editor", Text: "prefers vim for editing code"})
	if err != nil {
		t.Fatalf("Remember: %v", err)
	}

	binding, err := svc.Bind(ctx, memory.BindOptions{Scopes: []memory.Scope{userScope, workspaceScope}, DefaultWriteScope: workspaceScope})
	if err != nil {
		t.Fatalf("Bind: %v", err)
	}
	t.Cleanup(func() { _ = binding.Close() })

	result, err := binding.Recall(ctx, memory.RecallRequest{Query: "vim", Limit: 10, TokenBudget: 1000})
	if err != nil {
		t.Fatalf("Recall: %v", err)
	}
	if len(result.Records) != 1 || result.Records[0].ID != created.ID {
		t.Fatalf("Recall records = %+v", result.Records)
	}
}

func TestServiceBindObserveIdempotent(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	binding, err := svc.Bind(ctx, memory.BindOptions{Scopes: []memory.Scope{userScope}, DefaultWriteScope: userScope})
	if err != nil {
		t.Fatalf("Bind: %v", err)
	}
	t.Cleanup(func() { _ = binding.Close() })

	observation := memory.Observation{ID: "observation-1", UserText: "hello", AssistantText: "hi"}
	first, err := binding.Observe(ctx, observation)
	if err != nil {
		t.Fatalf("first Observe: %v", err)
	}
	if first.Existing || len(first.CandidateIDs) != 0 {
		t.Fatalf("first Observe = %+v, want fresh empty receipt", first)
	}

	second, err := binding.Observe(ctx, observation)
	if err != nil {
		t.Fatalf("second Observe: %v", err)
	}
	if !second.Existing || len(second.CandidateIDs) != 0 {
		t.Fatalf("second Observe = %+v, want existing empty receipt", second)
	}
}

func TestServiceBindObserveGuardsBeforeExtraction(t *testing.T) {
	svc := newTestService(t)
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	guardCalled := false
	guard := funcGuard(func(context.Context, memory.GuardInput) error {
		guardCalled = true
		return nil
	})
	extractorCalled := false
	extractor := funcExtractor(func(context.Context, memory.ExtractRequest) ([]memory.Proposal, error) {
		extractorCalled = true
		return nil, nil
	})

	binding, err := svc.Bind(ctx, memory.BindOptions{Scopes: []memory.Scope{userScope}, DefaultWriteScope: userScope, Guard: guard, Extractor: extractor})
	if err != nil {
		t.Fatalf("Bind: %v", err)
	}
	t.Cleanup(func() { _ = binding.Close() })

	if _, err := binding.Observe(ctx, memory.Observation{ID: "obs-1", UserText: "hello"}); err != nil {
		t.Fatalf("Observe: %v", err)
	}
	if !guardCalled {
		t.Fatal("binding guard was not invoked before extraction")
	}
	if !extractorCalled {
		t.Fatal("extractor was not invoked")
	}
}

func TestServiceCloseClosesStoreAndInvalidatesBindings(t *testing.T) {
	store := newTestStore(t)
	svc, err := memory.NewService(store, store, memory.DefaultPolicy{})
	if err != nil {
		t.Fatalf("NewService: %v", err)
	}
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	binding, err := svc.Bind(ctx, memory.BindOptions{Scopes: []memory.Scope{userScope}, DefaultWriteScope: userScope})
	if err != nil {
		t.Fatalf("Bind: %v", err)
	}

	if err := svc.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}
	if err := svc.Close(); err != nil {
		t.Fatalf("second Close: %v", err)
	}

	if _, err := svc.Get(ctx, memory.RecordRef{Scope: userScope, ID: "0123456789abcdef"}); !errors.Is(err, memory.ErrClosed) {
		t.Fatalf("Get after close error = %v, want ErrClosed", err)
	}
	if _, err := svc.Bind(ctx, memory.BindOptions{Scopes: []memory.Scope{userScope}, DefaultWriteScope: userScope}); !errors.Is(err, memory.ErrClosed) {
		t.Fatalf("Bind after close error = %v, want ErrClosed", err)
	}
	if _, err := binding.Recall(ctx, memory.RecallRequest{Query: "x", Limit: 1, TokenBudget: 100}); !errors.Is(err, memory.ErrClosed) {
		t.Fatalf("Recall after service close error = %v, want ErrClosed", err)
	}
}

type blockingRetriever struct {
	started chan struct{}
	release chan struct{}
}

func (r *blockingRetriever) Retrieve(context.Context, memory.RetrievalRequest) (memory.RetrievalResult, error) {
	close(r.started)
	<-r.release
	return memory.RetrievalResult{}, nil
}

func TestServiceCloseWaitsForInFlightBindingOperation(t *testing.T) {
	store := newTestStore(t)
	retriever := &blockingRetriever{started: make(chan struct{}), release: make(chan struct{})}
	svc, err := memory.NewService(store, retriever, memory.DefaultPolicy{})
	if err != nil {
		t.Fatalf("NewService: %v", err)
	}
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	bound, err := svc.Bind(ctx, memory.BindOptions{Scopes: []memory.Scope{userScope}, DefaultWriteScope: userScope})
	if err != nil {
		t.Fatalf("Bind: %v", err)
	}

	recallDone := make(chan error, 1)
	go func() {
		_, err := bound.Recall(ctx, memory.RecallRequest{Query: "x", Limit: 1, TokenBudget: 100})
		recallDone <- err
	}()
	<-retriever.started

	closeDone := make(chan error, 1)
	go func() { closeDone <- svc.Close() }()

	select {
	case err := <-closeDone:
		t.Fatalf("Close returned while a binding operation was still in flight: %v", err)
	case <-time.After(50 * time.Millisecond):
	}

	close(retriever.release)

	if err := <-recallDone; err != nil {
		t.Fatalf("Recall: %v", err)
	}
	select {
	case err := <-closeDone:
		if err != nil {
			t.Fatalf("Close: %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("Close did not return after the in-flight operation finished")
	}
}

// TestServiceCloseWaitsForInFlightManagerOperation covers the Manager and
// Proposer methods called directly on *service (by app.Controller's memory
// facade, the standalone "otto memory" CLI, and the agent's memory tools),
// as opposed to TestServiceCloseWaitsForInFlightBindingOperation above,
// which only covers Binding.Recall/Observe.
func TestServiceCloseWaitsForInFlightManagerOperation(t *testing.T) {
	store := newTestStore(t)
	retriever := &blockingRetriever{started: make(chan struct{}), release: make(chan struct{})}
	svc, err := memory.NewService(store, retriever, memory.DefaultPolicy{})
	if err != nil {
		t.Fatalf("NewService: %v", err)
	}
	userScope := memory.Scope{Namespace: memory.NamespaceUser, ID: "user-1"}
	ctx := context.Background()

	searchDone := make(chan error, 1)
	go func() {
		_, err := svc.Search(ctx, memory.SearchRequest{Query: "x", Scopes: []memory.Scope{userScope}, Limit: 1, TokenBudget: 100, Now: time.Now().UTC()})
		searchDone <- err
	}()
	<-retriever.started

	closeDone := make(chan error, 1)
	go func() { closeDone <- svc.Close() }()

	select {
	case err := <-closeDone:
		t.Fatalf("Close returned while a manager operation was still in flight: %v", err)
	case <-time.After(50 * time.Millisecond):
	}

	close(retriever.release)

	if err := <-searchDone; err != nil {
		t.Fatalf("Search: %v", err)
	}
	select {
	case err := <-closeDone:
		if err != nil {
			t.Fatalf("Close: %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("Close did not return after the in-flight operation finished")
	}
}
