package memory

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"sync"
	"testing"
	"time"
)

func nullValidBindOptions() BindOptions {
	scope := Scope{Namespace: NamespaceUser, ID: "user-1"}
	return BindOptions{
		Scopes:            []Scope{scope},
		DefaultWriteScope: scope,
		Extractor: testExtractor(func(context.Context, ExtractRequest) ([]Proposal, error) {
			panic("NullService invoked Extractor")
		}),
		Guard: testContentGuard(func(context.Context, GuardInput) error {
			panic("NullService invoked ContentGuard")
		}),
		EstimateTokens: func(string) int { panic("NullService invoked EstimateTokens") },
		Now:            func() time.Time { panic("NullService invoked Now") },
	}
}

type testExtractor func(context.Context, ExtractRequest) ([]Proposal, error)

func (f testExtractor) Extract(ctx context.Context, request ExtractRequest) ([]Proposal, error) {
	return f(ctx, request)
}

type testContentGuard func(context.Context, GuardInput) error

func (f testContentGuard) Check(ctx context.Context, input GuardInput) error {
	return f(ctx, input)
}

func TestNullServiceBindValidationAndIndependentBindings(t *testing.T) {
	service := NewNullService(nil)
	t.Cleanup(func() { _ = service.Close() })

	if binding, err := service.Bind(context.Background(), BindOptions{}); !errors.Is(err, ErrInvalidRequest) || binding != nil {
		t.Fatalf("invalid Bind = (%v, %v), want (nil, ErrInvalidRequest)", binding, err)
	}

	first, err := service.Bind(context.Background(), nullValidBindOptions())
	if err != nil {
		t.Fatalf("first Bind: %v", err)
	}
	second, err := service.Bind(context.Background(), nullValidBindOptions())
	if err != nil {
		t.Fatalf("second Bind: %v", err)
	}
	if first == second {
		t.Fatal("Bind returned the same Binding instance")
	}

	if err := first.Close(); err != nil {
		t.Fatalf("first Close: %v", err)
	}
	if err := first.Close(); err != nil {
		t.Fatalf("second first.Close: %v", err)
	}
	if _, err := first.Recall(context.Background(), RecallRequest{}); !errors.Is(err, ErrClosed) {
		t.Fatalf("closed first Recall error = %v, want ErrClosed", err)
	}
	if _, err := second.Recall(context.Background(), RecallRequest{}); err != nil {
		t.Fatalf("closing first binding affected second: %v", err)
	}
}

func TestNullServiceBindingEmptyResultsUseIndependentSlices(t *testing.T) {
	service := NewNullService(ErrDisabled)
	t.Cleanup(func() { _ = service.Close() })
	binding, err := service.Bind(context.Background(), nullValidBindOptions())
	if err != nil {
		t.Fatal(err)
	}

	firstRecall, err := binding.Recall(context.Background(), RecallRequest{})
	if err != nil {
		t.Fatalf("first Recall: %v", err)
	}
	secondRecall, err := binding.Recall(context.Background(), RecallRequest{})
	if err != nil {
		t.Fatalf("second Recall: %v", err)
	}
	if firstRecall.Records == nil || secondRecall.Records == nil || len(firstRecall.Records) != 0 || len(secondRecall.Records) != 0 {
		t.Fatalf("Recall slices = (%#v, %#v), want non-nil empty slices", firstRecall.Records, secondRecall.Records)
	}
	firstRecall.Records = append(firstRecall.Records, Record{ID: "changed"})
	if len(secondRecall.Records) != 0 {
		t.Fatal("Recall results alias each other")
	}

	firstObserve, err := binding.Observe(context.Background(), Observation{})
	if err != nil {
		t.Fatalf("first Observe: %v", err)
	}
	secondObserve, err := binding.Observe(context.Background(), Observation{})
	if err != nil {
		t.Fatalf("second Observe: %v", err)
	}
	if firstObserve.CandidateIDs == nil || secondObserve.CandidateIDs == nil || len(firstObserve.CandidateIDs) != 0 || len(secondObserve.CandidateIDs) != 0 {
		t.Fatalf("Observe slices = (%#v, %#v), want non-nil empty slices", firstObserve.CandidateIDs, secondObserve.CandidateIDs)
	}
	firstObserve.CandidateIDs = append(firstObserve.CandidateIDs, "changed")
	if len(secondObserve.CandidateIDs) != 0 {
		t.Fatal("Observe results alias each other")
	}
}

type nullServiceCall struct {
	name string
	call func(Service, context.Context) error
}

func nullServiceCalls() []nullServiceCall {
	return []nullServiceCall{
		{"Get", func(s Service, ctx context.Context) error { _, err := s.Get(ctx, RecordRef{}); return err }},
		{"GetByKey", func(s Service, ctx context.Context) error { _, err := s.GetByKey(ctx, RecordKey{}); return err }},
		{"GetTombstone", func(s Service, ctx context.Context) error { _, err := s.GetTombstone(ctx, RecordRef{}); return err }},
		{"GetCandidate", func(s Service, ctx context.Context) error { _, err := s.GetCandidate(ctx, CandidateRef{}); return err }},
		{"Search", func(s Service, ctx context.Context) error { _, err := s.Search(ctx, SearchRequest{}); return err }},
		{"Remember", func(s Service, ctx context.Context) error { _, err := s.Remember(ctx, RememberRequest{}); return err }},
		{"Forget", func(s Service, ctx context.Context) error { _, err := s.Forget(ctx, ForgetRequest{}); return err }},
		{"Review", func(s Service, ctx context.Context) error { _, err := s.Review(ctx, ReviewRequest{}); return err }},
		{"Propose", func(s Service, ctx context.Context) error { _, err := s.Propose(ctx, ProposeRequest{}); return err }},
	}
}

func TestNullServiceSanitizesConstructorReasonForCanonicalMethods(t *testing.T) {
	tests := []struct {
		name   string
		reason error
		want   error
	}{
		{"nil", nil, ErrDisabled},
		{"disabled", ErrDisabled, ErrDisabled},
		{"wrapped disabled", fmt.Errorf("LEAK-ME: %w", ErrDisabled), ErrDisabled},
		{"unavailable", ErrUnavailable, ErrUnavailable},
		{"wrapped unavailable", fmt.Errorf("LEAK-ME: %w", ErrUnavailable), ErrUnavailable},
		{"unknown", errors.New("LEAK-ME"), ErrUnavailable},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			service := NewNullService(tt.reason)
			t.Cleanup(func() { _ = service.Close() })
			for _, operation := range nullServiceCalls() {
				t.Run(operation.name, func(t *testing.T) {
					err := operation.call(service, context.Background())
					if !errors.Is(err, tt.want) {
						t.Fatalf("error = %v, want %v", err, tt.want)
					}
					if err != tt.want || strings.Contains(err.Error(), "LEAK-ME") {
						t.Fatalf("unsanitized error = %q, want exact category %q", err, tt.want)
					}
				})
			}
		})
	}
}

func TestNullServiceCloseInvalidatesServiceAndBindings(t *testing.T) {
	service := NewNullService(ErrUnavailable)
	binding, err := service.Bind(context.Background(), nullValidBindOptions())
	if err != nil {
		t.Fatal(err)
	}

	if err := service.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}
	if err := service.Close(); err != nil {
		t.Fatalf("second Close: %v", err)
	}
	if _, err := service.Bind(context.Background(), nullValidBindOptions()); !errors.Is(err, ErrClosed) {
		t.Fatalf("Bind after close error = %v, want ErrClosed", err)
	}
	if _, err := binding.Recall(context.Background(), RecallRequest{}); !errors.Is(err, ErrClosed) {
		t.Fatalf("Recall after parent close error = %v, want ErrClosed", err)
	}
	if _, err := binding.Observe(context.Background(), Observation{}); !errors.Is(err, ErrClosed) {
		t.Fatalf("Observe after parent close error = %v, want ErrClosed", err)
	}
	for _, operation := range nullServiceCalls() {
		if err := operation.call(service, context.Background()); !errors.Is(err, ErrClosed) {
			t.Errorf("%s after close error = %v, want ErrClosed", operation.name, err)
		}
	}
	if err := binding.Close(); err != nil {
		t.Fatalf("Binding.Close after parent close: %v", err)
	}
}

func TestNullServiceCancellationPrecedesCategoryAndClose(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	service := NewNullService(errors.New("LEAK-ME"))
	binding, err := service.Bind(context.Background(), nullValidBindOptions())
	if err != nil {
		t.Fatal(err)
	}
	if err := binding.Close(); err != nil {
		t.Fatal(err)
	}
	for _, operation := range nullServiceCalls() {
		if err := operation.call(service, ctx); !errors.Is(err, context.Canceled) {
			t.Errorf("%s canceled error = %v, want context.Canceled", operation.name, err)
		}
	}
	if _, err := binding.Recall(ctx, RecallRequest{}); !errors.Is(err, context.Canceled) {
		t.Fatalf("closed Binding Recall canceled error = %v, want context.Canceled", err)
	}
	if err := service.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := binding.Observe(ctx, Observation{}); !errors.Is(err, context.Canceled) {
		t.Fatalf("parent-closed Binding Observe canceled error = %v, want context.Canceled", err)
	}
	if got, err := service.Bind(ctx, BindOptions{}); !errors.Is(err, context.Canceled) || got != nil {
		t.Fatalf("closed invalid Bind with canceled context = (%v, %v), want (nil, context.Canceled)", got, err)
	}
}

func TestNullServiceConcurrentCloseAndCalls(t *testing.T) {
	service := NewNullService(nil)
	binding, err := service.Bind(context.Background(), nullValidBindOptions())
	if err != nil {
		t.Fatal(err)
	}

	var wg sync.WaitGroup
	for i := 0; i < 16; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			_, err := binding.Recall(context.Background(), RecallRequest{})
			if err != nil && !errors.Is(err, ErrClosed) {
				t.Errorf("Recall error = %v", err)
			}
		}()
	}
	wg.Add(2)
	go func() { defer wg.Done(); _ = binding.Close() }()
	go func() { defer wg.Done(); _ = service.Close() }()
	wg.Wait()
}
