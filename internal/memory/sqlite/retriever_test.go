package sqlite

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

func TestRetrieverLiteralScopeAndEmptyBaseline(t *testing.T) {
	path := t.TempDir() + "/retrieve.sqlite"
	store := openTestStore(t, path)
	defer store.Close()

	now := time.Date(2026, 8, 29, 12, 0, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "retrieval-user"}
	other := memory.Scope{Namespace: memory.NamespaceUser, ID: "other-user"}
	for _, value := range []struct {
		id, kind, key, text string
		scope               memory.Scope
	}{
		{"pref", "preference", "editor", "prefers vim", user},
		{"keyless", "note", "", "vim keyless", user},
		{"other", "note", "other", "vim elsewhere", other},
		{"sentinel", "note", "sentinel", "in-scope nonmatching sentinel", user},
	} {
		record := sqliteTestRecord(value.id, value.key, now)
		record.Scope, record.Kind, record.Text = value.scope, value.kind, value.text
		record.ExpiresAt = nil
		mustSQLiteCreate(t, store, record)
	}

	base := memory.RetrievalRequest{Scopes: []memory.Scope{user}, IncludeBaseline: true, Limit: 10, TokenBudget: 1000, Now: now, EstimateTokens: func(string) int { return 1 }}
	got, err := store.Retrieve(context.Background(), base)
	if err != nil || len(got.Matches) != 1 || got.Matches[0].Record.ID != "pref" {
		t.Fatalf("baseline = %#v, %v", got, err)
	}
	base.IncludeBaseline = false
	got, err = store.Retrieve(context.Background(), base)
	if err != nil || len(got.Matches) != 0 {
		t.Fatalf("empty = %#v, %v", got, err)
	}
	base.Query = `vim OR " OR 1=1 --`
	got, err = store.Retrieve(context.Background(), base)
	if err != nil || len(got.Matches) != 2 || got.Matches[0].Record.ID != "keyless" || got.Matches[1].Record.ID != "pref" {
		t.Fatalf("literal = %#v, %v", got, err)
	}
}

func TestRetrieverWorkspaceReplacementAndLabels(t *testing.T) {
	store := openTestStore(t, t.TempDir()+"/replacement.sqlite")
	defer store.Close()
	now := time.Date(2026, 8, 29, 12, 0, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "replacement-user"}
	workspace := memory.Scope{Namespace: memory.NamespaceWorkspace, ID: "replacement-workspace"}
	userRecord := sqliteTestRecord("user-value", "editor", now)
	userRecord.Scope, userRecord.Kind, userRecord.Text, userRecord.ExpiresAt = user, "preference", "needle user value", nil
	workspaceRecord := sqliteTestRecord("workspace-value", "editor", now.Add(time.Second))
	workspaceRecord.Scope, workspaceRecord.Kind, workspaceRecord.Text, workspaceRecord.ExpiresAt = workspace, "preference", "workspace override", nil
	workspaceRecord.Labels = []string{"workspace-only"}
	mustSQLiteCreate(t, store, userRecord)
	mustSQLiteCreate(t, store, workspaceRecord)

	request := memory.RetrievalRequest{Query: "needle", Scopes: []memory.Scope{user, workspace}, Limit: 10, TokenBudget: 100, Now: now, EstimateTokens: func(string) int { return 1 }}
	got, err := store.Retrieve(context.Background(), request)
	if err != nil || len(got.Matches) != 1 || got.Matches[0].Record.ID != "workspace-value" {
		t.Fatalf("replacement = %#v, %v", got, err)
	}
	request.Labels = []string{"alpha"}
	got, err = store.Retrieve(context.Background(), request)
	if err != nil || len(got.Matches) != 0 {
		t.Fatalf("filtered replacement resurrected user = %#v, %v", got, err)
	}
}

func TestRetrieverPaginationBudgetAndGeneration(t *testing.T) {
	store := openTestStore(t, t.TempDir()+"/pages.sqlite")
	defer store.Close()
	now := time.Date(2026, 8, 29, 12, 0, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "page-user"}
	for index, id := range []string{"page-a", "page-b", "page-c", "page-d", "page-e"} {
		record := sqliteTestRecord(id, id, now.Add(time.Duration(5-index)*time.Second))
		record.Scope, record.Text, record.ExpiresAt = user, "pagination "+id, nil
		mustSQLiteCreate(t, store, record)
	}
	request := memory.RetrievalRequest{Query: "pagination", Scopes: []memory.Scope{user}, Limit: 2, TokenBudget: 10, Now: now, EstimateTokens: func(string) int { return 1 }}
	var ids []string
	var ranks []int
	for {
		page, err := store.Retrieve(context.Background(), request)
		if err != nil {
			t.Fatal(err)
		}
		for _, match := range page.Matches {
			ids, ranks = append(ids, match.Record.ID), append(ranks, match.Rank)
		}
		if page.NextCursor == "" {
			break
		}
		request.Cursor = page.NextCursor
	}
	if strings.Join(ids, ",") != "page-a,page-b,page-c,page-d,page-e" || len(ranks) != 5 {
		t.Fatalf("pages = %v ranks %v", ids, ranks)
	}
	for index, rank := range ranks {
		if rank != index+1 {
			t.Fatalf("ranks = %v", ranks)
		}
	}

	request.Cursor = ""
	first, err := store.Retrieve(context.Background(), request)
	if err != nil || first.NextCursor == "" {
		t.Fatalf("first = %#v, %v", first, err)
	}
	mismatch := request
	mismatch.Cursor, mismatch.Labels = first.NextCursor, []string{"alpha"}
	if _, err := store.Retrieve(context.Background(), mismatch); !errors.Is(err, memory.ErrInvalidCursor) {
		t.Fatalf("cross-filter cursor = %v", err)
	}
	mutation := sqliteTestRecord("page-mutation", "page-mutation", now.Add(time.Hour))
	mutation.Scope, mutation.Text, mutation.ExpiresAt = user, "pagination mutation", nil
	mustSQLiteCreate(t, store, mutation)
	request.Cursor = first.NextCursor
	if _, err := store.Retrieve(context.Background(), request); !errors.Is(err, memory.ErrConflict) {
		t.Fatalf("stale cursor = %v", err)
	}
}

func TestRetrieverExpiryDedupeTombstoneAndAliases(t *testing.T) {
	store := openTestStore(t, t.TempDir()+"/filters.sqlite")
	defer store.Close()
	now := time.Date(2026, 8, 29, 12, 0, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "filter-user"}
	workspace := memory.Scope{Namespace: memory.NamespaceWorkspace, ID: "filter-workspace"}

	userDuplicate := sqliteTestRecord("duplicate-user", "user-key", now.Add(time.Hour))
	userDuplicate.Scope, userDuplicate.Text, userDuplicate.ExpiresAt = user, "duplicate content", nil
	workspaceDuplicate := sqliteTestRecord("duplicate-workspace", "workspace-key", now)
	workspaceDuplicate.Scope, workspaceDuplicate.Text, workspaceDuplicate.ExpiresAt = workspace, "duplicate content", nil
	expired := sqliteTestRecord("expired", "expired", now)
	expired.Scope, expired.Text = user, "expired searchable"
	expiry := now
	expired.ExpiresAt = &expiry
	tombstoned := sqliteTestRecord("forgotten", "forgotten", now)
	tombstoned.Scope, tombstoned.Text, tombstoned.ExpiresAt = user, "forgotten searchable", nil
	for _, record := range []memory.Record{userDuplicate, workspaceDuplicate, expired, tombstoned} {
		mustSQLiteCreate(t, store, record)
	}
	created, err := store.Get(context.Background(), memory.RecordRef{Scope: user, ID: "forgotten"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.Forget(context.Background(), memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: user, ID: "forgotten"}, ExpectedRevision: created.Revision, ForgottenAt: now.Add(2 * time.Hour)}); err != nil {
		t.Fatal(err)
	}

	request := memory.RetrievalRequest{Query: "duplicate", Scopes: []memory.Scope{user, workspace}, Kinds: []string{"note"}, Labels: []string{"alpha"}, Limit: 10, TokenBudget: 100, Now: now, EstimateTokens: func(string) int { return 1 }}
	first, err := store.Retrieve(context.Background(), request)
	if err != nil || len(first.Matches) != 1 || first.Matches[0].Record.ID != "duplicate-workspace" {
		t.Fatalf("dedupe precedence = %#v, %v", first, err)
	}
	first.Matches[0].Record.Text = "aliased"
	first.Matches[0].Record.Labels[0] = "aliased"
	again, err := store.Retrieve(context.Background(), request)
	if err != nil || again.Matches[0].Record.Text == "aliased" || again.Matches[0].Record.Labels[0] == "aliased" {
		t.Fatalf("result alias = %#v, %v", again, err)
	}

	request.Query, request.Scopes, request.Labels = "expired", []memory.Scope{user}, nil
	got, err := store.Retrieve(context.Background(), request)
	if err != nil || len(got.Matches) != 0 {
		t.Fatalf("excluded expiry = %#v, %v", got, err)
	}
	request.IncludeExpired = true
	got, err = store.Retrieve(context.Background(), request)
	if err != nil || len(got.Matches) != 1 || got.Matches[0].Record.ID != "expired" {
		t.Fatalf("included expiry = %#v, %v", got, err)
	}
	request.Query, request.IncludeExpired = "forgotten", false
	got, err = store.Retrieve(context.Background(), request)
	if err != nil || len(got.Matches) != 0 {
		t.Fatalf("tombstone = %#v, %v", got, err)
	}
}

func TestRetrieverCorruptOversizedProjectionIsSafe(t *testing.T) {
	path := t.TempDir() + "/corrupt.sqlite"
	store := openTestStore(t, path)
	defer store.Close()
	now := time.Date(2026, 8, 29, 12, 0, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "corrupt-user"}
	record := sqliteTestRecord("corrupt-record", "corrupt-key", now)
	record.Scope, record.Kind, record.Text, record.ExpiresAt = user, "preference", "safe original", nil
	mustSQLiteCreate(t, store, record)
	external := openExternalSQLite(t, path)
	defer external.Close()
	secret := strings.Repeat("sensitive-oversized-value", 400)
	if _, err := external.Exec(`UPDATE memory_records SET text_value=? WHERE id=?`, secret, record.ID); err != nil {
		t.Fatal(err)
	}
	request := memory.RetrievalRequest{Scopes: []memory.Scope{user}, IncludeBaseline: true, Limit: 10, TokenBudget: 100, Now: now, EstimateTokens: func(string) int { return 1 }}
	_, err := store.Retrieve(context.Background(), request)
	if !errors.Is(err, memory.ErrCorrupt) || strings.Contains(err.Error(), secret) {
		t.Fatalf("corrupt error = %v", err)
	}
}

func TestRetrieverOversizedProgressAndEstimatorInput(t *testing.T) {
	store := openTestStore(t, t.TempDir()+"/budget.sqlite")
	defer store.Close()
	now := time.Date(2026, 8, 29, 12, 0, 0, 0, time.UTC)
	user := memory.Scope{Namespace: memory.NamespaceUser, ID: "budget-user"}
	for index, id := range []string{"oversized", "fits-one", "fits-two"} {
		record := sqliteTestRecord(id, id, now.Add(time.Duration(3-index)*time.Second))
		record.Scope, record.Text, record.ExpiresAt = user, "budget "+id, nil
		record.Labels = []string{"z", "a"}
		mustSQLiteCreate(t, store, record)
	}
	var inputs []string
	estimator := func(value string) int {
		inputs = append(inputs, value)
		if strings.Contains(value, `"id":"oversized"`) {
			return 4
		}
		return 2
	}
	request := memory.RetrievalRequest{Query: "budget", Scopes: []memory.Scope{user}, Limit: 3, TokenBudget: 3, Now: now, EstimateTokens: estimator}
	first, err := store.Retrieve(context.Background(), request)
	if err != nil || len(first.Matches) != 1 || first.Matches[0].Record.ID != "fits-one" || first.UsedTokens != 2 || first.NextCursor == "" {
		t.Fatalf("first = %#v, %v", first, err)
	}
	if len(inputs) < 2 || !strings.Contains(inputs[0], `"scope"`) || !strings.Contains(inputs[0], `"labels":["a","z"]`) {
		t.Fatalf("estimator input = %q", inputs)
	}
	request.Cursor = first.NextCursor
	second, err := store.Retrieve(context.Background(), request)
	if err != nil || len(second.Matches) != 1 || second.Matches[0].Record.ID != "fits-two" || second.NextCursor != "" {
		t.Fatalf("second = %#v, %v", second, err)
	}
}

var _ memory.Retriever = (*Store)(nil)
