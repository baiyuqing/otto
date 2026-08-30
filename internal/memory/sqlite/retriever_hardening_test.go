package sqlite

import (
	"context"
	"database/sql"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

func TestRetrieverLabelsFollowWorkspaceReplacement(t *testing.T) {
	now := time.Date(2026, 8, 29, 19, 0, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "inverse-label-user"}
	workspace := memory.Scope{Namespace: memory.NamespaceWorkspace, ID: "inverse-label-workspace"}

	for _, test := range []struct {
		name            string
		query           string
		includeBaseline bool
		userText        string
		workspaceText   string
	}{
		{"query seed", "needle", false, "needle only in user seed", "nonmatching workspace override"},
		{"baseline seed", "", true, "user baseline", "workspace baseline override"},
	} {
		t.Run(test.name, func(t *testing.T) {
			store := openTestStore(t, t.TempDir()+"/inverse-label.sqlite")
			userRecord := sqliteTestRecord("inverse-user", "editor", now)
			userRecord.Scope, userRecord.Kind, userRecord.Text, userRecord.Labels, userRecord.ExpiresAt = user, "preference", test.userText, []string{"user-only"}, nil
			workspaceRecord := sqliteTestRecord("inverse-workspace", "editor", now.Add(time.Second))
			workspaceRecord.Scope, workspaceRecord.Kind, workspaceRecord.Text, workspaceRecord.Labels, workspaceRecord.ExpiresAt = workspace, "preference", test.workspaceText, []string{"wanted"}, nil
			mustSQLiteCreate(t, store, userRecord)
			mustSQLiteCreate(t, store, workspaceRecord)

			request := memory.RetrievalRequest{
				Query: test.query, Scopes: []memory.Scope{user, workspace}, Labels: []string{"wanted"}, IncludeBaseline: test.includeBaseline,
				Limit: 10, TokenBudget: 100, Now: now, EstimateTokens: func(string) int { return 1 },
			}
			got, err := store.Retrieve(context.Background(), request)
			if err != nil || len(got.Matches) != 1 || got.Matches[0].Record.ID != workspaceRecord.ID {
				t.Fatalf("replacement = %#v, %v", got, err)
			}
		})
	}
}

func TestRetrieverKindFilteredReplacementDoesNotSuppressUser(t *testing.T) {
	store := openTestStore(t, t.TempDir()+"/kind-replacement.sqlite")
	now := time.Date(2026, 8, 29, 19, 5, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "kind-user"}
	workspace := memory.Scope{Namespace: memory.NamespaceWorkspace, ID: "kind-workspace"}
	userRecord := sqliteTestRecord("kind-user-value", "shared", now)
	userRecord.Scope, userRecord.Kind, userRecord.Text, userRecord.ExpiresAt = user, "preference", "kind needle", nil
	workspaceRecord := sqliteTestRecord("kind-workspace-value", "shared", now.Add(time.Second))
	workspaceRecord.Scope, workspaceRecord.Kind, workspaceRecord.Text, workspaceRecord.ExpiresAt = workspace, "instruction", "different kind override", nil
	mustSQLiteCreate(t, store, userRecord)
	mustSQLiteCreate(t, store, workspaceRecord)

	request := memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{user, workspace}, Kinds: []string{"preference"}, Limit: 10, TokenBudget: 100, Now: now, EstimateTokens: func(string) int { return 1 }}
	got, err := store.Retrieve(context.Background(), request)
	if err != nil || len(got.Matches) != 1 || got.Matches[0].Record.ID != userRecord.ID {
		t.Fatalf("kind-filtered replacement = %#v, %v", got, err)
	}
}

func TestRetrieverSelectedSemanticLabelCorruptionIsNotFilteredOut(t *testing.T) {
	path := t.TempDir() + "/semantic-label-corruption.sqlite"
	store := openTestStore(t, path)
	now := time.Date(2026, 8, 29, 19, 10, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "corrupt-label-user"}
	record := sqliteTestRecord("corrupt-label", "corrupt-label", now)
	record.Scope, record.Text, record.ExpiresAt = user, "semantic-corruption needle", nil
	mustSQLiteCreate(t, store, record)
	external := openExternalSQLite(t, path)
	if _, err := external.Exec(`UPDATE memory_records SET labels_json='["alpha","alpha"]' WHERE id=?`, record.ID); err != nil {
		t.Fatal(err)
	}
	if err := external.Close(); err != nil {
		t.Fatal(err)
	}

	request := memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{user}, Labels: []string{"wanted"}, Limit: 10, TokenBudget: 100, Now: now, EstimateTokens: func(string) int { return 1 }}
	_, err := store.Retrieve(context.Background(), request)
	if !errors.Is(err, memory.ErrCorrupt) || strings.Contains(fmt.Sprint(err), "alpha") {
		t.Fatalf("semantic label corruption = %v", err)
	}
}

func TestRetrieverSelectedBaselineSemanticLabelCorruptionIsNotFilteredOut(t *testing.T) {
	path := t.TempDir() + "/baseline-label-corruption.sqlite"
	store := openTestStore(t, path)
	now := time.Date(2026, 8, 29, 19, 12, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "corrupt-baseline-label-user"}
	record := sqliteTestRecord("corrupt-baseline-label", "corrupt-baseline-label", now)
	record.Scope, record.Kind, record.Text, record.ExpiresAt = user, "preference", "semantic baseline corruption", nil
	mustSQLiteCreate(t, store, record)
	external := openExternalSQLite(t, path)
	if _, err := external.Exec(`UPDATE memory_records SET labels_json='["alpha","alpha"]' WHERE id=?`, record.ID); err != nil {
		t.Fatal(err)
	}
	if err := external.Close(); err != nil {
		t.Fatal(err)
	}

	request := memory.RetrievalRequest{Scopes: []memory.Scope{user}, Labels: []string{"wanted"}, IncludeBaseline: true, Limit: 10, TokenBudget: 100, Now: now, EstimateTokens: func(string) int { return 1 }}
	_, err := store.Retrieve(context.Background(), request)
	if !errors.Is(err, memory.ErrCorrupt) || strings.Contains(fmt.Sprint(err), "alpha") {
		t.Fatalf("semantic baseline label corruption = %v", err)
	}
}

func TestDedupeRetrievalCandidatesUsesIndependentWinnerPrecedence(t *testing.T) {
	now := time.Date(2026, 8, 29, 19, 15, 0, 0, time.UTC)
	userRecord := sqliteTestRecord("dedupe-user", "user", now.Add(time.Hour))
	workspaceRecord := sqliteTestRecord("dedupe-workspace", "workspace", now)
	workspaceRecord.Scope.Namespace = memory.NamespaceWorkspace
	userRecord.Text, workspaceRecord.Text = " same duplicate ", "same duplicate"

	t.Run("workspace beats baseline-first user and keeps baseline class", func(t *testing.T) {
		got := dedupeRetrievalCandidates([]retrievalCandidate{
			makeRetrievalCandidate(userRecord, true, 1),
			makeRetrievalCandidate(workspaceRecord, false, 2),
		})
		if len(got) != 1 || got[0].record.ID != workspaceRecord.ID || !got[0].baseline {
			t.Fatalf("dedupe = %#v", got)
		}
	})

	t.Run("newer beats better lexical rank", func(t *testing.T) {
		older := userRecord
		older.ID, older.UpdatedAt = "dedupe-older", now
		newer := userRecord
		newer.ID, newer.UpdatedAt = "dedupe-newer", now.Add(time.Second)
		got := dedupeRetrievalCandidates([]retrievalCandidate{
			makeRetrievalCandidate(older, false, 1),
			makeRetrievalCandidate(newer, false, 2),
		})
		if len(got) != 1 || got[0].record.ID != newer.ID {
			t.Fatalf("dedupe = %#v", got)
		}
	})
}

func TestRetrieverNewestExactBaselineCap(t *testing.T) {
	store := openTestStore(t, t.TempDir()+"/baseline-cap.sqlite")
	now := time.Date(2026, 8, 29, 19, 20, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "baseline-cap-user"}
	for index := 0; index < memory.MaxBaselineRecords+1; index++ {
		record := sqliteTestRecord(fmt.Sprintf("baseline-%02d", index), fmt.Sprintf("key-%02d", index), now.Add(time.Duration(index)*time.Second))
		record.Scope, record.Kind, record.Text, record.ExpiresAt = user, "preference", fmt.Sprintf("baseline value %02d", index), nil
		mustSQLiteCreate(t, store, record)
	}
	request := memory.RetrievalRequest{Scopes: []memory.Scope{user}, IncludeBaseline: true, Limit: 100, TokenBudget: 100, Now: now, EstimateTokens: func(string) int { return 1 }}
	got, err := store.Retrieve(context.Background(), request)
	if err != nil || len(got.Matches) != memory.MaxBaselineRecords {
		t.Fatalf("baseline count = %d, %v", len(got.Matches), err)
	}
	for index, match := range got.Matches {
		want := fmt.Sprintf("baseline-%02d", memory.MaxBaselineRecords-index)
		if match.Record.ID != want {
			t.Fatalf("baseline[%d] = %s, want %s", index, match.Record.ID, want)
		}
	}
}

func TestRetrieverEstimatorBoundariesAndExactWire(t *testing.T) {
	store := openTestStore(t, t.TempDir()+"/estimator-wire.sqlite")
	now := time.Date(2026, 8, 29, 19, 25, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "estimator-user"}
	record := sqliteTestRecord("wire-record", "wire-key", now)
	record.Scope, record.Kind, record.Text, record.Labels, record.ExpiresAt = user, "note", "wire needle", []string{"z", "a"}, nil
	mustSQLiteCreate(t, store, record)

	var wire string
	request := memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{user}, Limit: 1, TokenBudget: 10, Now: now, EstimateTokens: func(value string) int { wire = value; return 0 }}
	got, err := store.Retrieve(context.Background(), request)
	wantWire := `{"id":"wire-record","scope":{"Namespace":"user","ID":"estimator-user"},"kind":"note","key":"wire-key","labels":["a","z"],"text":"wire needle"}`
	if err != nil || len(got.Matches) != 1 || got.UsedTokens != 1 || wire != wantWire {
		t.Fatalf("zero estimate = %#v, %v, wire %q", got, err, wire)
	}
	request.EstimateTokens = func(string) int { return -1 }
	got, err = store.Retrieve(context.Background(), request)
	if err != nil || got.UsedTokens != 1 {
		t.Fatalf("negative estimate = %#v, %v", got, err)
	}
	request.EstimateTokens = func(string) int { return math.MaxInt }
	got, err = store.Retrieve(context.Background(), request)
	if err != nil || len(got.Matches) != 0 || got.UsedTokens != 0 || got.NextCursor != "" {
		t.Fatalf("MaxInt estimate = %#v, %v", got, err)
	}
}

func TestRetrieverSkippedRanksAndTerminalOversizedLookahead(t *testing.T) {
	store := openTestStore(t, t.TempDir()+"/skipped-ranks.sqlite")
	now := time.Date(2026, 8, 29, 19, 30, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "skipped-rank-user"}
	for index, id := range []string{"oversized-first", "fits-second", "fits-third"} {
		record := sqliteTestRecord(id, id, now.Add(time.Duration(3-index)*time.Second))
		record.Scope, record.Text, record.ExpiresAt = user, "rank needle "+id, nil
		mustSQLiteCreate(t, store, record)
	}
	estimator := func(value string) int {
		if strings.Contains(value, `"id":"oversized-first"`) {
			return math.MaxInt
		}
		return 1
	}
	request := memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{user}, Limit: 2, TokenBudget: 2, Now: now, EstimateTokens: estimator}
	got, err := store.Retrieve(context.Background(), request)
	if err != nil || len(got.Matches) != 2 || got.Matches[0].Rank != 2 || got.Matches[1].Rank != 3 || got.UsedTokens != 2 || got.NextCursor != "" {
		t.Fatalf("skipped ranks = %#v, %v", got, err)
	}

	request.Limit = 1
	request.EstimateTokens = func(value string) int {
		if strings.Contains(value, `"id":"fits-third"`) {
			return math.MaxInt
		}
		return 1
	}
	got, err = store.Retrieve(context.Background(), request)
	if err != nil || len(got.Matches) != 1 || got.Matches[0].Record.ID != "oversized-first" || got.NextCursor == "" {
		t.Fatalf("first full page = %#v, %v", got, err)
	}
	request.Cursor = got.NextCursor
	got, err = store.Retrieve(context.Background(), request)
	if err != nil || len(got.Matches) != 1 || got.Matches[0].Record.ID != "fits-second" || got.NextCursor != "" {
		t.Fatalf("full page followed only by oversized = %#v, %v", got, err)
	}
}

func TestRetrieverRefusesForgedCursorTuple(t *testing.T) {
	store := openTestStore(t, t.TempDir()+"/forged-retrieval-cursor.sqlite")
	now := time.Date(2026, 8, 29, 19, 35, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "forged-cursor-user"}
	for index := range 3 {
		record := sqliteTestRecord(fmt.Sprintf("forged-%d", index), fmt.Sprintf("forged-%d", index), now.Add(time.Duration(3-index)*time.Second))
		record.Scope, record.Text, record.ExpiresAt = user, fmt.Sprintf("cursor needle %d", index), nil
		mustSQLiteCreate(t, store, record)
	}
	request := memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{user}, Limit: 1, TokenBudget: 10, Now: now, EstimateTokens: func(string) int { return 1 }}
	first, err := store.Retrieve(context.Background(), request)
	if err != nil || first.NextCursor == "" {
		t.Fatalf("first = %#v, %v", first, err)
	}
	for _, cursor := range []string{
		mutateRetrievalCursor(t, first.NextCursor, func(value *retrievalCursor) { value.Ordinal++ }),
		mutateRetrievalCursor(t, first.NextCursor, func(value *retrievalCursor) { value.ID = "forged-valid-id" }),
	} {
		request.Cursor = cursor
		if _, err := store.Retrieve(context.Background(), request); !errors.Is(err, memory.ErrInvalidCursor) {
			t.Fatalf("forged cursor error = %v", err)
		}
	}
}

func TestRetrieverCoherentWALSnapshot(t *testing.T) {
	store := openTestStore(t, t.TempDir()+"/retrieval-snapshot.sqlite")
	now := time.Date(2026, 8, 29, 19, 40, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "snapshot-user"}
	workspace := memory.Scope{Namespace: memory.NamespaceWorkspace, ID: "snapshot-workspace"}
	seed := sqliteTestRecord("snapshot-user-record", "editor", now)
	seed.Scope, seed.Kind, seed.Text, seed.ExpiresAt = user, "preference", "snapshot needle", nil
	mustSQLiteCreate(t, store, seed)

	snapshotReady := make(chan struct{})
	writerDone := make(chan error, 1)
	stopWriter := make(chan struct{})
	var gated atomic.Bool
	installTestHooks(t, testHooks{beforeReadGeneration: func(conn *sql.Conn) {
		if !gated.CompareAndSwap(false, true) {
			return
		}
		var ignored string
		if err := conn.QueryRowContext(context.Background(), `SELECT value FROM memory_meta WHERE key='generation'`).Scan(&ignored); err != nil {
			writerDone <- err
			return
		}
		close(snapshotReady)
		if err := <-writerDone; err != nil {
			t.Errorf("snapshot writer: %v", err)
		}
	}})
	go func() {
		select {
		case <-snapshotReady:
			replacement := sqliteTestRecord("snapshot-workspace-record", "editor", now.Add(time.Second))
			replacement.Scope, replacement.Kind, replacement.Text, replacement.ExpiresAt = workspace, "preference", "new workspace value", nil
			_, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: replacement})
			writerDone <- err
		case <-stopWriter:
		}
	}()
	defer close(stopWriter)

	request := memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{user, workspace}, Limit: 10, TokenBudget: 10, Now: now, EstimateTokens: func(string) int { return 1 }}
	first, err := store.Retrieve(context.Background(), request)
	if err != nil || len(first.Matches) != 1 || first.Matches[0].Record.ID != seed.ID {
		t.Fatalf("snapshot result = %#v, %v", first, err)
	}
	second, err := store.Retrieve(context.Background(), request)
	if err != nil || len(second.Matches) != 1 || second.Matches[0].Record.ID != "snapshot-workspace-record" {
		t.Fatalf("post-snapshot result = %#v, %v", second, err)
	}
}

type retrievalResourceGuard struct {
	store   atomic.Pointer[Store]
	enabled atomic.Bool
	calls   atomic.Int64
	err     atomic.Value
}

func (guard *retrievalResourceGuard) Check(ctx context.Context, input memory.GuardInput) error {
	if guard.enabled.Load() {
		guard.calls.Add(1)
		if store := guard.store.Load(); store != nil {
			store.lifecycleMu.Lock()
			active := store.active
			store.lifecycleMu.Unlock()
			if active != 0 || len(store.writeGate) != 1 || len(store.connections) != retainedConnectionCount {
				guard.err.Store(fmt.Sprintf("guard resources active=%d write=%d connections=%d", active, len(store.writeGate), len(store.connections)))
			}
		}
	}
	return (memory.DefaultGuard{}).Check(ctx, input)
}

func TestRetrieverCallbacksReleaseResourcesAndCompositeSanitizes(t *testing.T) {
	t.Run("resources", func(t *testing.T) {
		guard := &retrievalResourceGuard{}
		store := openTestStore(t, t.TempDir()+"/retrieval-resources.sqlite", func(options *Options) { options.Guard = guard })
		guard.store.Store(store)
		now := time.Date(2026, 8, 29, 19, 45, 0, 0, time.UTC)
		user := memory.Scope{Namespace: memory.NamespaceUser, ID: "resource-user"}
		record := sqliteTestRecord("resource-record", "resource", now)
		record.Scope, record.Text, record.ExpiresAt = user, "resource needle", nil
		mustSQLiteCreate(t, store, record)
		guard.enabled.Store(true)
		var estimatorCalls atomic.Int64
		request := memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{user}, Limit: 1, TokenBudget: 10, Now: now, EstimateTokens: func(string) int {
			estimatorCalls.Add(1)
			store.lifecycleMu.Lock()
			active := store.active
			store.lifecycleMu.Unlock()
			if active != 0 || len(store.writeGate) != 1 || len(store.connections) != retainedConnectionCount {
				guard.err.Store(fmt.Sprintf("estimator resources active=%d write=%d connections=%d", active, len(store.writeGate), len(store.connections)))
			}
			return 1
		}}
		if _, err := store.Retrieve(context.Background(), request); err != nil {
			t.Fatal(err)
		}
		if value := guard.err.Load(); value != nil {
			t.Fatal(value)
		}
		if guard.calls.Load() != 1 || estimatorCalls.Load() != 1 {
			t.Fatalf("callback calls = guard %d estimator %d", guard.calls.Load(), estimatorCalls.Load())
		}
	})

	t.Run("composite unknown member error", func(t *testing.T) {
		leak := &switchingLeakGuard{}
		guard := memory.NewCompositeGuard(memory.DefaultGuard{}, leak)
		store := openTestStore(t, t.TempDir()+"/retrieval-composite.sqlite", func(options *Options) { options.Guard = guard })
		now := time.Date(2026, 8, 29, 19, 46, 0, 0, time.UTC)
		user := memory.Scope{Namespace: memory.NamespaceUser, ID: "composite-user"}
		record := sqliteTestRecord("composite-record", "composite", now)
		record.Scope, record.Text, record.ExpiresAt = user, "composite-secret needle", nil
		mustSQLiteCreate(t, store, record)
		leak.enabled.Store(true)
		request := memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{user}, Limit: 1, TokenBudget: 10, Now: now, EstimateTokens: func(string) int { return 1 }}
		_, err := store.Retrieve(context.Background(), request)
		if !errors.Is(err, memory.ErrUnavailable) || strings.Contains(fmt.Sprint(err), "composite-secret") {
			t.Fatalf("composite error = %v", err)
		}
	})
}

type switchingLeakGuard struct{ enabled atomic.Bool }

func (guard *switchingLeakGuard) Check(_ context.Context, input memory.GuardInput) error {
	if !guard.enabled.Load() {
		return nil
	}
	return fmt.Errorf("unsafe guard member: %s", input.Fields[len(input.Fields)-1].Value)
}

func TestRetrieverFinalCallbackWorkIsBounded(t *testing.T) {
	guard := &retrievalResourceGuard{}
	path := t.TempDir() + "/retrieval-ceiling.sqlite"
	store := openTestStore(t, path, func(options *Options) { options.Guard = guard })
	guard.store.Store(store)
	now := time.Date(2026, 8, 29, 19, 50, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "ceiling-user"}
	workspace := memory.Scope{Namespace: memory.NamespaceWorkspace, ID: "ceiling-workspace"}
	records := make([]memory.Record, 0, 2*memory.MaxBaselineRecords+memory.MaxRetrievalCandidates)
	for index := range memory.MaxBaselineRecords {
		key := fmt.Sprintf("baseline-%03d", index)
		record := sqliteTestRecord(fmt.Sprintf("ceiling-baseline-%03d", index), key, now.Add(time.Duration(index)*time.Second))
		record.Scope, record.Kind, record.Text, record.ExpiresAt = user, "preference", fmt.Sprintf("baseline only %03d", index), nil
		records = append(records, record)
		replacement := sqliteTestRecord(fmt.Sprintf("ceiling-replacement-%03d", index), key, now.Add(time.Duration(index+1)*time.Second))
		replacement.Scope, replacement.Kind, replacement.Text, replacement.ExpiresAt = workspace, "preference", fmt.Sprintf("workspace replacement only %03d", index), nil
		records = append(records, replacement)
	}
	for index := range memory.MaxRetrievalCandidates {
		record := sqliteTestRecord(fmt.Sprintf("ceiling-lexical-%03d", index), fmt.Sprintf("lexical-%03d", index), now.Add(time.Duration(index)*time.Second))
		record.Scope, record.Text, record.ExpiresAt = user, fmt.Sprintf("ceiling needle %03d", index), nil
		records = append(records, record)
	}
	bulkInsertRetrievalRecords(t, path, records)
	guard.enabled.Store(true)
	var estimatorCalls atomic.Int64
	request := memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{user, workspace}, IncludeBaseline: true, Limit: 100, TokenBudget: memory.MaxTokenBudget, Now: now, EstimateTokens: func(string) int {
		estimatorCalls.Add(1)
		return math.MaxInt
	}}
	got, err := store.Retrieve(context.Background(), request)
	if err != nil || len(got.Matches) != 0 || got.NextCursor != "" {
		t.Fatalf("bounded retrieval = %#v, %v", got, err)
	}
	if guard.calls.Load() != memory.MaxRetrievalCandidates || estimatorCalls.Load() != memory.MaxRetrievalCandidates {
		t.Fatalf("callback work = guard %d estimator %d", guard.calls.Load(), estimatorCalls.Load())
	}
}

func TestRetrieverLimitThreeOrderAndRanksStable(t *testing.T) {
	store := openTestStore(t, t.TempDir()+"/stable-three.sqlite")
	now := time.Date(2026, 8, 29, 19, 55, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "stable-user"}
	for index, id := range []string{"stable-a", "stable-b", "stable-c", "stable-d"} {
		record := sqliteTestRecord(id, id, now.Add(time.Duration(4-index)*time.Second))
		record.Scope, record.Text, record.ExpiresAt = user, "stable needle "+id, nil
		mustSQLiteCreate(t, store, record)
	}
	request := memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{user}, Limit: 3, TokenBudget: 10, Now: now, EstimateTokens: func(string) int { return 1 }}
	for run := range 100 {
		got, err := store.Retrieve(context.Background(), request)
		if err != nil || len(got.Matches) != 3 {
			t.Fatalf("run %d = %#v, %v", run, got, err)
		}
		for index, want := range []string{"stable-a", "stable-b", "stable-c"} {
			if got.Matches[index].Record.ID != want || got.Matches[index].Rank != index+1 {
				t.Fatalf("run %d match %d = %#v", run, index, got.Matches[index])
			}
		}
	}
}

func mutateRetrievalCursor(t *testing.T, value string, mutate func(*retrievalCursor)) string {
	t.Helper()
	raw, err := base64.RawURLEncoding.DecodeString(value)
	if err != nil {
		t.Fatal(err)
	}
	var cursor retrievalCursor
	if err := json.Unmarshal(raw, &cursor); err != nil {
		t.Fatal(err)
	}
	mutate(&cursor)
	raw, err = json.Marshal(cursor)
	if err != nil {
		t.Fatal(err)
	}
	return base64.RawURLEncoding.EncodeToString(raw)
}

func bulkInsertRetrievalRecords(t *testing.T, path string, records []memory.Record) {
	t.Helper()
	database := openExternalSQLite(t, path)
	defer database.Close()
	tx, err := database.Begin()
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	for _, record := range records {
		row := makeRawRecord(t, record)
		if _, err := tx.Exec(`INSERT INTO memory_records(
			id,scope_namespace,scope_id,kind,semantic_key,text_value,labels_json,metadata_json,source_json,
			confidence,revision,created_at,updated_at,expires_at,state,forgotten_at
		) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
			row.id, row.namespace, row.scopeID, row.kind, row.key, row.text, row.labels, row.metadata, row.source,
			row.confidence, row.revision, row.created, row.updated, row.expires, row.state, row.forgotten,
		); err != nil {
			t.Fatal(err)
		}
		if _, err := tx.Exec(`INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES(?,?,?,?,?)`,
			record.ID, record.Text, record.Kind, record.Key, ftsLabels(record.Labels)); err != nil {
			t.Fatal(err)
		}
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}
}
