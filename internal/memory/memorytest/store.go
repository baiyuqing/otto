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

// Fixture owns one fresh database. Reopen opens the same database after Store
// has been closed; Cleanup must be safe after either store has been closed.
type Fixture struct {
	Store   RecordStore
	Reopen  func() (RecordStore, error)
	Cleanup func()
}

// Factory returns a fresh isolated fixture for each conformance subtest.
type Factory func(*testing.T) Fixture

// RunRecordConformance checks behavior shared by persistent record stores.
func RunRecordConformance(t *testing.T, factory Factory) {
	t.Helper()

	t.Run("create identity aliasing and reopen", func(t *testing.T) {
		fixture := factory(t)
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
		fixture := factory(t)
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
		fixture := factory(t)
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
		fixture := factory(t)
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
	})
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
