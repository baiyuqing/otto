package memory

import (
	"context"
	"errors"
	"reflect"
	"regexp"
	"strings"
	"testing"
)

func TestContractLiteralConstants(t *testing.T) {
	assertConstants(t, "namespace", map[string]string{
		"user": NamespaceUser, "workspace": NamespaceWorkspace,
	})
	assertConstants(t, "origin", map[string]string{
		"human": string(OriginHuman), "model": string(OriginModel), "extractor": string(OriginExtractor),
		"import": string(OriginImport), "migration": string(OriginMigration),
	})
	assertConstants(t, "candidate action", map[string]string{
		"create": string(CandidateCreate), "update": string(CandidateUpdate), "forget": string(CandidateForget),
	})
	assertConstants(t, "candidate state", map[string]string{
		"pending": string(CandidatePending), "accepted": string(CandidateAccepted), "rejected": string(CandidateRejected),
	})
	assertConstants(t, "review decision", map[string]string{
		"accept": string(ReviewAccept), "reject": string(ReviewReject),
	})
	assertConstants(t, "commit operation", map[string]string{
		"schema": string(CommitSchema), "upsert": string(CommitUpsert), "forget": string(CommitForget),
		"propose": string(CommitPropose), "observe": string(CommitObserve), "review": string(CommitReview),
	})
	assertConstants(t, "record state", map[string]string{
		"active": string(RecordActive), "tombstone": string(RecordTombstone),
	})
	assertConstants(t, "policy decision", map[string]string{
		"accept": string(PolicyAccept), "pending": string(PolicyPending), "reject": string(PolicyReject),
	})
}

func assertConstants(t *testing.T, kind string, values map[string]string) {
	t.Helper()
	for want, got := range values {
		if got != want {
			t.Errorf("%s constant = %q, want %q", kind, got, want)
		}
	}
}

func TestContractHardLimits(t *testing.T) {
	got := map[string]int{
		"record text": MaxRecordTextBytes, "namespace": MaxNamespaceBytes, "kind": MaxKindBytes,
		"entity ID": MaxIDBytes, "scope ID": MaxScopeIDBytes, "session ID": MaxSessionIDBytes,
		"message ID": MaxMessageIDBytes, "semantic key": MaxSemanticKeyBytes, "labels": MaxLabels,
		"label": MaxLabelBytes, "metadata entries": MaxMetadataEntries, "metadata key": MaxMetadataKeyBytes,
		"metadata value": MaxMetadataValueBytes, "metadata total": MaxMetadataBytes, "reason": MaxReasonBytes,
		"provenance IDs": MaxProvenanceMessageIDs, "query": MaxQueryBytes, "observation": MaxObservationBytes,
		"observation IDs": MaxObservationMessageIDs, "tool facts": MaxToolFacts, "tool name": MaxToolNameBytes,
		"tool fact": MaxToolFactTextBytes, "FTS terms": MaxFTSTerms, "FTS term": MaxFTSTermBytes,
		"baseline": MaxBaselineRecords, "retrieval candidates": MaxRetrievalCandidates,
		"candidate scan": MaxCandidateScan, "request scopes": MaxRequestScopes, "request kinds": MaxRequestKinds,
		"request labels": MaxRequestLabels, "page": MaxPageSize, "recall": MaxRecallRecords,
		"token budget": MaxTokenBudget, "candidate batch": MaxCandidateBatch,
		"candidate batch bytes": MaxCandidateBatchBytes, "guard fields": MaxGuardFields,
		"guard bytes": MaxGuardBytes, "exact spans": MaxExactGuardSpans, "exact values": MaxExactGuardValues,
		"exact value": MaxExactGuardValueBytes, "cursor": MaxCursorBytes, "commit IDs": MaxCommitUnknownIDs,
	}
	want := map[string]int{
		"record text": 8 * 1024, "namespace": 32, "kind": 32, "entity ID": 64, "scope ID": 128,
		"session ID": 128, "message ID": 128, "semantic key": 256, "labels": 32, "label": 64,
		"metadata entries": 32, "metadata key": 64, "metadata value": 512, "metadata total": 4 * 1024,
		"reason": 2 * 1024, "provenance IDs": 32, "query": 8 * 1024, "observation": 64 * 1024,
		"observation IDs": 32, "tool facts": 32, "tool name": 64, "tool fact": 2 * 1024,
		"FTS terms": 64, "FTS term": 256, "baseline": 16, "retrieval candidates": 256,
		"candidate scan": 500, "request scopes": 16, "request kinds": 16, "request labels": 16,
		"page": 100, "recall": 64, "token budget": 8192, "candidate batch": 8,
		"candidate batch bytes": 256 * 1024, "guard fields": 512, "guard bytes": 64 * 1024,
		"exact spans": 8192, "exact values": 64, "exact value": 8 * 1024, "cursor": 4 * 1024,
		"commit IDs": 16,
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("limits = %#v, want %#v", got, want)
	}
}

func TestErrorSentinelLiterals(t *testing.T) {
	tests := []struct {
		name string
		err  error
		want string
	}{
		{"disabled", ErrDisabled, "memory is disabled"},
		{"unavailable", ErrUnavailable, "memory is unavailable"},
		{"conflict", ErrConflict, "memory revision conflict"},
		{"sensitive memory", ErrSensitiveMemory, "memory contains sensitive data"},
		{"unsupported", ErrUnsupported, "memory operation is unsupported"},
		{"memory in use", ErrMemoryInUse, "memory is in use"},
		{"persistence disabled", ErrPersistenceDisabled, "memory persistence is disabled"},
		{"busy", ErrBusy, "memory store is busy"},
		{"commit unknown", ErrCommitUnknown, "memory commit outcome is unknown"},
		{"corrupt", ErrCorrupt, "memory data is corrupt"},
		{"incompatible schema", ErrIncompatibleSchema, "memory schema is incompatible"},
		{"invalid record", ErrInvalidRecord, "invalid memory record"},
		{"invalid request", ErrInvalidRequest, "invalid memory request"},
		{"not found", ErrNotFound, "memory entity not found"},
		{"closed", ErrClosed, "memory is closed"},
		{"invalid cursor", ErrInvalidCursor, "invalid memory cursor"},
		{"incomplete forget", ErrIncompleteForget, "memory was forgotten but tombstone recording is incomplete"},
		{"incomplete purge", ErrIncompletePurge, "memory backup purge is incomplete"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := test.err.Error(); got != test.want {
				t.Fatalf("error literal = %q, want %q", got, test.want)
			}
		})
	}
}

func TestErrorsAreDistinctAndSafe(t *testing.T) {
	sentinels := []error{
		ErrDisabled, ErrUnavailable, ErrConflict, ErrSensitiveMemory, ErrUnsupported, ErrMemoryInUse,
		ErrPersistenceDisabled, ErrBusy, ErrCommitUnknown, ErrCorrupt, ErrIncompatibleSchema,
		ErrInvalidRecord, ErrInvalidRequest, ErrNotFound, ErrClosed, ErrInvalidCursor,
		ErrIncompleteForget, ErrIncompletePurge,
	}
	seen := make(map[error]struct{}, len(sentinels))
	for _, sentinel := range sentinels {
		if sentinel == nil {
			t.Fatal("nil sentinel")
		}
		if _, exists := seen[sentinel]; exists {
			t.Fatalf("sentinel %q is not distinct", sentinel)
		}
		seen[sentinel] = struct{}{}
	}

	conflict := &ConflictError{EntityKind: "record", ID: "opaque-id", ExpectedRevision: 2, ActualRevision: 3}
	if !errors.Is(conflict, ErrConflict) {
		t.Fatalf("errors.Is(%v, ErrConflict) = false", conflict)
	}
	if regexp.MustCompile(`opaque-id|record`).MatchString(conflict.Error()) {
		t.Fatalf("conflict error leaks details: %q", conflict)
	}
}

func TestErrorsCommitUnknownValidatesAndClonesIDs(t *testing.T) {
	ids := []string{"r1", "r2"}
	errDetail, err := NewCommitUnknownError(CommitUpsert, ids)
	if err != nil {
		t.Fatal(err)
	}
	ids[0] = "mutated"
	if !errors.Is(errDetail, ErrCommitUnknown) {
		t.Fatalf("errors.Is(%v, ErrCommitUnknown) = false", errDetail)
	}
	if errDetail.Operation() != CommitUpsert {
		t.Fatalf("operation = %q", errDetail.Operation())
	}
	got := errDetail.EntityIDs()
	if !reflect.DeepEqual(got, []string{"r1", "r2"}) {
		t.Fatalf("entity IDs = %#v", got)
	}
	got[0] = "mutated-again"
	if again := errDetail.EntityIDs(); again[0] != "r1" {
		t.Fatalf("accessor aliases internal IDs: %#v", again)
	}
	if !regexp.MustCompile(`(?i)reconcile.*before retry`).MatchString(errDetail.Error()) {
		t.Fatalf("error lacks reconciliation direction: %q", errDetail)
	}

	if _, err := NewCommitUnknownError(CommitOperation("unknown"), nil); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("unknown operation error = %v, want ErrInvalidRequest", err)
	}
	tooMany := make([]string, MaxCommitUnknownIDs+1)
	for i := range tooMany {
		tooMany[i] = "id"
	}
	if _, err := NewCommitUnknownError(CommitReview, tooMany); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("too many IDs error = %v, want ErrInvalidRequest", err)
	}
	unsafeID := "raw content with spaces"
	if _, err := NewCommitUnknownError(CommitReview, []string{unsafeID}); !errors.Is(err, ErrInvalidRequest) || regexp.MustCompile(regexp.QuoteMeta(unsafeID)).MatchString(err.Error()) {
		t.Fatalf("unsafe ID error = %v, want content-safe ErrInvalidRequest", err)
	}
}

func TestNewIDIsRandomLowercaseHex(t *testing.T) {
	first, err := NewID()
	if err != nil {
		t.Fatal(err)
	}
	second, err := NewID()
	if err != nil {
		t.Fatal(err)
	}
	valid := regexp.MustCompile(`^[0-9a-f]{32}$`)
	if !valid.MatchString(first) || !valid.MatchString(second) {
		t.Fatalf("IDs = %q, %q; want 32 lowercase hexadecimal characters", first, second)
	}
	if first == second {
		t.Fatalf("generated duplicate IDs: %q", first)
	}
}

func TestGenerateDistinctIDsRetriesDuplicatesThroughCeiling(t *testing.T) {
	first := strings.Repeat("a", 32)
	second := strings.Repeat("b", 32)
	sequence := []string{first}
	for range MaxDuplicateIDRetries {
		sequence = append(sequence, first)
	}
	sequence = append(sequence, second)
	calls := 0
	generate := func() (string, error) {
		id := sequence[calls]
		calls++
		return id, nil
	}

	got, err := GenerateDistinctIDs(2, generate)
	if err != nil {
		t.Fatal(err)
	}
	if want := []string{first, second}; !reflect.DeepEqual(got, want) {
		t.Fatalf("IDs = %#v, want %#v", got, want)
	}
	if got[0] == got[1] {
		t.Fatalf("IDs are not distinct: %#v", got)
	}
	if want := MaxDuplicateIDRetries + 2; calls != want {
		t.Fatalf("generator calls = %d, want %d", calls, want)
	}
}

func TestGenerateDistinctIDsExhaustionIsSafe(t *testing.T) {
	duplicate := strings.Repeat("c", 32)
	calls := 0
	_, err := GenerateDistinctIDs(2, func() (string, error) {
		calls++
		return duplicate, nil
	})
	if !errors.Is(err, ErrUnavailable) {
		t.Fatalf("error = %v, want ErrUnavailable", err)
	}
	if strings.Contains(err.Error(), duplicate) {
		t.Fatalf("error leaks generated ID: %q", err)
	}
	if want := MaxDuplicateIDRetries + 2; calls != want {
		t.Fatalf("generator calls = %d, want %d", calls, want)
	}
}

func TestGenerateDistinctIDsSanitizesGeneratorFailure(t *testing.T) {
	underlying := errors.New("private generator failure")
	_, err := GenerateDistinctIDs(1, func() (string, error) {
		return "", underlying
	})
	if !errors.Is(err, ErrUnavailable) {
		t.Fatalf("error = %v, want ErrUnavailable", err)
	}
	if errors.Is(err, underlying) || strings.Contains(err.Error(), underlying.Error()) {
		t.Fatalf("error exposes underlying failure: %q", err)
	}
}

// These fakes deliberately depend only on this neutral package and the standard library.
type fakeService struct{}

func (fakeService) Get(context.Context, RecordRef) (Record, error)      { return Record{}, nil }
func (fakeService) GetByKey(context.Context, RecordKey) (Record, error) { return Record{}, nil }
func (fakeService) GetTombstone(context.Context, RecordRef) (Tombstone, error) {
	return Tombstone{}, nil
}
func (fakeService) GetCandidate(context.Context, CandidateRef) (Candidate, error) {
	return Candidate{}, nil
}
func (fakeService) Search(context.Context, SearchRequest) (SearchResult, error) {
	return SearchResult{}, nil
}
func (fakeService) Remember(context.Context, RememberRequest) (Record, error) { return Record{}, nil }
func (fakeService) Forget(context.Context, ForgetRequest) (ForgetResult, error) {
	return ForgetResult{}, nil
}
func (fakeService) Review(context.Context, ReviewRequest) (ReviewResult, error) {
	return ReviewResult{}, nil
}
func (fakeService) Propose(context.Context, ProposeRequest) (CandidateBatch, error) {
	return CandidateBatch{}, nil
}
func (fakeService) Bind(context.Context, BindOptions) (Binding, error) { return fakeBinding{}, nil }
func (fakeService) Close() error                                       { return nil }

type fakeBinding struct{}

func (fakeBinding) Recall(context.Context, RecallRequest) (RecallResult, error) {
	return RecallResult{}, nil
}
func (fakeBinding) Observe(context.Context, Observation) (ObserveResult, error) {
	return ObserveResult{}, nil
}
func (fakeBinding) Close() error { return nil }

type fakeStore struct{}

func (fakeStore) Identity(context.Context) (StoreIdentity, error)            { return StoreIdentity{}, nil }
func (fakeStore) Get(context.Context, RecordRef) (Record, error)             { return Record{}, nil }
func (fakeStore) GetByKey(context.Context, RecordKey) (Record, error)        { return Record{}, nil }
func (fakeStore) GetTombstone(context.Context, RecordRef) (Tombstone, error) { return Tombstone{}, nil }
func (fakeStore) GetCandidate(context.Context, CandidateRef) (Candidate, error) {
	return Candidate{}, nil
}
func (fakeStore) List(context.Context, ListRequest) (RecordPage, error) { return RecordPage{}, nil }
func (fakeStore) ListTombstones(context.Context, TombstoneListRequest) (TombstonePage, error) {
	return TombstonePage{}, nil
}
func (fakeStore) ListCandidates(context.Context, CandidateListRequest) (CandidatePage, error) {
	return CandidatePage{}, nil
}
func (fakeStore) Upsert(context.Context, UpsertRequest) (Record, error) { return Record{}, nil }
func (fakeStore) Forget(context.Context, StoreForgetRequest) (Tombstone, error) {
	return Tombstone{}, nil
}
func (fakeStore) Propose(context.Context, ProposalBatch) (CandidateBatch, error) {
	return CandidateBatch{}, nil
}
func (fakeStore) GetObservationReceipt(context.Context, string) (ObservationReceipt, error) {
	return ObservationReceipt{}, nil
}
func (fakeStore) CommitObservation(context.Context, ObservationCommit) (ObservationReceipt, error) {
	return ObservationReceipt{}, nil
}
func (fakeStore) Review(context.Context, StoreReviewRequest) (ReviewResult, error) {
	return ReviewResult{}, nil
}
func (fakeStore) Close() error { return nil }

type fakeRetriever struct{}

func (fakeRetriever) Retrieve(context.Context, RetrievalRequest) (RetrievalResult, error) {
	return RetrievalResult{}, nil
}

type fakeExtractor struct{}

func (fakeExtractor) Extract(context.Context, ExtractRequest) ([]Proposal, error) { return nil, nil }

type fakePolicy struct{}

func (fakePolicy) Decide(context.Context, PolicyRequest) (PolicyDecision, error) { return "", nil }

type fakeMaintenance struct{}

func (fakeMaintenance) Backup(context.Context, BackupRequest) (BackupInfo, error) {
	return BackupInfo{}, nil
}
func (fakeMaintenance) ListBackups(context.Context) ([]BackupInfo, error) { return nil, nil }
func (fakeMaintenance) VerifyBackup(context.Context, string) (BackupInfo, error) {
	return BackupInfo{}, nil
}
func (fakeMaintenance) Restore(context.Context, RestoreRequest) error { return nil }
func (fakeMaintenance) PurgeForgotten(context.Context, PurgeForgottenRequest) (PurgeForgottenResult, error) {
	return PurgeForgottenResult{}, nil
}

func TestInterfacesAreAdapterNeutral(t *testing.T) {
	var _ Reader = fakeService{}
	var _ Service = fakeService{}
	var _ Store = fakeStore{}
	var _ Retriever = fakeRetriever{}
	var _ Extractor = fakeExtractor{}
	var _ Policy = fakePolicy{}
	var _ Maintenance = fakeMaintenance{}
}
