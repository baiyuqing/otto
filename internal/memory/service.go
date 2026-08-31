package memory

import (
	"context"
	"errors"
	"sync"
	"time"
)

func nowUTC() time.Time { return time.Now().UTC().Round(0) }

type service struct {
	store     Store
	retriever Retriever
	policy    Policy

	mu     sync.Mutex
	closed bool
	wg     sync.WaitGroup
}

type binding struct {
	parent         *service
	scopes         []Scope
	extractor      Extractor
	guard          ContentGuard
	estimateTokens func(string) int
	now            func() time.Time

	mu     sync.Mutex
	closed bool
}

var (
	_ Service  = (*service)(nil)
	_ Reader   = (*service)(nil)
	_ Manager  = (*service)(nil)
	_ Proposer = (*service)(nil)
	_ Binding  = (*binding)(nil)
)

// NewService composes a Store, Retriever, and Policy into a working memory
// Service: human-authorized writes through Manager, model/extractor/import
// writes always landing as pending candidates through Proposer.
func NewService(store Store, retriever Retriever, policy Policy) (Service, error) {
	if store == nil || retriever == nil || policy == nil {
		return nil, ErrInvalidRequest
	}
	return &service{store: store, retriever: retriever, policy: policy}, nil
}

// operationError checks whether ctx or the service is done, and if not,
// registers the caller with s.wg so Close waits for it. Every call that
// receives a nil error must defer s.wg.Done().
func (s *service) operationError(ctx context.Context) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	s.mu.Lock()
	closed := s.closed
	if !closed {
		s.wg.Add(1)
	}
	s.mu.Unlock()
	if closed {
		return ErrClosed
	}
	return nil
}

func (s *service) Get(ctx context.Context, ref RecordRef) (Record, error) {
	if err := s.operationError(ctx); err != nil {
		return Record{}, err
	}
	defer s.wg.Done()
	return s.store.Get(ctx, ref)
}

func (s *service) GetByKey(ctx context.Context, key RecordKey) (Record, error) {
	if err := s.operationError(ctx); err != nil {
		return Record{}, err
	}
	defer s.wg.Done()
	return s.store.GetByKey(ctx, key)
}

func (s *service) GetTombstone(ctx context.Context, ref RecordRef) (Tombstone, error) {
	if err := s.operationError(ctx); err != nil {
		return Tombstone{}, err
	}
	defer s.wg.Done()
	return s.store.GetTombstone(ctx, ref)
}

func (s *service) GetCandidate(ctx context.Context, ref CandidateRef) (Candidate, error) {
	if err := s.operationError(ctx); err != nil {
		return Candidate{}, err
	}
	defer s.wg.Done()
	return s.store.GetCandidate(ctx, ref)
}

func (s *service) Search(ctx context.Context, request SearchRequest) (SearchResult, error) {
	if err := s.operationError(ctx); err != nil {
		return SearchResult{}, err
	}
	defer s.wg.Done()
	if err := ValidateSearchRequest(request); err != nil {
		return SearchResult{}, err
	}

	var records []Record
	var nextCursor string
	if request.Query == "" {
		page, err := s.store.List(ctx, ListRequest{
			Scopes: request.Scopes, Kinds: request.Kinds, Labels: request.Labels,
			Limit: request.Limit, Cursor: request.Cursor, Now: request.Now, IncludeExpired: request.IncludeExpired,
		})
		if err != nil {
			return SearchResult{}, err
		}
		records, nextCursor = page.Records, page.NextCursor
	} else {
		result, err := s.retriever.Retrieve(ctx, RetrievalRequest{
			Query: request.Query, Scopes: request.Scopes, Kinds: request.Kinds, Labels: request.Labels,
			IncludeExpired: request.IncludeExpired, Limit: request.Limit, TokenBudget: request.TokenBudget,
			Now: request.Now, Cursor: request.Cursor,
		})
		if err != nil {
			return SearchResult{}, err
		}
		records = make([]Record, len(result.Matches))
		for i, match := range result.Matches {
			records[i] = match.Record
		}
		nextCursor = result.NextCursor
	}

	var candidates []Candidate
	if request.IncludeCandidates {
		page, err := s.store.ListCandidates(ctx, CandidateListRequest{
			Scopes: request.Scopes, States: request.CandidateStates, Limit: request.Limit, Cursor: request.Cursor,
		})
		if err != nil {
			return SearchResult{}, err
		}
		candidates = page.Candidates
	}

	return SearchResult{Records: records, Candidates: candidates, NextCursor: nextCursor}, nil
}

func (s *service) Remember(ctx context.Context, request RememberRequest) (Record, error) {
	if err := s.operationError(ctx); err != nil {
		return Record{}, err
	}
	defer s.wg.Done()
	if err := ValidateRememberRequest(request); err != nil {
		return Record{}, err
	}
	source := request.Source
	if provenanceZero(source) {
		source.Origin = OriginHuman
	}
	now := nowUTC()

	if request.ExpectedRevision == nil {
		if request.Key != "" {
			existing, err := s.store.GetByKey(ctx, RecordKey{Scope: request.Scope, Kind: request.Kind, Key: request.Key})
			if err == nil {
				record := Record{
					ID: existing.ID, Scope: request.Scope, Kind: request.Kind, Key: request.Key, Text: request.Text,
					Labels: request.Labels, Metadata: request.Metadata, Source: source, Confidence: request.Confidence,
					ExpiresAt: request.ExpiresAt, Revision: existing.Revision, CreatedAt: existing.CreatedAt, UpdatedAt: now,
				}
				return s.store.Upsert(ctx, UpsertRequest{Record: record, ExpectedRevision: &existing.Revision})
			}
			if !errors.Is(err, ErrNotFound) {
				return Record{}, err
			}
		}
		id, err := NewID()
		if err != nil {
			return Record{}, err
		}
		record := Record{
			ID: id, Scope: request.Scope, Kind: request.Kind, Key: request.Key, Text: request.Text,
			Labels: request.Labels, Metadata: request.Metadata, Source: source, Confidence: request.Confidence,
			ExpiresAt: request.ExpiresAt, CreatedAt: now, UpdatedAt: now,
		}
		return s.store.Upsert(ctx, UpsertRequest{Record: record})
	}

	var existing Record
	var err error
	if request.ID != "" {
		existing, err = s.store.Get(ctx, RecordRef{Scope: request.Scope, ID: request.ID})
	} else {
		existing, err = s.store.GetByKey(ctx, RecordKey{Scope: request.Scope, Kind: request.Kind, Key: request.Key})
	}
	if err != nil {
		return Record{}, err
	}
	record := Record{
		ID: existing.ID, Scope: request.Scope, Kind: request.Kind, Key: request.Key, Text: request.Text,
		Labels: request.Labels, Metadata: request.Metadata, Source: source, Confidence: request.Confidence,
		ExpiresAt: request.ExpiresAt, Revision: *request.ExpectedRevision, CreatedAt: existing.CreatedAt, UpdatedAt: now,
	}
	return s.store.Upsert(ctx, UpsertRequest{Record: record, ExpectedRevision: request.ExpectedRevision})
}

func (s *service) Forget(ctx context.Context, request ForgetRequest) (ForgetResult, error) {
	if err := s.operationError(ctx); err != nil {
		return ForgetResult{}, err
	}
	defer s.wg.Done()
	if err := ValidateForgetRequest(request); err != nil {
		return ForgetResult{}, err
	}
	if request.PurgeBackups {
		return ForgetResult{}, ErrUnsupported
	}
	tombstone, err := s.store.Forget(ctx, StoreForgetRequest{
		Ref: request.Ref, ExpectedRevision: request.ExpectedRevision, ForgottenAt: nowUTC(),
	})
	if err != nil {
		return ForgetResult{}, err
	}
	return ForgetResult{Tombstone: tombstone}, nil
}

func (s *service) Review(ctx context.Context, request ReviewRequest) (ReviewResult, error) {
	if err := s.operationError(ctx); err != nil {
		return ReviewResult{}, err
	}
	defer s.wg.Done()
	candidate, err := s.store.GetCandidate(ctx, request.Ref)
	if err != nil {
		return ReviewResult{}, err
	}
	storeRequest := StoreReviewRequest{
		Ref: request.Ref, Decision: request.Decision, Edited: request.Edited, TargetRevision: request.TargetRevision,
		DecisionSource: OriginHuman, DecidedAt: nowUTC(),
	}
	if request.Decision == ReviewAccept && candidate.Action == CandidateCreate {
		id, err := NewID()
		if err != nil {
			return ReviewResult{}, err
		}
		storeRequest.ResultRecordID = id
	}
	return s.store.Review(ctx, storeRequest)
}

func (s *service) Propose(ctx context.Context, request ProposeRequest) (CandidateBatch, error) {
	if err := s.operationError(ctx); err != nil {
		return CandidateBatch{}, err
	}
	defer s.wg.Done()
	if err := ValidateProposeRequest(request); err != nil {
		return CandidateBatch{}, err
	}
	decision, err := s.policy.Decide(ctx, PolicyRequest{
		Origin: request.Source.Origin, Action: request.Action, Scope: request.Scope, Kind: request.Kind,
		Confidence: request.Confidence, Source: request.Source, Valid: true,
	})
	if err != nil {
		return CandidateBatch{}, err
	}
	if decision != PolicyPending {
		return CandidateBatch{}, &PolicyDecisionError{Decision: decision}
	}
	id, err := NewID()
	if err != nil {
		return CandidateBatch{}, err
	}
	candidate := Candidate{
		ID: id,
		Proposed: Record{
			Scope: request.Scope, Kind: request.Kind, Key: request.Key, Text: request.Text,
			Labels: request.Labels, Metadata: request.Metadata, Confidence: request.Confidence,
			ExpiresAt: request.ExpiresAt, Source: request.Source,
		},
		Action: request.Action, TargetID: request.TargetID, BaseRevision: request.BaseRevision,
		Reason: request.Reason, State: CandidatePending, CreatedAt: nowUTC(),
	}
	return s.store.Propose(ctx, ProposalBatch{Candidates: []Candidate{candidate}})
}

func (s *service) Bind(ctx context.Context, options BindOptions) (Binding, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return nil, ErrClosed
	}
	if err := ValidateBindOptions(options); err != nil {
		return nil, err
	}
	extractor := options.Extractor
	if extractor == nil {
		extractor = NoopExtractor{}
	}
	guard := options.Guard
	if guard == nil {
		guard = DefaultGuard{}
	}
	now := options.Now
	if now == nil {
		now = nowUTC
	}
	return &binding{
		parent: s, scopes: append([]Scope{}, options.Scopes...),
		extractor: extractor, guard: guard, estimateTokens: options.EstimateTokens, now: now,
	}, nil
}

func (s *service) Close() error {
	s.mu.Lock()
	if s.closed {
		s.mu.Unlock()
		return nil
	}
	s.closed = true
	s.mu.Unlock()
	s.wg.Wait()
	return s.store.Close()
}

// operationError reports whether the service or binding is closed and, if
// neither is, registers the caller's operation with the service's
// WaitGroup so Close waits for it before closing the shared store. Callers
// that get a nil error must call parent.wg.Done() (typically via defer)
// once their operation finishes.
func (b *binding) operationError(ctx context.Context) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	b.parent.mu.Lock()
	b.mu.Lock()
	closed := b.parent.closed || b.closed
	if !closed {
		b.parent.wg.Add(1)
	}
	b.mu.Unlock()
	b.parent.mu.Unlock()
	if closed {
		return ErrClosed
	}
	return nil
}

func (b *binding) Recall(ctx context.Context, request RecallRequest) (RecallResult, error) {
	if err := b.operationError(ctx); err != nil {
		return RecallResult{}, err
	}
	defer b.parent.wg.Done()
	if err := ValidateRecallRequest(request); err != nil {
		return RecallResult{}, err
	}
	result, err := b.parent.retriever.Retrieve(ctx, RetrievalRequest{
		Query: request.Query, Scopes: b.scopes, Kinds: request.Kinds,
		IncludeBaseline: true, Limit: request.Limit, TokenBudget: request.TokenBudget,
		Now: b.now(), EstimateTokens: b.estimateTokens,
	})
	if err != nil {
		return RecallResult{}, err
	}
	records := make([]Record, len(result.Matches))
	for i, match := range result.Matches {
		records[i] = match.Record
	}
	return RecallResult{Records: records, UsedTokens: result.UsedTokens}, nil
}

func (b *binding) Observe(ctx context.Context, observation Observation) (ObserveResult, error) {
	if err := b.operationError(ctx); err != nil {
		return ObserveResult{}, err
	}
	defer b.parent.wg.Done()
	if err := ValidateObservation(observation); err != nil {
		return ObserveResult{}, err
	}

	receipt, err := b.parent.store.GetObservationReceipt(ctx, observation.ID)
	if err == nil {
		return ObserveResult{CandidateIDs: receipt.CandidateIDs, Existing: true}, nil
	}
	if !errors.Is(err, ErrNotFound) {
		return ObserveResult{}, err
	}

	if err := GuardObservation(ctx, b.guard, observation); err != nil {
		return ObserveResult{}, err
	}
	proposals, err := b.extractor.Extract(ctx, ExtractRequest{Observation: observation})
	if err != nil {
		return ObserveResult{}, err
	}

	now := b.now()
	candidates := make([]Candidate, 0, len(proposals))
	for _, proposal := range proposals {
		if err := ValidateProposal(proposal); err != nil {
			return ObserveResult{}, err
		}
		id, err := NewID()
		if err != nil {
			return ObserveResult{}, err
		}
		candidates = append(candidates, Candidate{
			ID: id,
			Proposed: Record{
				Scope: proposal.Scope, Kind: proposal.Kind, Key: proposal.Key, Text: proposal.Text,
				Labels: proposal.Labels, Metadata: proposal.Metadata, Confidence: proposal.Confidence,
				ExpiresAt: proposal.ExpiresAt, Source: Provenance{Origin: OriginExtractor, ObservationID: observation.ID},
			},
			Action: proposal.Action, TargetID: proposal.TargetID, BaseRevision: proposal.BaseRevision,
			Reason: proposal.Reason, State: CandidatePending, CreatedAt: now,
		})
	}

	commit, err := b.parent.store.CommitObservation(ctx, ObservationCommit{
		ObservationID: observation.ID, Candidates: candidates, CreatedAt: now,
	})
	if err != nil {
		return ObserveResult{}, err
	}
	return ObserveResult{CandidateIDs: commit.CandidateIDs, Existing: commit.Existing}, nil
}

func (b *binding) Close() error {
	b.parent.mu.Lock()
	b.mu.Lock()
	b.closed = true
	b.mu.Unlock()
	b.parent.mu.Unlock()
	return nil
}
