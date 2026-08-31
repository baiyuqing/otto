package repl

import (
	"bytes"
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/memory"
)

type replMemoryBackend struct {
	fakeBackend
	searchMemory          func(context.Context, memory.SearchRequest) (memory.SearchResult, error)
	rememberMemory        func(context.Context, memory.RememberRequest) (memory.Record, error)
	forgetMemory          func(context.Context, memory.ForgetRequest) (memory.ForgetResult, error)
	reviewMemoryCandidate func(context.Context, memory.ReviewRequest) (memory.ReviewResult, error)
	getMemory             func(context.Context, memory.RecordRef) (memory.Record, error)
	memoryScopes          func() (memory.Scope, memory.Scope, bool)
}

func (b *replMemoryBackend) SearchMemory(ctx context.Context, request memory.SearchRequest) (memory.SearchResult, error) {
	if b.searchMemory == nil {
		return memory.SearchResult{}, app.ErrMemoryUnavailable
	}
	return b.searchMemory(ctx, request)
}

func (b *replMemoryBackend) RememberMemory(ctx context.Context, request memory.RememberRequest) (memory.Record, error) {
	if b.rememberMemory == nil {
		return memory.Record{}, app.ErrMemoryUnavailable
	}
	return b.rememberMemory(ctx, request)
}

func (b *replMemoryBackend) ForgetMemory(ctx context.Context, request memory.ForgetRequest) (memory.ForgetResult, error) {
	if b.forgetMemory == nil {
		return memory.ForgetResult{}, app.ErrMemoryUnavailable
	}
	return b.forgetMemory(ctx, request)
}

func (b *replMemoryBackend) ReviewMemoryCandidate(ctx context.Context, request memory.ReviewRequest) (memory.ReviewResult, error) {
	if b.reviewMemoryCandidate == nil {
		return memory.ReviewResult{}, app.ErrMemoryUnavailable
	}
	return b.reviewMemoryCandidate(ctx, request)
}

func (b *replMemoryBackend) GetMemory(ctx context.Context, ref memory.RecordRef) (memory.Record, error) {
	if b.getMemory == nil {
		return memory.Record{}, app.ErrMemoryUnavailable
	}
	return b.getMemory(ctx, ref)
}

func (b *replMemoryBackend) MemoryScopes() (memory.Scope, memory.Scope, bool) {
	if b.memoryScopes == nil {
		return memory.Scope{}, memory.Scope{}, false
	}
	return b.memoryScopes()
}

func replTestMemoryScopes() (memory.Scope, memory.Scope) {
	return memory.Scope{Namespace: "user", ID: "u1"}, memory.Scope{Namespace: "workspace", ID: "w1"}
}

func TestREPLHelpListsMemoryAndRememberCommands(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{}
	r := New(strings.NewReader("/help\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "/memory") || !strings.Contains(stdout.String(), "/remember") {
		t.Fatalf("help = %q", stdout.String())
	}
}

func TestREPLMemoryCommandWithoutManagerReturnsCommandError(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{}
	r := New(strings.NewReader("/memory search vim\n"), &stdout, &stderr, backend)
	err := r.Run(context.Background())
	if !errors.Is(err, app.ErrMemoryUnavailable) {
		t.Fatalf("Run() error = %v, want ErrMemoryUnavailable", err)
	}
	if !IsCommandError(err, "/memory") {
		t.Fatalf("Run() error = %v, want /memory command error", err)
	}
}

func TestREPLMemoryCommandUsageWithoutSubcommandContinues(t *testing.T) {
	userScope, workspaceScope := replTestMemoryScopes()
	backend := &replMemoryBackend{memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true }}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/memory\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stderr.String(), "usage:") {
		t.Fatalf("stderr = %q", stderr.String())
	}
}

func TestREPLMemorySearchCommandPrintsRecords(t *testing.T) {
	userScope, workspaceScope := replTestMemoryScopes()
	backend := &replMemoryBackend{
		memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true },
		searchMemory: func(_ context.Context, request memory.SearchRequest) (memory.SearchResult, error) {
			if request.Query != "vim" {
				t.Fatalf("query = %q, want vim", request.Query)
			}
			if len(request.Scopes) != 2 || request.Scopes[0] != userScope || request.Scopes[1] != workspaceScope {
				t.Fatalf("scopes = %#v", request.Scopes)
			}
			return memory.SearchResult{Records: []memory.Record{
				{ID: "rec-1", Scope: workspaceScope, Kind: "preference", Key: "editor", Text: "uses vim", Revision: 1},
			}}, nil
		},
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/memory search vim\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "id=rec-1") || !strings.Contains(stdout.String(), "uses vim") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLMemoryForgetCommandResolvesRevisionAcrossScopes(t *testing.T) {
	userScope, workspaceScope := replTestMemoryScopes()
	backend := &replMemoryBackend{
		memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true },
		getMemory: func(_ context.Context, ref memory.RecordRef) (memory.Record, error) {
			if ref.Scope == userScope {
				return memory.Record{}, memory.ErrNotFound
			}
			return memory.Record{ID: ref.ID, Scope: workspaceScope, Revision: 3}, nil
		},
		forgetMemory: func(_ context.Context, request memory.ForgetRequest) (memory.ForgetResult, error) {
			if request.ExpectedRevision != 3 || request.Ref.Scope != workspaceScope {
				t.Fatalf("forget request = %#v", request)
			}
			return memory.ForgetResult{Tombstone: memory.Tombstone{ID: request.Ref.ID}}, nil
		},
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/memory forget rec-1\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "forgot rec-1") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLMemoryForgetCommandNotFoundReturnsCommandError(t *testing.T) {
	userScope, workspaceScope := replTestMemoryScopes()
	backend := &replMemoryBackend{
		memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true },
		getMemory: func(_ context.Context, _ memory.RecordRef) (memory.Record, error) {
			return memory.Record{}, memory.ErrNotFound
		},
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/memory forget missing\n"), &stdout, &stderr, backend)
	err := r.Run(context.Background())
	if err == nil || !strings.Contains(err.Error(), "not found") {
		t.Fatalf("Run() error = %v, want not found error", err)
	}
	if !IsCommandError(err, "/memory forget") {
		t.Fatalf("Run() error = %v, want /memory forget command error", err)
	}
}

func TestREPLMemoryReviewCommandTriesScopesAndAppliesDecision(t *testing.T) {
	userScope, workspaceScope := replTestMemoryScopes()
	backend := &replMemoryBackend{
		memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true },
		reviewMemoryCandidate: func(_ context.Context, request memory.ReviewRequest) (memory.ReviewResult, error) {
			if request.Ref.Scope == userScope {
				return memory.ReviewResult{}, memory.ErrNotFound
			}
			if request.Ref.Scope != workspaceScope || request.Decision != memory.ReviewAccept {
				t.Fatalf("review request = %#v", request)
			}
			return memory.ReviewResult{}, nil
		},
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/memory review cand-1 accept\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "reviewed cand-1") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLMemoryReviewCommandRejectsInvalidDecisionContinues(t *testing.T) {
	userScope, workspaceScope := replTestMemoryScopes()
	backend := &replMemoryBackend{memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true }}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/memory review cand-1 maybe\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stderr.String(), "usage:") {
		t.Fatalf("stderr = %q", stderr.String())
	}
}

func TestREPLRememberCommandDefaultsToWorkspaceScopeAndNoteKind(t *testing.T) {
	userScope, workspaceScope := replTestMemoryScopes()
	backend := &replMemoryBackend{
		memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true },
		rememberMemory: func(_ context.Context, request memory.RememberRequest) (memory.Record, error) {
			if request.Scope != workspaceScope || request.Kind != "note" || request.Text != "prefers dark mode" || request.Key != "" {
				t.Fatalf("remember request = %#v", request)
			}
			return memory.Record{ID: "rec-9", Scope: workspaceScope, Kind: "note", Revision: 1}, nil
		},
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/remember prefers dark mode\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "remembered rec-9") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLRememberCommandParsesScopeKindKeyFlags(t *testing.T) {
	userScope, workspaceScope := replTestMemoryScopes()
	backend := &replMemoryBackend{
		memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true },
		rememberMemory: func(_ context.Context, request memory.RememberRequest) (memory.Record, error) {
			if request.Scope != userScope || request.Kind != "preference" || request.Key != "editor" || request.Text != "vim" {
				t.Fatalf("remember request = %#v", request)
			}
			return memory.Record{ID: "rec-2", Scope: userScope, Kind: "preference"}, nil
		},
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/remember --scope user --kind preference --key editor vim\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "remembered rec-2") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLRememberCommandRejectsEmptyTextContinues(t *testing.T) {
	userScope, workspaceScope := replTestMemoryScopes()
	backend := &replMemoryBackend{memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true }}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/remember --scope user\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stderr.String(), "usage:") {
		t.Fatalf("stderr = %q", stderr.String())
	}
}

func TestREPLRememberCommandWithoutManagerReturnsCommandError(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{}
	r := New(strings.NewReader("/remember prefers dark mode\n"), &stdout, &stderr, backend)
	err := r.Run(context.Background())
	if !errors.Is(err, app.ErrMemoryUnavailable) {
		t.Fatalf("Run() error = %v, want ErrMemoryUnavailable", err)
	}
	if !IsCommandError(err, "/remember") {
		t.Fatalf("Run() error = %v, want /remember command error", err)
	}
}
