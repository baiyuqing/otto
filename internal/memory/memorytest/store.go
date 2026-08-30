// Package memorytest provides reusable conformance tests for memory adapters.
package memorytest

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"reflect"
	"sort"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

// RecordStore is the Task 5 record surface. It deliberately omits later store
// methods so adapters need not add untested stubs to use the conformance suite.
type RecordStore interface {
	Identity(context.Context) (memory.StoreIdentity, error)
	Get(context.Context, memory.RecordRef) (memory.Record, error)
	GetByKey(context.Context, memory.RecordKey) (memory.Record, error)
	List(context.Context, memory.ListRequest) (memory.RecordPage, error)
	Upsert(context.Context, memory.UpsertRequest) (memory.Record, error)
	Close() error
}

// MutationStore is the Task 6 revision and tombstone surface.
type MutationStore interface {
	RecordStore
	GetTombstone(context.Context, memory.RecordRef) (memory.Tombstone, error)
	ListTombstones(context.Context, memory.TombstoneListRequest) (memory.TombstonePage, error)
	Forget(context.Context, memory.StoreForgetRequest) (memory.Tombstone, error)
}

// CandidateStore is the complete neutral persistence surface added in Task 7.
type CandidateStore interface {
	MutationStore
	GetCandidate(context.Context, memory.CandidateRef) (memory.Candidate, error)
	ListCandidates(context.Context, memory.CandidateListRequest) (memory.CandidatePage, error)
	Propose(context.Context, memory.ProposalBatch) (memory.CandidateBatch, error)
	GetObservationReceipt(context.Context, string) (memory.ObservationReceipt, error)
	CommitObservation(context.Context, memory.ObservationCommit) (memory.ObservationReceipt, error)
	Review(context.Context, memory.StoreReviewRequest) (memory.ReviewResult, error)
}

// Corruption identifies a direct-storage fixture mutation. These values are
// test-only and let conformance exercise adapter safety without production
// fault methods on RecordStore.
type Corruption string

const (
	CorruptTextOneOver        Corruption = "text-one-over"
	CorruptMetadataOneOver    Corruption = "metadata-one-over"
	CorruptTextOneMiB         Corruption = "text-one-mib"
	CorruptLabelsMalformed    Corruption = "labels-malformed"
	CorruptLabelsOversized    Corruption = "labels-oversized"
	CorruptLabelsWrongShape   Corruption = "labels-wrong-shape"
	CorruptExpiryMalformed    Corruption = "expiry-malformed"
	CorruptExpiryNoncanonical Corruption = "expiry-noncanonical"
	CorruptSensitiveText      Corruption = "sensitive-text"
)

// Persistence is adapter state inspected without going through RecordStore.
type Persistence struct {
	RecordRows int
	FTSRows    int
	Generation uint64
}

// MutationPersistence is a test-only, content-safe persistence fingerprint.
// Digests let conformance prove failed mutations leave row and FTS bytes intact.
type MutationPersistence struct {
	RecordRows     int
	ActiveRows     int
	TombstoneRows  int
	FTSRows        int
	Generation     uint64
	RowDigest      string
	FTSDigest      string
	ContentCleared bool
}

// Fixture owns one fresh database. Reopen opens the same database after Store
// has been closed; Cleanup must be safe after either store has been closed.
// Every callback is required and connected to the adapter's real persistence.
type Fixture struct {
	Store                    RecordStore
	Reopen                   func() (RecordStore, error)
	Cleanup                  func()
	Inspect                  func(context.Context, string) (Persistence, error)
	Inject                   func(memory.Record, Corruption) error
	UpsertBeforeCommitCancel func(context.Context, memory.UpsertRequest) (memory.Record, error)
	UpsertCommitResponseLoss func(context.Context, memory.UpsertRequest) (memory.Record, error)
	InspectMutation          func(context.Context, string) (MutationPersistence, error)
	ForgetBeforeCommitCancel func(context.Context, memory.StoreForgetRequest) (memory.Tombstone, error)
	ForgetCommitResponseLoss func(context.Context, memory.StoreForgetRequest) (memory.Tombstone, error)
	ForbiddenValue           string
}

// Factory returns a fresh isolated fixture for each conformance subtest.
type Factory func(*testing.T) Fixture

// RunRecordConformance checks behavior shared by persistent record stores.
func RunRecordConformance(t *testing.T, factory Factory) {
	t.Helper()
	newFixture := func(t *testing.T) Fixture {
		t.Helper()
		fixture := factory(t)
		if fixture.Store == nil || fixture.Reopen == nil || fixture.Cleanup == nil || fixture.Inspect == nil || fixture.Inject == nil ||
			fixture.UpsertBeforeCommitCancel == nil || fixture.UpsertCommitResponseLoss == nil || fixture.ForbiddenValue == "" {
			t.Fatal("memorytest: incomplete required record fixture callbacks")
		}
		return fixture
	}

	t.Run("create identity aliasing and reopen", func(t *testing.T) {
		fixture := newFixture(t)
		defer fixture.Cleanup()
		ctx := context.Background()
		before, err := fixture.Store.Identity(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if before.Generation != 0 {
			t.Fatalf("initial generation = %d", before.Generation)
		}

		desired := fullRecord("record-a", "primary", time.Date(2026, 8, 29, 12, 0, 0, 123456789, time.UTC))
		want := memory.CloneRecord(desired)
		want.Revision = 1
		created, err := fixture.Store.Upsert(ctx, memory.UpsertRequest{Record: desired})
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(created, want) {
			t.Fatalf("created mismatch\n got: %#v\nwant: %#v", created, want)
		}
		desired.Labels[0] = "mutated-input"
		desired.Metadata["language"] = "mutated-input"
		desired.Source.MessageIDs[0] = "mutated-input"
		*desired.ExpiresAt = desired.ExpiresAt.Add(time.Hour)
		created.Labels[0] = "mutated-output"
		created.Metadata["language"] = "mutated-output"
		created.Source.MessageIDs[0] = "mutated-output"

		got, err := fixture.Store.Get(ctx, memory.RecordRef{Scope: want.Scope, ID: want.ID})
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("stored alias changed\n got: %#v\nwant: %#v", got, want)
		}
		got.Labels[0] = "mutated-get"
		got.Metadata["language"] = "mutated-get"
		got.Source.MessageIDs[0] = "mutated-get"
		again, err := fixture.Store.Get(ctx, memory.RecordRef{Scope: want.Scope, ID: want.ID})
		if err != nil || !reflect.DeepEqual(again, want) {
			t.Fatalf("Get alias changed: %#v, %v", again, err)
		}
		after, err := fixture.Store.Identity(ctx)
		if err != nil || after.Generation != 1 {
			t.Fatalf("generation after create = %d, %v", after.Generation, err)
		}
		persisted, err := fixture.Inspect(ctx, want.ID)
		if err != nil || persisted != (Persistence{RecordRows: 1, FTSRows: 1, Generation: 1}) {
			t.Fatalf("create persistence = %#v, %v", persisted, err)
		}

		if err := fixture.Store.Close(); err != nil {
			t.Fatal(err)
		}
		reopened, err := fixture.Reopen()
		if err != nil {
			t.Fatal(err)
		}
		reopenedRecord, err := reopened.Get(ctx, memory.RecordRef{Scope: want.Scope, ID: want.ID})
		if err != nil || !reflect.DeepEqual(reopenedRecord, want) {
			t.Fatalf("reopen mismatch: %#v, %v", reopenedRecord, err)
		}
	})

	t.Run("keys exact reads and conflicts", func(t *testing.T) {
		fixture := newFixture(t)
		defer fixture.Cleanup()
		ctx := context.Background()
		now := time.Date(2026, 8, 29, 12, 0, 0, 1, time.UTC)
		first := fullRecord("record-key-a", "duplicate", now)
		first.ExpiresAt = timePtr(now.Add(time.Nanosecond))
		created := mustCreate(t, fixture.Store, first)
		for _, id := range []string{"record-empty-a", "record-empty-b"} {
			r := fullRecord(id, "", now.Add(time.Nanosecond))
			mustCreate(t, fixture.Store, r)
		}
		duplicate := fullRecord("record-key-b", "duplicate", now.Add(2*time.Nanosecond))
		if _, err := fixture.Store.Upsert(ctx, memory.UpsertRequest{Record: duplicate}); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("duplicate key error = %v", err)
		}
		identity, err := fixture.Store.Identity(ctx)
		if err != nil || identity.Generation != 3 {
			t.Fatalf("generation after conflict = %d, %v", identity.Generation, err)
		}
		persisted, inspectErr := fixture.Inspect(ctx, duplicate.ID)
		if inspectErr != nil || persisted.RecordRows != 0 || persisted.FTSRows != 0 || persisted.Generation != 3 {
			t.Fatalf("conflict persistence = %#v, %v", persisted, inspectErr)
		}
		got, err := fixture.Store.GetByKey(ctx, memory.RecordKey{Scope: first.Scope, Kind: first.Kind, Key: first.Key})
		if err != nil || !reflect.DeepEqual(got, created) {
			t.Fatalf("GetByKey = %#v, %v", got, err)
		}
		if _, err := fixture.Store.GetByKey(ctx, memory.RecordKey{Scope: first.Scope, Kind: first.Kind}); !errors.Is(err, memory.ErrInvalidRequest) {
			t.Fatalf("empty key error = %v", err)
		}
		wrong := []memory.RecordRef{
			{Scope: memory.Scope{Namespace: "user", ID: "other-user"}, ID: first.ID},
			{Scope: first.Scope, ID: "missing-record"},
		}
		for _, ref := range wrong {
			if _, err := fixture.Store.Get(ctx, ref); !errors.Is(err, memory.ErrNotFound) {
				t.Fatalf("Get(%#v) error = %v", ref, err)
			}
		}
		if _, err := fixture.Store.GetByKey(ctx, memory.RecordKey{Scope: first.Scope, Kind: "other", Key: first.Key}); !errors.Is(err, memory.ErrNotFound) {
			t.Fatalf("wrong kind error = %v", err)
		}
		// Exact management reads include active records whose expiry has passed.
		if _, err := fixture.Store.Get(ctx, memory.RecordRef{Scope: first.Scope, ID: first.ID}); err != nil {
			t.Fatalf("expired exact Get: %v", err)
		}
	})

	t.Run("list filters expiry order and pagination", func(t *testing.T) {
		fixture := newFixture(t)
		defer fixture.Cleanup()
		ctx := context.Background()
		base := time.Date(2026, 8, 29, 12, 0, 0, 0, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: "list-user"}
		for _, spec := range []struct {
			id, kind string
			labels   []string
			updated  time.Time
			expires  *time.Time
		}{
			{"page-e", "note", []string{"blue", "shared"}, base.Add(time.Second + time.Nanosecond), nil},
			{"page-d", "note", []string{"blue", "shared"}, base.Add(time.Second), nil},
			{"page-a", "note", []string{"blue", "shared"}, base, nil},
			{"page-b", "note", []string{"blue", "shared"}, base, nil},
			{"page-c", "note", []string{"blue", "shared"}, base, nil},
			{"expired", "note", []string{"blue", "shared"}, base.Add(2 * time.Second), timePtr(base.Add(3 * time.Second))},
			{"other-kind", "fact", []string{"blue", "shared"}, base.Add(4 * time.Second), nil},
			{"other-label", "note", []string{"blue"}, base.Add(5 * time.Second), nil},
		} {
			r := fullRecord(spec.id, spec.id, spec.updated)
			r.Scope, r.Kind, r.Labels, r.ExpiresAt = scope, spec.kind, spec.labels, spec.expires
			r.CreatedAt = spec.updated
			mustCreate(t, fixture.Store, r)
		}
		request := memory.ListRequest{Scopes: []memory.Scope{scope}, Kinds: []string{"note"}, Labels: []string{"blue", "shared"}, Limit: 2, Now: base.Add(3 * time.Second)}
		var ids []string
		for {
			page, err := fixture.Store.List(ctx, request)
			if err != nil {
				t.Fatal(err)
			}
			for _, record := range page.Records {
				ids = append(ids, record.ID)
			}
			if page.NextCursor == "" {
				break
			}
			request.Cursor = page.NextCursor
		}
		want := []string{"page-e", "page-d", "page-a", "page-b", "page-c"}
		if !reflect.DeepEqual(ids, want) {
			t.Fatalf("pagination order = %v, want %v", ids, want)
		}
		empty, err := fixture.Store.List(ctx, memory.ListRequest{Limit: 10, Now: base})
		if err != nil || len(empty.Records) != 0 {
			t.Fatalf("empty scopes = %#v, %v", empty, err)
		}
		withExpired, err := fixture.Store.List(ctx, memory.ListRequest{Scopes: []memory.Scope{scope}, Kinds: []string{"note"}, Labels: []string{"blue", "shared"}, Limit: 10, IncludeExpired: true})
		if err != nil {
			t.Fatal(err)
		}
		var all []string
		for _, record := range withExpired.Records {
			all = append(all, record.ID)
		}
		if !contains(all, "expired") {
			t.Fatalf("IncludeExpired IDs = %v", all)
		}
	})

	t.Run("cursor cancellation and sensitive refusal", func(t *testing.T) {
		fixture := newFixture(t)
		defer fixture.Cleanup()
		ctx := context.Background()
		now := time.Date(2026, 8, 29, 13, 0, 0, 0, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: "cursor-conformance"}
		for index, id := range []string{"cursor-a", "cursor-b", "cursor-c"} {
			record := fullRecord(id, id, now.Add(time.Duration(index)*time.Nanosecond))
			record.Scope = scope
			mustCreate(t, fixture.Store, record)
		}
		request := memory.ListRequest{Scopes: []memory.Scope{scope}, Kinds: []string{"note"}, Limit: 1, Now: now.Add(time.Hour)}
		first, err := fixture.Store.List(ctx, request)
		if err != nil || first.NextCursor == "" {
			t.Fatalf("first cursor page = %#v, %v", first, err)
		}
		for name, cursor := range map[string]string{
			"malformed": "not_base64_$sensitive-cursor",
			"oversized": strings.Repeat("A", memory.MaxCursorBytes+1),
			"version":   cursorWithVersion(t, first.NextCursor, 999),
		} {
			bad := request
			bad.Cursor = cursor
			if _, err := fixture.Store.List(ctx, bad); !errors.Is(err, memory.ErrInvalidCursor) {
				t.Fatalf("%s cursor error = %v", name, err)
			}
		}
		cross := request
		cross.Cursor, cross.Kinds = first.NextCursor, []string{"fact"}
		if _, err := fixture.Store.List(ctx, cross); !errors.Is(err, memory.ErrInvalidCursor) {
			t.Fatalf("cross-filter cursor error = %v", err)
		}
		mutation := fullRecord("cursor-mutation", "cursor-mutation", now.Add(time.Hour))
		mutation.Scope = scope
		mustCreate(t, fixture.Store, mutation)
		stale := request
		stale.Cursor = first.NextCursor
		if _, err := fixture.Store.List(ctx, stale); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("stale cursor error = %v", err)
		}

		before, err := fixture.Store.Identity(ctx)
		if err != nil {
			t.Fatal(err)
		}
		canceled, cancel := context.WithCancel(ctx)
		cancel()
		if _, err := fixture.Store.Upsert(canceled, memory.UpsertRequest{Record: fullRecord("canceled", "canceled", now)}); !errors.Is(err, context.Canceled) {
			t.Fatalf("canceled create error = %v", err)
		}
		sensitive := fullRecord("sensitive", "sensitive", now)
		sensitive.Text = "password=synthetic-conformance-value"
		if _, err := fixture.Store.Upsert(ctx, memory.UpsertRequest{Record: sensitive}); !errors.Is(err, memory.ErrSensitiveMemory) {
			t.Fatalf("sensitive create error = %v", err)
		}
		after, err := fixture.Store.Identity(ctx)
		if err != nil || after.Generation != before.Generation {
			t.Fatalf("refusal generation = %d, want %d: %v", after.Generation, before.Generation, err)
		}
		persisted, inspectErr := fixture.Inspect(ctx, sensitive.ID)
		if inspectErr != nil || persisted.RecordRows != 0 || persisted.FTSRows != 0 || persisted.Generation != before.Generation {
			t.Fatalf("refusal persistence = %#v, %v", persisted, inspectErr)
		}
	})

	t.Run("nil and empty collection normalization", func(t *testing.T) {
		for _, nonnil := range []bool{false, true} {
			fixture := newFixture(t)
			ctx := context.Background()
			record := fullRecord("normalized", "normalized", time.Date(2026, 8, 29, 13, 10, 0, 0, time.UTC))
			if nonnil {
				record.Labels = make([]string, 0, 1)
				record.Metadata = make(map[string]string, 1)
				record.Source.MessageIDs = make([]string, 0, 1)
			} else {
				record.Labels, record.Metadata, record.Source.MessageIDs = nil, nil, nil
			}
			created, err := fixture.Store.Upsert(ctx, memory.UpsertRequest{Record: record})
			if err != nil {
				t.Fatal(err)
			}
			assertNormalizedCollections(t, created)
			if nonnil {
				record.Labels = append(record.Labels, "caller-label")
				record.Metadata["caller"] = "metadata"
				record.Source.MessageIDs = append(record.Source.MessageIDs, "caller-message")
			}
			got, err := fixture.Store.Get(ctx, memory.RecordRef{Scope: record.Scope, ID: record.ID})
			if err != nil {
				t.Fatal(err)
			}
			assertNormalizedCollections(t, got)
			if err := fixture.Store.Close(); err != nil {
				t.Fatal(err)
			}
			reopened, err := fixture.Reopen()
			if err != nil {
				t.Fatal(err)
			}
			got, err = reopened.Get(ctx, memory.RecordRef{Scope: record.Scope, ID: record.ID})
			if err != nil {
				t.Fatal(err)
			}
			assertNormalizedCollections(t, got)
			fixture.Cleanup()
		}
	})

	for _, corruption := range []Corruption{
		CorruptTextOneOver, CorruptMetadataOneOver, CorruptTextOneMiB,
		CorruptLabelsMalformed, CorruptLabelsOversized, CorruptLabelsWrongShape,
		CorruptExpiryMalformed, CorruptExpiryNoncanonical, CorruptSensitiveText,
	} {
		corruption := corruption
		t.Run("isolated filtered corruption "+string(corruption), func(t *testing.T) {
			fixture := newFixture(t)
			defer fixture.Cleanup()
			now := time.Date(2026, 8, 29, 13, 20, 0, 0, time.UTC)
			record := fullRecord("injected", "injected", now)
			if err := fixture.Inject(record, corruption); err != nil {
				t.Fatal(err)
			}
			want := memory.ErrCorrupt
			if corruption == CorruptSensitiveText {
				want = memory.ErrSensitiveMemory
			}
			if _, err := fixture.Store.Get(context.Background(), memory.RecordRef{Scope: record.Scope, ID: record.ID}); !errors.Is(err, want) {
				t.Fatalf("Get corruption %q = %v", corruption, err)
			}
			request := memory.ListRequest{Scopes: []memory.Scope{record.Scope}, Kinds: []string{record.Kind}, Labels: []string{"alpha"}, Limit: 1, Now: now}
			if _, err := fixture.Store.List(context.Background(), request); !errors.Is(err, want) {
				t.Fatalf("filtered List corruption %q = %v", corruption, err)
			}
		})
	}

	t.Run("precommit cancellation after persistence writes", func(t *testing.T) {
		fixture := newFixture(t)
		defer fixture.Cleanup()
		record := fullRecord("cancel-after-writes", "cancel-after-writes", time.Date(2026, 8, 29, 13, 30, 0, 0, time.UTC))
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		if _, err := fixture.UpsertBeforeCommitCancel(ctx, memory.UpsertRequest{Record: record}); !errors.Is(err, context.Canceled) {
			t.Fatalf("precommit cancellation = %v", err)
		}
		persisted, err := fixture.Inspect(context.Background(), record.ID)
		if err != nil || persisted != (Persistence{}) {
			t.Fatalf("precommit residue = %#v, %v", persisted, err)
		}
	})

	t.Run("real commit response loss reconciles caller ID", func(t *testing.T) {
		fixture := newFixture(t)
		defer fixture.Cleanup()
		record := fullRecord("commit-loss", "commit-loss", time.Date(2026, 8, 29, 13, 40, 0, 0, time.UTC))
		_, err := fixture.UpsertCommitResponseLoss(context.Background(), memory.UpsertRequest{Record: record})
		var unknown *memory.CommitUnknownError
		if !errors.As(err, &unknown) || !reflect.DeepEqual(unknown.EntityIDs(), []string{record.ID}) {
			t.Fatalf("commit response loss = %#v, %v", unknown, err)
		}
		if _, err := fixture.Store.Get(context.Background(), memory.RecordRef{Scope: record.Scope, ID: record.ID}); !errors.Is(err, memory.ErrUnavailable) {
			t.Fatalf("old Store after unknown commit = %v", err)
		}
		if err := fixture.Store.Close(); err != nil {
			t.Fatal(err)
		}
		reopened, err := fixture.Reopen()
		if err != nil {
			t.Fatal(err)
		}
		got, err := reopened.Get(context.Background(), memory.RecordRef{Scope: record.Scope, ID: record.ID})
		if err != nil || got.ID != record.ID || got.Revision != 1 {
			t.Fatalf("commit reconciliation = %#v, %v", got, err)
		}
	})

	t.Run("exact forbidden semantic and opaque fields", func(t *testing.T) {
		for _, field := range []string{"semantic", "opaque ID", "opaque source"} {
			fixture := newFixture(t)
			record := fullRecord("forbidden", "forbidden", time.Date(2026, 8, 29, 13, 50, 0, 0, time.UTC))
			switch field {
			case "semantic":
				record.Metadata["exact"] = fixture.ForbiddenValue
			case "opaque ID":
				record.ID = fixture.ForbiddenValue
			case "opaque source":
				record.Source.SessionID = fixture.ForbiddenValue
			}
			if _, err := fixture.Store.Upsert(context.Background(), memory.UpsertRequest{Record: record}); !errors.Is(err, memory.ErrSensitiveMemory) {
				t.Fatalf("exact forbidden %s Upsert = %v", field, err)
			}
			persisted, err := fixture.Inspect(context.Background(), record.ID)
			if err != nil || persisted != (Persistence{}) {
				t.Fatalf("forbidden %s persistence = %#v, %v", field, persisted, err)
			}
			fixture.Cleanup()
		}
	})
}

// RunMutationConformance checks conditional updates, atomic forgetting, and
// content-free tombstone reads. RunRecordConformance remains a separate gate.
func RunMutationConformance(t *testing.T, factory Factory) {
	t.Helper()
	newFixture := func(t *testing.T) (Fixture, MutationStore) {
		t.Helper()
		fixture := factory(t)
		store, ok := fixture.Store.(MutationStore)
		if !ok || fixture.Reopen == nil || fixture.Cleanup == nil || fixture.InspectMutation == nil ||
			fixture.ForgetBeforeCommitCancel == nil || fixture.ForgetCommitResponseLoss == nil {
			t.Fatal("memorytest: incomplete required mutation fixture callbacks")
		}
		return fixture, store
	}

	t.Run("conditional update and exact FTS replacement", func(t *testing.T) {
		fixture, store := newFixture(t)
		defer fixture.Cleanup()
		ctx := context.Background()
		created := mustCreate(t, store, fullRecord("update-record", "update-key", time.Date(2026, 8, 29, 18, 0, 0, 1, time.UTC)))
		desired := memory.CloneRecord(created)
		desired.Text = "replacement searchable text"
		desired.Labels = []string{"replacement", "labels"}
		desired.Metadata = map[string]string{"updated": "true"}
		desired.Source.MessageIDs = []string{"updated-message"}
		desired.Confidence = 0.5
		desired.UpdatedAt = created.UpdatedAt.Add(time.Hour)
		expected := uint64(1)
		want := memory.CloneRecord(desired)
		want.Revision = 2
		updated, err := store.Upsert(ctx, memory.UpsertRequest{Record: desired, ExpectedRevision: &expected})
		if err != nil || !reflect.DeepEqual(updated, want) {
			t.Fatalf("update = %#v, %v; want %#v", updated, err, want)
		}
		desired.Labels[0], desired.Metadata["updated"], desired.Source.MessageIDs[0] = "caller", "caller", "caller"
		updated.Labels[0], updated.Metadata["updated"], updated.Source.MessageIDs[0] = "output", "output", "output"
		got, err := store.Get(ctx, memory.RecordRef{Scope: want.Scope, ID: want.ID})
		if err != nil || !reflect.DeepEqual(got, want) {
			t.Fatalf("updated alias = %#v, %v", got, err)
		}
		persisted, err := fixture.InspectMutation(ctx, want.ID)
		if err != nil || persisted.RecordRows != 1 || persisted.ActiveRows != 1 || persisted.TombstoneRows != 0 || persisted.FTSRows != 1 || persisted.Generation != 2 {
			t.Fatalf("updated persistence = %#v, %v", persisted, err)
		}
	})

	t.Run("update refusals are atomic", func(t *testing.T) {
		fixture, store := newFixture(t)
		defer fixture.Cleanup()
		ctx := context.Background()
		firstInput := fullRecord("atomic-update", "first-key", time.Date(2026, 8, 29, 18, 10, 0, 0, time.UTC))
		firstInput.UpdatedAt = firstInput.CreatedAt.Add(time.Hour)
		first := mustCreate(t, store, firstInput)
		other := fullRecord("atomic-other", "occupied-key", first.CreatedAt.Add(time.Nanosecond))
		mustCreate(t, store, other)
		assertUnchanged := func(name string, request memory.UpsertRequest, want error) {
			t.Helper()
			before, inspectErr := fixture.InspectMutation(ctx, first.ID)
			if inspectErr != nil {
				t.Fatal(inspectErr)
			}
			if _, err := store.Upsert(ctx, request); !errors.Is(err, want) {
				t.Fatalf("%s error = %v, want %v", name, err, want)
			}
			after, inspectErr := fixture.InspectMutation(ctx, first.ID)
			if inspectErr != nil || !reflect.DeepEqual(after, before) {
				t.Fatalf("%s persistence changed\n before: %#v\n  after: %#v, %v", name, before, after, inspectErr)
			}
		}
		request := func(revision uint64) memory.UpsertRequest {
			desired := memory.CloneRecord(first)
			desired.Revision = revision
			desired.UpdatedAt = desired.UpdatedAt.Add(time.Hour)
			return memory.UpsertRequest{Record: desired, ExpectedRevision: &revision}
		}
		assertUnchanged("zero", request(0), memory.ErrInvalidRequest)
		assertUnchanged("too new", request(2), memory.ErrConflict)

		changedCreated := request(1)
		changedCreated.Record.CreatedAt = changedCreated.Record.CreatedAt.Add(time.Nanosecond)
		assertUnchanged("created time", changedCreated, memory.ErrInvalidRequest)
		early := request(1)
		early.Record.UpdatedAt = first.UpdatedAt.Add(-time.Nanosecond)
		assertUnchanged("early update", early, memory.ErrInvalidRequest)
		occupied := request(1)
		occupied.Record.Key = other.Key
		assertUnchanged("semantic key", occupied, memory.ErrConflict)

		wrongID := request(1)
		wrongID.Record.ID = "different-update-id"
		assertUnchanged("immutable ID", wrongID, memory.ErrNotFound)
		wrongScope := request(1)
		wrongScope.Record.Scope.ID = "different-update-scope"
		assertUnchanged("immutable scope", wrongScope, memory.ErrNotFound)

		valid := request(1)
		updated, err := store.Upsert(ctx, valid)
		if err != nil || updated.Revision != 2 {
			t.Fatalf("valid update = %#v, %v", updated, err)
		}
		assertUnchanged("old", request(1), memory.ErrConflict)
	})

	t.Run("forget clears content and is revision idempotent", func(t *testing.T) {
		fixture, store := newFixture(t)
		defer fixture.Cleanup()
		ctx := context.Background()
		record := mustCreate(t, store, fullRecord("forgotten-record", "forgotten-key", time.Date(2026, 8, 29, 18, 20, 0, 0, time.UTC)))
		before, err := fixture.InspectMutation(ctx, record.ID)
		if err != nil {
			t.Fatal(err)
		}
		early := memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: record.Scope, ID: record.ID}, ExpectedRevision: 1, ForgottenAt: record.UpdatedAt.Add(-time.Nanosecond)}
		if _, err := store.Forget(ctx, early); !errors.Is(err, memory.ErrInvalidRequest) {
			t.Fatalf("early Forget = %v", err)
		}
		unchanged, err := fixture.InspectMutation(ctx, record.ID)
		if err != nil || !reflect.DeepEqual(unchanged, before) {
			t.Fatalf("early Forget persistence = %#v, %v; want %#v", unchanged, err, before)
		}
		stale := early
		stale.ExpectedRevision, stale.ForgottenAt = 2, record.UpdatedAt.Add(time.Hour)
		beforeStale, err := fixture.InspectMutation(ctx, record.ID)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := store.Forget(ctx, stale); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("stale Forget = %v", err)
		}
		afterStale, err := fixture.InspectMutation(ctx, record.ID)
		if err != nil || !reflect.DeepEqual(afterStale, beforeStale) {
			t.Fatalf("stale Forget persistence changed\n before: %#v\n  after: %#v, %v", beforeStale, afterStale, err)
		}

		request := memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: record.Scope, ID: record.ID}, ExpectedRevision: 1, ForgottenAt: record.UpdatedAt.Add(time.Hour)}
		want := memory.Tombstone{ID: record.ID, Scope: record.Scope, Revision: 2, CreatedAt: record.CreatedAt, UpdatedAt: request.ForgottenAt, ForgottenAt: request.ForgottenAt}
		tombstone, err := store.Forget(ctx, request)
		if err != nil || tombstone != want {
			t.Fatalf("Forget = %#v, %v; want %#v", tombstone, err, want)
		}
		persisted, err := fixture.InspectMutation(ctx, record.ID)
		if err != nil || persisted.RecordRows != 1 || persisted.ActiveRows != 0 || persisted.TombstoneRows != 1 || persisted.FTSRows != 0 || persisted.Generation != 2 || !persisted.ContentCleared {
			t.Fatalf("forgotten persistence = %#v, %v", persisted, err)
		}
		if _, err := store.Get(ctx, request.Ref); !errors.Is(err, memory.ErrNotFound) {
			t.Fatalf("forgotten Get = %v", err)
		}
		if page, err := store.List(ctx, memory.ListRequest{Scopes: []memory.Scope{record.Scope}, Limit: 10, IncludeExpired: true}); err != nil || len(page.Records) != 0 {
			t.Fatalf("forgotten List = %#v, %v", page, err)
		}
		got, err := store.GetTombstone(ctx, request.Ref)
		if err != nil || got != want {
			t.Fatalf("GetTombstone = %#v, %v", got, err)
		}
		for name, revision := range map[string]uint64{"old": 1, "new": 3, "zero": 0} {
			retry := request
			retry.ExpectedRevision = revision
			_, err := store.Forget(ctx, retry)
			category := memory.ErrConflict
			if revision == 0 {
				category = memory.ErrInvalidRequest
			}
			if !errors.Is(err, category) {
				t.Fatalf("%s repeated Forget = %v", name, err)
			}
		}
		idempotent := request
		idempotent.ExpectedRevision = want.Revision
		again, err := store.Forget(ctx, idempotent)
		if err != nil || again != want {
			t.Fatalf("idempotent Forget = %#v, %v", again, err)
		}
		after, err := fixture.InspectMutation(ctx, record.ID)
		if err != nil || !reflect.DeepEqual(after, persisted) {
			t.Fatalf("idempotent persistence = %#v, %v; want %#v", after, err, persisted)
		}

		staleRecord := memory.CloneRecord(record)
		staleRecord.UpdatedAt = request.ForgottenAt.Add(time.Hour)
		expected := uint64(1)
		_, err = store.Upsert(ctx, memory.UpsertRequest{Record: staleRecord, ExpectedRevision: &expected})
		var conflict *memory.ConflictError
		if !errors.As(err, &conflict) {
			t.Fatalf("tombstone resurrection = %v (%T), want *memory.ConflictError", err, err)
		}
		if conflict.ExpectedRevision != 1 || conflict.ActualRevision != 2 {
			t.Fatalf("tombstone resurrection revisions = expected %d, actual %d; want expected 1, actual 2", conflict.ExpectedRevision, conflict.ActualRevision)
		}
	})

	t.Run("tombstone exact reads pagination cursors and reopen", func(t *testing.T) {
		fixture, store := newFixture(t)
		defer fixture.Cleanup()
		ctx := context.Background()
		scope := memory.Scope{Namespace: "user", ID: "tombstone-list-user"}
		base := time.Date(2026, 8, 29, 18, 30, 0, 0, time.UTC)
		active := fullRecord("still-active", "still-active", base)
		active.Scope = scope
		mustCreate(t, store, active)
		for index, spec := range []struct {
			id string
			at time.Time
		}{{"tomb-c", base.Add(time.Hour)}, {"tomb-a", base.Add(2 * time.Hour)}, {"tomb-b", base.Add(2 * time.Hour)}} {
			record := fullRecord(spec.id, spec.id, base.Add(time.Duration(index+1)*time.Nanosecond))
			record.Scope = scope
			mustCreate(t, store, record)
			_, err := store.Forget(ctx, memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: scope, ID: spec.id}, ExpectedRevision: 1, ForgottenAt: spec.at})
			if err != nil {
				t.Fatal(err)
			}
		}
		for _, ref := range []memory.RecordRef{{Scope: scope, ID: active.ID}, {Scope: scope, ID: "missing-tombstone"}, {Scope: memory.Scope{Namespace: "user", ID: "wrong-tombstone-scope"}, ID: "tomb-a"}} {
			if _, err := store.GetTombstone(ctx, ref); !errors.Is(err, memory.ErrNotFound) {
				t.Fatalf("GetTombstone(%#v) = %v", ref, err)
			}
		}
		request := memory.TombstoneListRequest{Scopes: []memory.Scope{scope}, Limit: 1}
		first, err := store.ListTombstones(ctx, request)
		if err != nil || len(first.Tombstones) != 1 || first.NextCursor == "" {
			t.Fatalf("first tombstone page = %#v, %v", first, err)
		}
		var ids []string
		for {
			page, err := store.ListTombstones(ctx, request)
			if err != nil {
				t.Fatal(err)
			}
			for _, tombstone := range page.Tombstones {
				if err := memory.ValidateTombstone(tombstone); err != nil {
					t.Fatalf("listed tombstone = %#v: %v", tombstone, err)
				}
				ids = append(ids, tombstone.ID)
			}
			if page.NextCursor == "" {
				break
			}
			request.Cursor = page.NextCursor
		}
		if want := []string{"tomb-a", "tomb-b", "tomb-c"}; !reflect.DeepEqual(ids, want) {
			t.Fatalf("tombstone order = %v, want %v", ids, want)
		}
		if _, err := store.ListTombstones(ctx, memory.TombstoneListRequest{Scopes: []memory.Scope{scope}, Limit: memory.MaxPageSize + 1}); !errors.Is(err, memory.ErrInvalidRequest) {
			t.Fatalf("hard tombstone page bound = %v", err)
		}
		if _, err := store.ListTombstones(ctx, memory.TombstoneListRequest{Scopes: []memory.Scope{scope}, Limit: 1, Cursor: "not_base64_$tombstone"}); !errors.Is(err, memory.ErrInvalidCursor) {
			t.Fatalf("malformed tombstone cursor = %v", err)
		}
		crossScope := memory.TombstoneListRequest{Scopes: []memory.Scope{{Namespace: "user", ID: "other-tombstone-list-user"}}, Limit: 1, Cursor: first.NextCursor}
		if _, err := store.ListTombstones(ctx, crossScope); !errors.Is(err, memory.ErrInvalidCursor) {
			t.Fatalf("cross-scope tombstone cursor = %v", err)
		}
		if page, err := store.ListTombstones(ctx, memory.TombstoneListRequest{Scopes: crossScope.Scopes, Limit: 10}); err != nil || len(page.Tombstones) != 0 {
			t.Fatalf("exact tombstone scope = %#v, %v", page, err)
		}
		mutation := fullRecord("tombstone-cursor-mutation", "tombstone-cursor-mutation", base.Add(3*time.Hour))
		mutation.Scope = scope
		mustCreate(t, store, mutation)
		if _, err := store.ListTombstones(ctx, memory.TombstoneListRequest{Scopes: []memory.Scope{scope}, Limit: 1, Cursor: first.NextCursor}); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("stale tombstone cursor = %v", err)
		}

		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		reopenedBase, err := fixture.Reopen()
		if err != nil {
			t.Fatal(err)
		}
		reopened, ok := reopenedBase.(MutationStore)
		if !ok {
			t.Fatal("reopened store does not implement MutationStore")
		}
		got, err := reopened.GetTombstone(ctx, memory.RecordRef{Scope: scope, ID: "tomb-a"})
		if err != nil || got.ID != "tomb-a" || got.Revision != 2 {
			t.Fatalf("reopened tombstone = %#v, %v", got, err)
		}
	})

	t.Run("forget cancellation and real commit response loss", func(t *testing.T) {
		fixture, store := newFixture(t)
		defer fixture.Cleanup()
		ctx := context.Background()
		canceledRecord := mustCreate(t, store, fullRecord("forget-canceled", "forget-canceled", time.Date(2026, 8, 29, 18, 40, 0, 0, time.UTC)))
		canceledRequest := memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: canceledRecord.Scope, ID: canceledRecord.ID}, ExpectedRevision: 1, ForgottenAt: canceledRecord.UpdatedAt.Add(time.Hour)}
		before, err := fixture.InspectMutation(ctx, canceledRecord.ID)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := fixture.ForgetBeforeCommitCancel(ctx, canceledRequest); !errors.Is(err, context.Canceled) {
			t.Fatalf("canceled Forget = %v", err)
		}
		after, err := fixture.InspectMutation(ctx, canceledRecord.ID)
		if err != nil || !reflect.DeepEqual(after, before) {
			t.Fatalf("canceled Forget persistence = %#v, %v; want %#v", after, err, before)
		}

		lostRecord := mustCreate(t, store, fullRecord("forget-commit-loss", "forget-commit-loss", canceledRecord.UpdatedAt.Add(time.Nanosecond)))
		lostRequest := memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: lostRecord.Scope, ID: lostRecord.ID}, ExpectedRevision: 1, ForgottenAt: lostRecord.UpdatedAt.Add(time.Hour)}
		_, err = fixture.ForgetCommitResponseLoss(ctx, lostRequest)
		var unknown *memory.CommitUnknownError
		if !errors.As(err, &unknown) || unknown.Operation() != memory.CommitForget || !reflect.DeepEqual(unknown.EntityIDs(), []string{lostRecord.ID}) {
			t.Fatalf("Forget commit response loss = %#v, %v", unknown, err)
		}
		if _, err := store.GetTombstone(ctx, lostRequest.Ref); !errors.Is(err, memory.ErrUnavailable) {
			t.Fatalf("poisoned GetTombstone after unknown commit = %v", err)
		}
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		reopenedBase, err := fixture.Reopen()
		if err != nil {
			t.Fatal(err)
		}
		reopened := reopenedBase.(MutationStore)
		reconciled, err := reopened.GetTombstone(ctx, lostRequest.Ref)
		if err != nil || reconciled.Revision != 2 || reconciled.ForgottenAt != lostRequest.ForgottenAt {
			t.Fatalf("Forget reconciliation = %#v, %v", reconciled, err)
		}
		retry := lostRequest
		retry.ExpectedRevision = reconciled.Revision
		idempotent, err := reopened.Forget(ctx, retry)
		if err != nil || idempotent != reconciled {
			t.Fatalf("Forget retry after reconciliation = %#v, %v", idempotent, err)
		}
	})
}

// RunCandidateConformance checks candidate durability, observation idempotency,
// review materialization, rebases, pagination, clearing, and cross-handle races.
func RunCandidateConformance(t *testing.T, factory Factory) {
	t.Helper()
	newFixture := func(t *testing.T) (Fixture, CandidateStore) {
		t.Helper()
		fixture := factory(t)
		store, ok := fixture.Store.(CandidateStore)
		if !ok || fixture.Reopen == nil || fixture.Cleanup == nil || fixture.ForbiddenValue == "" {
			t.Fatal("memorytest: incomplete required candidate fixture")
		}
		return fixture, store
	}
	identityGeneration := func(t *testing.T, store CandidateStore) uint64 {
		t.Helper()
		identity, err := store.Identity(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		return identity.Generation
	}
	propose := func(t *testing.T, store CandidateStore, candidates ...memory.Candidate) memory.CandidateBatch {
		t.Helper()
		batch, err := store.Propose(context.Background(), memory.ProposalBatch{Candidates: candidates})
		if err != nil {
			t.Fatal(err)
		}
		return batch
	}

	t.Run("proposal atomic order aliases exact reads and pagination", func(t *testing.T) {
		fixture, store := newFixture(t)
		defer fixture.Cleanup()
		at := time.Date(2026, 8, 29, 20, 0, 0, 0, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: "candidate-page"}
		first := pendingCandidate("candidate-b", scope, at)
		second := pendingCandidate("candidate-a", scope, at)
		want := []memory.Candidate{memory.CloneCandidate(first), memory.CloneCandidate(second)}
		batch := propose(t, store, first, second)
		if !reflect.DeepEqual(batch.Candidates, want) || identityGeneration(t, store) != 1 {
			t.Fatalf("proposal = %#v, generation=%d", batch, identityGeneration(t, store))
		}
		first.Proposed.Labels[0], first.Proposed.Metadata["purpose"], first.Proposed.Source.MessageIDs[0] = "input", "input", "input"
		batch.Candidates[0].Proposed.Labels[0] = "output"
		got, err := store.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: want[0].ID})
		if err != nil || !reflect.DeepEqual(got, want[0]) {
			t.Fatalf("GetCandidate = %#v, %v", got, err)
		}
		got.Proposed.Metadata["purpose"] = "alias"
		again, _ := store.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: want[0].ID})
		if !reflect.DeepEqual(again, want[0]) {
			t.Fatalf("candidate alias changed: %#v", again)
		}
		if _, err := store.GetCandidate(context.Background(), memory.CandidateRef{Scope: memory.Scope{Namespace: "user", ID: "wrong"}, ID: want[0].ID}); !errors.Is(err, memory.ErrNotFound) {
			t.Fatalf("wrong scope = %v", err)
		}
		request := memory.CandidateListRequest{Scopes: []memory.Scope{scope}, States: []memory.CandidateState{memory.CandidatePending}, Limit: 1}
		page1, err := store.ListCandidates(context.Background(), request)
		if err != nil || len(page1.Candidates) != 1 || page1.Candidates[0].ID != "candidate-a" || page1.NextCursor == "" || strings.Contains(page1.NextCursor, want[0].Proposed.Text) {
			t.Fatalf("first candidate page = %#v, %v", page1, err)
		}
		request.Cursor = page1.NextCursor
		page2, err := store.ListCandidates(context.Background(), request)
		if err != nil || len(page2.Candidates) != 1 || page2.Candidates[0].ID != "candidate-b" || page2.NextCursor != "" {
			t.Fatalf("second candidate page = %#v, %v", page2, err)
		}
		bad := pendingCandidate("candidate-b", scope, at.Add(time.Second))
		if _, err := store.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{pendingCandidate("candidate-c", scope, at), bad}}); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("duplicate batch = %v", err)
		}
		invalid := pendingCandidate("candidate-invalid", scope, at)
		invalid.Proposed.Text = ""
		if _, err := store.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{pendingCandidate("candidate-atomic", scope, at), invalid}}); !errors.Is(err, memory.ErrInvalidRecord) {
			t.Fatalf("invalid batch = %v", err)
		}
		sensitive := pendingCandidate("candidate-sensitive", scope, at)
		sensitive.Proposed.Metadata["blocked"] = fixture.ForbiddenValue
		if _, err := store.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{pendingCandidate("candidate-guard-atomic", scope, at), sensitive}}); !errors.Is(err, memory.ErrSensitiveMemory) {
			t.Fatalf("guarded batch = %v", err)
		}
		for _, id := range []string{"candidate-c", "candidate-atomic", "candidate-guard-atomic"} {
			if _, err := store.GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: id}); !errors.Is(err, memory.ErrNotFound) {
				t.Fatalf("atomic residue %s = %v", id, err)
			}
		}
		if identityGeneration(t, store) != 1 {
			t.Fatalf("failed proposal generation = %d", identityGeneration(t, store))
		}
		mutation := pendingCandidate("candidate-cursor-mutation", scope, at.Add(time.Second))
		propose(t, store, mutation)
		if _, err := store.ListCandidates(context.Background(), request); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("stale candidate cursor = %v", err)
		}
	})

	t.Run("observation receipt is ordered durable and payload independent", func(t *testing.T) {
		fixture, store := newFixture(t)
		defer fixture.Cleanup()
		ctx := context.Background()
		at := time.Date(2026, 8, 29, 20, 10, 0, 0, time.UTC)
		if _, err := store.GetObservationReceipt(ctx, "observation-empty"); !errors.Is(err, memory.ErrNotFound) {
			t.Fatalf("unknown receipt = %v", err)
		}
		empty, err := store.CommitObservation(ctx, memory.ObservationCommit{ObservationID: "observation-empty", CreatedAt: at, Candidates: []memory.Candidate{}})
		if err != nil || empty.Existing || empty.CandidateIDs == nil || len(empty.CandidateIDs) != 0 || identityGeneration(t, store) != 1 {
			t.Fatalf("empty receipt = %#v, %v generation=%d", empty, err, identityGeneration(t, store))
		}
		// Durable receipt probing wins before validation of an alternate retry.
		retry, err := store.CommitObservation(ctx, memory.ObservationCommit{ObservationID: "observation-empty"})
		if err != nil || !retry.Existing || len(retry.CandidateIDs) != 0 || identityGeneration(t, store) != 1 {
			t.Fatalf("empty retry = %#v, %v", retry, err)
		}
		scope := memory.Scope{Namespace: "user", ID: "observed"}
		one := pendingCandidate("observed-two", scope, at.Add(time.Second))
		two := pendingCandidate("observed-one", scope, at.Add(time.Second))
		one.Proposed.Source.ObservationID, two.Proposed.Source.ObservationID = "observation-batch", "observation-batch"
		commit := memory.ObservationCommit{ObservationID: "observation-batch", CreatedAt: at.Add(time.Second), Candidates: []memory.Candidate{one, two}}
		receipt, err := store.CommitObservation(ctx, commit)
		if err != nil || receipt.Existing || !reflect.DeepEqual(receipt.CandidateIDs, []string{one.ID, two.ID}) || identityGeneration(t, store) != 2 {
			t.Fatalf("receipt = %#v, %v", receipt, err)
		}
		commit.Candidates[0].Proposed.Text = "caller mutation"
		receipt.CandidateIDs[0] = "caller mutation"
		listed, err := store.ListCandidates(ctx, memory.CandidateListRequest{Scopes: []memory.Scope{scope}, States: []memory.CandidateState{memory.CandidatePending}, Limit: 10})
		if err != nil || len(listed.Candidates) != 2 || listed.Candidates[0].ID != two.ID || listed.Candidates[1].ID != one.ID {
			t.Fatalf("observed candidates = %#v, %v", listed, err)
		}
		probe, err := store.GetObservationReceipt(ctx, "observation-batch")
		if err != nil || !probe.Existing || !reflect.DeepEqual(probe.CandidateIDs, []string{one.ID, two.ID}) {
			t.Fatalf("probe = %#v, %v", probe, err)
		}
		alternate := memory.ObservationCommit{ObservationID: "observation-batch", Candidates: []memory.Candidate{pendingCandidate("invalid-alternate", scope, time.Time{})}}
		again, err := store.CommitObservation(ctx, alternate)
		if err != nil || !again.Existing || !reflect.DeepEqual(again.CandidateIDs, probe.CandidateIDs) || identityGeneration(t, store) != 2 {
			t.Fatalf("alternate retry = %#v, %v", again, err)
		}
		occupied := pendingCandidate("occupied-observation-candidate", scope, at.Add(2*time.Second))
		propose(t, store, occupied)
		duplicate := memory.CloneCandidate(occupied)
		duplicate.Proposed.Source.ObservationID = "failed-observation"
		if _, err := store.CommitObservation(ctx, memory.ObservationCommit{ObservationID: "failed-observation", CreatedAt: at.Add(2 * time.Second), Candidates: []memory.Candidate{duplicate}}); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("failed candidate insert = %v", err)
		}
		if _, err := store.GetObservationReceipt(ctx, "failed-observation"); !errors.Is(err, memory.ErrNotFound) || identityGeneration(t, store) != 3 {
			t.Fatalf("failed insert receipt residue = %v generation=%d", err, identityGeneration(t, store))
		}
		mismatch := pendingCandidate("observation-mismatch", scope, at.Add(3*time.Second))
		mismatch.Proposed.Source.ObservationID = "other-observation"
		if _, err := store.CommitObservation(ctx, memory.ObservationCommit{ObservationID: "observation-mismatch", CreatedAt: at.Add(3 * time.Second), Candidates: []memory.Candidate{mismatch}}); !errors.Is(err, memory.ErrCorrupt) && !errors.Is(err, memory.ErrInvalidRequest) {
			t.Fatalf("mismatched source = %v", err)
		}
		if _, err := store.GetObservationReceipt(ctx, "observation-mismatch"); !errors.Is(err, memory.ErrNotFound) {
			t.Fatalf("mismatch receipt residue = %v", err)
		}
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		reopenedBase, err := fixture.Reopen()
		if err != nil {
			t.Fatal(err)
		}
		reopened := reopenedBase.(CandidateStore)
		durable, err := reopened.GetObservationReceipt(ctx, "observation-batch")
		if err != nil || !reflect.DeepEqual(durable.CandidateIDs, []string{one.ID, two.ID}) {
			t.Fatalf("reopened receipt = %#v, %v", durable, err)
		}
	})

	t.Run("reject clears content and survives reopen", func(t *testing.T) {
		fixture, store := newFixture(t)
		defer fixture.Cleanup()
		at := time.Date(2026, 8, 29, 20, 20, 0, 0, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: "review-reject"}
		candidate := pendingCandidate("reject-candidate", scope, at)
		propose(t, store, candidate)
		result, err := store.Review(context.Background(), memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: candidate.ID}, Decision: memory.ReviewReject, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(time.Second)})
		if err != nil || result.Record != nil || result.Tombstone != nil || result.Candidate.State != memory.CandidateRejected || result.Candidate.Reason != "" || result.Candidate.Proposed.Text != "" || result.Candidate.Proposed.Scope != scope {
			t.Fatalf("reject = %#v, %v", result, err)
		}
		if identityGeneration(t, store) != 2 {
			t.Fatalf("reject generation = %d", identityGeneration(t, store))
		}
		if _, err := store.Review(context.Background(), memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: candidate.ID}, Decision: memory.ReviewReject, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(2 * time.Second)}); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("second review = %v", err)
		}
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		reopenedBase, err := fixture.Reopen()
		if err != nil {
			t.Fatal(err)
		}
		cleared, err := reopenedBase.(CandidateStore).GetCandidate(context.Background(), memory.CandidateRef{Scope: scope, ID: candidate.ID})
		if err != nil || cleared.Proposed.Text != "" || cleared.Reason != "" || cleared.State != memory.CandidateRejected {
			t.Fatalf("reopened reject = %#v, %v", cleared, err)
		}
	})

	t.Run("accept create update rebase and forget", func(t *testing.T) {
		fixture, store := newFixture(t)
		defer fixture.Cleanup()
		ctx := context.Background()
		at := time.Date(2026, 8, 29, 20, 30, 0, 0, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: "review-accept"}
		create := pendingCandidate("create-candidate", scope, at)
		propose(t, store, create)
		for name, id := range map[string]string{"missing": "", "extra reject": "extra"} {
			decision := memory.ReviewAccept
			if name == "extra reject" {
				decision = memory.ReviewReject
			}
			_, err := store.Review(ctx, memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: create.ID}, ResultRecordID: id, Decision: decision, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(time.Second)})
			if !errors.Is(err, memory.ErrInvalidRequest) {
				t.Fatalf("%s result ID = %v", name, err)
			}
		}
		accepted, err := store.Review(ctx, memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: create.ID}, ResultRecordID: "created-record", Decision: memory.ReviewAccept, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(time.Second)})
		if err != nil || accepted.Record == nil || accepted.Record.ID != "created-record" || accepted.Record.Revision != 1 || accepted.Record.Source.DecisionAt == nil || *accepted.Record.Source.DecisionAt != at.Add(time.Second) || accepted.Candidate.State != memory.CandidateAccepted || accepted.Candidate.Proposed.Text != "" {
			t.Fatalf("accept create = %#v, %v", accepted, err)
		}
		collision := pendingCandidate("collision-candidate", scope, at.Add(2*time.Second))
		propose(t, store, collision)
		if _, err := store.Review(ctx, memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: collision.ID}, ResultRecordID: "created-record", Decision: memory.ReviewAccept, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(3 * time.Second)}); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("colliding create = %v", err)
		}
		pending, _ := store.GetCandidate(ctx, memory.CandidateRef{Scope: scope, ID: collision.ID})
		if pending.State != memory.CandidatePending {
			t.Fatalf("collision resolved candidate: %#v", pending)
		}

		nilCreate := pendingCandidate("nil-update-create", scope, at.Add(2*time.Second))
		nilCreate.Proposed.Key = "nil-update-key"
		propose(t, store, nilCreate)
		nilTarget, err := store.Review(ctx, memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: nilCreate.ID}, ResultRecordID: "nil-update-target", Decision: memory.ReviewAccept, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(3 * time.Second)})
		if err != nil || nilTarget.Record == nil {
			t.Fatalf("nil update target create = %#v, %v", nilTarget, err)
		}
		nilUpdate := pendingCandidate("nil-update-candidate", scope, at.Add(4*time.Second))
		nilUpdate.Action, nilUpdate.TargetID, nilUpdate.BaseRevision = memory.CandidateUpdate, nilTarget.Record.ID, 1
		nilUpdate.Proposed.Text = "accepted proposed content without edit"
		propose(t, store, nilUpdate)
		nilUpdated, err := store.Review(ctx, memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: nilUpdate.ID}, Decision: memory.ReviewAccept, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(5 * time.Second)})
		if err != nil || nilUpdated.Record == nil || nilUpdated.Record.Text != nilUpdate.Proposed.Text || nilUpdated.Record.Revision != 2 {
			t.Fatalf("nil edited update = %#v, %v", nilUpdated, err)
		}

		target := *accepted.Record
		update := pendingCandidate("update-candidate", scope, at.Add(4*time.Second))
		update.Action, update.TargetID, update.BaseRevision = memory.CandidateUpdate, target.ID, target.Revision
		update.Proposed.Text, update.Proposed.Metadata = "intended update", map[string]string{"base": "proposal"}
		propose(t, store, update)
		otherBase, err := fixture.Reopen()
		if err != nil {
			t.Fatal(err)
		}
		other := otherBase.(CandidateStore)
		changed := memory.CloneRecord(target)
		changed.Metadata = map[string]string{"concurrent": "preserve"}
		changed.UpdatedAt = at.Add(5 * time.Second)
		expected := uint64(1)
		current, err := other.Upsert(ctx, memory.UpsertRequest{Record: changed, ExpectedRevision: &expected})
		if err != nil {
			t.Fatal(err)
		}
		if _, err := store.Review(ctx, memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: update.ID}, Decision: memory.ReviewAccept, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(6 * time.Second)}); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("stale nil update = %v", err)
		}
		refreshed := memory.CloneRecord(current)
		refreshed.ID, refreshed.Revision, refreshed.CreatedAt, refreshed.UpdatedAt = "", 0, time.Time{}, time.Time{}
		refreshed.Text = "rebased intended update"
		refreshed.Source.DecisionAt, refreshed.Source.DecisionSource = nil, ""
		targetRevision := current.Revision
		updated, err := store.Review(ctx, memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: update.ID}, Decision: memory.ReviewAccept, Edited: &refreshed, TargetRevision: &targetRevision, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(6 * time.Second)})
		if err != nil || updated.Record == nil || updated.Record.Revision != 3 || updated.Record.Metadata["concurrent"] != "preserve" || updated.Record.Text != refreshed.Text {
			t.Fatalf("rebased update = %#v, %v", updated, err)
		}

		forget := pendingCandidate("forget-candidate", scope, at.Add(7*time.Second))
		forget.Action, forget.TargetID, forget.BaseRevision = memory.CandidateForget, target.ID, 3
		forget.Proposed = memory.Record{Scope: scope, Source: memory.Provenance{Origin: memory.OriginModel}}
		propose(t, store, forget)
		concurrent := memory.CloneRecord(*updated.Record)
		concurrent.Text, concurrent.UpdatedAt = "concurrent update before forget rebase", at.Add(8*time.Second)
		expected = 3
		current, err = other.Upsert(ctx, memory.UpsertRequest{Record: concurrent, ExpectedRevision: &expected})
		if err != nil || current.Revision != 4 {
			t.Fatalf("concurrent forget target = %#v, %v", current, err)
		}
		if _, err := store.Review(ctx, memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: forget.ID}, Decision: memory.ReviewAccept, DecisionSource: memory.OriginMigration, DecidedAt: at.Add(9 * time.Second)}); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("stale forget = %v", err)
		}
		forgetRevision := uint64(4)
		forgotten, err := store.Review(ctx, memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: forget.ID}, Decision: memory.ReviewAccept, TargetRevision: &forgetRevision, DecisionSource: memory.OriginMigration, DecidedAt: at.Add(9 * time.Second)})
		if err != nil || forgotten.Tombstone == nil || forgotten.Tombstone.Revision != 5 || forgotten.Candidate.ResultRevision != 5 {
			t.Fatalf("rebased forget = %#v, %v", forgotten, err)
		}
		if _, err := store.Get(ctx, memory.RecordRef{Scope: scope, ID: target.ID}); !errors.Is(err, memory.ErrNotFound) {
			t.Fatalf("forgotten active read = %v", err)
		}
	})

	t.Run("two review handles have exactly one winner", func(t *testing.T) {
		fixture, first := newFixture(t)
		defer fixture.Cleanup()
		at := time.Date(2026, 8, 29, 20, 50, 0, 0, time.UTC)
		scope := memory.Scope{Namespace: "user", ID: "review-race"}
		candidate := pendingCandidate("race-candidate", scope, at)
		propose(t, first, candidate)
		secondBase, err := fixture.Reopen()
		if err != nil {
			t.Fatal(err)
		}
		second := secondBase.(CandidateStore)
		request := memory.StoreReviewRequest{Ref: memory.CandidateRef{Scope: scope, ID: candidate.ID}, Decision: memory.ReviewReject, DecisionSource: memory.OriginHuman, DecidedAt: at.Add(time.Second)}
		errorsCh := make(chan error, 2)
		start := make(chan struct{})
		for _, store := range []CandidateStore{first, second} {
			go func(store CandidateStore) {
				<-start
				_, err := store.Review(context.Background(), request)
				errorsCh <- err
			}(store)
		}
		close(start)
		successes, conflicts := 0, 0
		for range 2 {
			err := <-errorsCh
			if err == nil {
				successes++
			} else if errors.Is(err, memory.ErrConflict) {
				conflicts++
			} else {
				t.Fatalf("race error = %v", err)
			}
		}
		if successes != 1 || conflicts != 1 {
			t.Fatalf("race successes=%d conflicts=%d", successes, conflicts)
		}
	})
}

func pendingCandidate(id string, scope memory.Scope, at time.Time) memory.Candidate {
	return memory.Candidate{
		ID: id, Action: memory.CandidateCreate, State: memory.CandidatePending, CreatedAt: at, Reason: "candidate reason",
		Proposed: memory.Record{
			Scope: scope, Kind: "note", Key: id, Text: "candidate proposed text", Labels: []string{"candidate"},
			Metadata: map[string]string{"purpose": "test"}, Confidence: 0.75,
			Source: memory.Provenance{Origin: memory.OriginModel, SessionID: "candidate-session", MessageIDs: []string{"candidate-message"}},
		},
	}
}

func assertNormalizedCollections(t *testing.T, record memory.Record) {
	t.Helper()
	if record.Labels == nil || len(record.Labels) != 0 || record.Metadata == nil || len(record.Metadata) != 0 ||
		record.Source.MessageIDs == nil || len(record.Source.MessageIDs) != 0 {
		t.Fatalf("collections not normalized: labels=%#v metadata=%#v messages=%#v", record.Labels, record.Metadata, record.Source.MessageIDs)
	}
}

func cursorWithVersion(t *testing.T, value string, version int) string {
	t.Helper()
	raw, err := base64.RawURLEncoding.DecodeString(value)
	if err != nil {
		t.Fatal(err)
	}
	var payload map[string]any
	if err := json.Unmarshal(raw, &payload); err != nil {
		t.Fatal(err)
	}
	payload["v"] = version
	raw, err = json.Marshal(payload)
	if err != nil {
		t.Fatal(err)
	}
	return base64.RawURLEncoding.EncodeToString(raw)
}

func fullRecord(id, key string, at time.Time) memory.Record {
	decision := at.Add(-time.Nanosecond)
	expires := at.Add(24 * time.Hour)
	return memory.Record{
		ID: id, Scope: memory.Scope{Namespace: "user", ID: "user-conformance"}, Kind: "note", Key: key,
		Text: "persistent memory text", Labels: []string{"alpha", "beta"},
		Metadata:   map[string]string{"language": "go", "purpose": "test"},
		Source:     memory.Provenance{Origin: memory.OriginModel, SessionID: "session-1", MessageIDs: []string{"message-1", "message-2"}, ObservationID: "observation-1", DecisionAt: &decision, DecisionSource: memory.OriginHuman},
		Confidence: 0.875, Revision: 0, CreatedAt: at, UpdatedAt: at, ExpiresAt: &expires,
	}
}

func mustCreate(t *testing.T, store RecordStore, record memory.Record) memory.Record {
	t.Helper()
	created, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: record})
	if err != nil {
		t.Fatal(err)
	}
	return created
}

func timePtr(value time.Time) *time.Time { return &value }

func contains(values []string, value string) bool {
	sorted := appendSorted(values)
	index := sort.SearchStrings(sorted, value)
	return index < len(sorted) && sorted[index] == value
}

func appendSorted(values []string) []string {
	copyValues := append([]string(nil), values...)
	sort.Strings(copyValues)
	return copyValues
}
