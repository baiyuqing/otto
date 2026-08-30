package memory

import (
	"context"
	"errors"
	"sync"
)

type nullServiceState struct {
	mu     sync.Mutex
	closed bool
}

type nullService struct {
	state    *nullServiceState
	category error
}

type nullBinding struct {
	parent   *nullServiceState
	category error

	mu     sync.Mutex
	closed bool
}

var (
	_ Service  = (*nullService)(nil)
	_ Reader   = (*nullService)(nil)
	_ Manager  = (*nullService)(nil)
	_ Proposer = (*nullService)(nil)
	_ Binding  = (*nullBinding)(nil)
)

// NewNullService returns a no-resource Service for disabled or unavailable
// memory. The supplied reason is reduced to a safe public error category.
func NewNullService(reason error) Service {
	return &nullService{
		state:    &nullServiceState{},
		category: nullCategory(reason),
	}
}

func nullCategory(reason error) error {
	switch {
	case reason == nil, errors.Is(reason, ErrDisabled):
		return ErrDisabled
	case errors.Is(reason, ErrUnavailable):
		return ErrUnavailable
	default:
		return ErrUnavailable
	}
}

func (s *nullService) operationError(ctx context.Context) error {
	if err := ctx.Err(); err != nil {
		return err
	}

	s.state.mu.Lock()
	closed := s.state.closed
	s.state.mu.Unlock()
	if closed {
		return ErrClosed
	}
	return s.category
}

func (s *nullService) Get(ctx context.Context, _ RecordRef) (Record, error) {
	return Record{}, s.operationError(ctx)
}

func (s *nullService) GetByKey(ctx context.Context, _ RecordKey) (Record, error) {
	return Record{}, s.operationError(ctx)
}

func (s *nullService) GetTombstone(ctx context.Context, _ RecordRef) (Tombstone, error) {
	return Tombstone{}, s.operationError(ctx)
}

func (s *nullService) GetCandidate(ctx context.Context, _ CandidateRef) (Candidate, error) {
	return Candidate{}, s.operationError(ctx)
}

func (s *nullService) Search(ctx context.Context, _ SearchRequest) (SearchResult, error) {
	return SearchResult{}, s.operationError(ctx)
}

func (s *nullService) Remember(ctx context.Context, _ RememberRequest) (Record, error) {
	return Record{}, s.operationError(ctx)
}

func (s *nullService) Forget(ctx context.Context, _ ForgetRequest) (ForgetResult, error) {
	return ForgetResult{}, s.operationError(ctx)
}

func (s *nullService) Review(ctx context.Context, _ ReviewRequest) (ReviewResult, error) {
	return ReviewResult{}, s.operationError(ctx)
}

func (s *nullService) Propose(ctx context.Context, _ ProposeRequest) (CandidateBatch, error) {
	return CandidateBatch{}, s.operationError(ctx)
}

func (s *nullService) Bind(ctx context.Context, options BindOptions) (Binding, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}

	s.state.mu.Lock()
	defer s.state.mu.Unlock()
	if s.state.closed {
		return nil, ErrClosed
	}
	if err := ValidateBindOptions(options); err != nil {
		return nil, err
	}
	return &nullBinding{parent: s.state, category: s.category}, nil
}

func (s *nullService) Close() error {
	s.state.mu.Lock()
	s.state.closed = true
	s.state.mu.Unlock()
	return nil
}

func (b *nullBinding) operationError(ctx context.Context) error {
	if err := ctx.Err(); err != nil {
		return err
	}

	b.parent.mu.Lock()
	b.mu.Lock()
	closed := b.parent.closed || b.closed
	b.mu.Unlock()
	b.parent.mu.Unlock()
	if closed {
		return ErrClosed
	}
	return nil
}

func (b *nullBinding) Recall(ctx context.Context, _ RecallRequest) (RecallResult, error) {
	if err := b.operationError(ctx); err != nil {
		return RecallResult{}, err
	}
	return RecallResult{Records: make([]Record, 0)}, nil
}

func (b *nullBinding) Observe(ctx context.Context, _ Observation) (ObserveResult, error) {
	if err := b.operationError(ctx); err != nil {
		return ObserveResult{}, err
	}
	return ObserveResult{CandidateIDs: make([]string, 0)}, nil
}

func (b *nullBinding) Close() error {
	b.parent.mu.Lock()
	b.mu.Lock()
	b.closed = true
	b.mu.Unlock()
	b.parent.mu.Unlock()
	return nil
}
