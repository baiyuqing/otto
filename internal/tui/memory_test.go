package tui

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/model"
)

type memoryBackend struct {
	info                  app.Info
	history               []model.Message
	searchMemory          func(context.Context, memory.SearchRequest) (memory.SearchResult, error)
	rememberMemory        func(context.Context, memory.RememberRequest) (memory.Record, error)
	forgetMemory          func(context.Context, memory.ForgetRequest) (memory.ForgetResult, error)
	reviewMemoryCandidate func(context.Context, memory.ReviewRequest) (memory.ReviewResult, error)
	getMemory             func(context.Context, memory.RecordRef) (memory.Record, error)
	memoryScopes          func() (memory.Scope, memory.Scope, bool)
}

func (b *memoryBackend) Prompt(context.Context, string, func(agent.Event)) error { return nil }

func (b *memoryBackend) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{Noop: true}, nil
}

func (b *memoryBackend) NewSession() error { return nil }

func (b *memoryBackend) Info() app.Info { return b.info }

func (b *memoryBackend) History() []model.Message {
	return append([]model.Message(nil), b.history...)
}

func (b *memoryBackend) SearchMemory(ctx context.Context, request memory.SearchRequest) (memory.SearchResult, error) {
	if b.searchMemory == nil {
		return memory.SearchResult{}, app.ErrMemoryUnavailable
	}
	return b.searchMemory(ctx, request)
}

func (b *memoryBackend) RememberMemory(ctx context.Context, request memory.RememberRequest) (memory.Record, error) {
	if b.rememberMemory == nil {
		return memory.Record{}, app.ErrMemoryUnavailable
	}
	return b.rememberMemory(ctx, request)
}

func (b *memoryBackend) ForgetMemory(ctx context.Context, request memory.ForgetRequest) (memory.ForgetResult, error) {
	if b.forgetMemory == nil {
		return memory.ForgetResult{}, app.ErrMemoryUnavailable
	}
	return b.forgetMemory(ctx, request)
}

func (b *memoryBackend) ReviewMemoryCandidate(ctx context.Context, request memory.ReviewRequest) (memory.ReviewResult, error) {
	if b.reviewMemoryCandidate == nil {
		return memory.ReviewResult{}, app.ErrMemoryUnavailable
	}
	return b.reviewMemoryCandidate(ctx, request)
}

func (b *memoryBackend) GetMemory(ctx context.Context, ref memory.RecordRef) (memory.Record, error) {
	if b.getMemory == nil {
		return memory.Record{}, app.ErrMemoryUnavailable
	}
	return b.getMemory(ctx, ref)
}

func (b *memoryBackend) MemoryScopes() (memory.Scope, memory.Scope, bool) {
	if b.memoryScopes == nil {
		return memory.Scope{}, memory.Scope{}, false
	}
	return b.memoryScopes()
}

func newTestMemoryModel(t *testing.T, backend *memoryBackend) Model {
	t.Helper()
	return NewModel(context.Background(), backend, WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	})))
}

func testMemoryScopes() (memory.Scope, memory.Scope) {
	return memory.Scope{Namespace: "user", ID: "u1"}, memory.Scope{Namespace: "workspace", ID: "w1"}
}

func TestMemoryCommandRegistryCompletionAndHelp(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 16)
	m = typeEditorText(t, m, "/mem")
	if suggestions := m.commandSuggestions(); len(suggestions) != 1 || suggestions[0].Name != "/memory" {
		t.Fatalf("/mem suggestions = %#v", suggestions)
	}
	updated, _ := m.Update(keyPress(tea.KeyTab))
	if got := updated.(Model).editor.Value(); got != "/memory" {
		t.Fatalf("completed editor = %q, want /memory", got)
	}

	m = resizeModel(t, newTestModel(t), 80, 16)
	updated, _ = m.Update(showHelpOverlayMsg{})
	content := updated.(Model).View().Content
	if !strings.Contains(content, "/memory") || !strings.Contains(content, "/remember") {
		t.Fatalf("help overlay = %q", content)
	}
}

func TestMemoryCommandWithoutManagerReportsUnavailable(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.editor.SetValue("/memory search vim")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil", cmd)
	}
	if got.statusText != app.ErrMemoryUnavailable.Error() {
		t.Fatalf("status = %q", got.statusText)
	}
}

func TestMemoryCommandUsageWithoutSubcommand(t *testing.T) {
	userScope, workspaceScope := testMemoryScopes()
	backend := &memoryBackend{memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true }}
	m := resizeModel(t, newTestMemoryModel(t, backend), 80, 12)
	m.editor.SetValue("/memory")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil", cmd)
	}
	if !strings.Contains(got.statusText, "usage:") {
		t.Fatalf("status = %q", got.statusText)
	}
}

func TestMemoryAndRememberCommandsRejectedWhileTurnActive(t *testing.T) {
	userScope, workspaceScope := testMemoryScopes()
	backend := &memoryBackend{
		memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true },
		searchMemory: func(context.Context, memory.SearchRequest) (memory.SearchResult, error) {
			t.Fatal("searchMemory called while turn active")
			return memory.SearchResult{}, nil
		},
	}
	m := resizeModel(t, newTestMemoryModel(t, backend), 80, 12)
	m.running = true
	m.editor.SetValue("/memory search vim")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if cmd != nil || got.statusText != app.ErrPromptActive.Error() {
		t.Fatalf("cmd=%v status=%q", cmd, got.statusText)
	}
}

func TestMemorySearchCommandRendersRecords(t *testing.T) {
	userScope, workspaceScope := testMemoryScopes()
	backend := &memoryBackend{
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
	m := resizeModel(t, newTestMemoryModel(t, backend), 80, 20)
	m.editor.SetValue("/memory search vim")
	updated, cmd := m.dispatch(keyPress(tea.KeyEnter))
	pending := updated.(Model)
	if cmd == nil {
		t.Fatal("/memory search cmd = nil")
	}
	result := runCommandWithin(t, cmd, time.Second)
	updated, _ = pending.dispatch(result)
	got := updated.(Model)
	if got.statusText != "" {
		t.Fatalf("status = %q", got.statusText)
	}
	content := strings.Join(got.pendingPrints, "\n")
	if !strings.Contains(content, "id=rec-1") || !strings.Contains(content, "uses vim") {
		t.Fatalf("committed transcript = %q", content)
	}
}

func TestMemoryForgetCommandResolvesRevisionAcrossScopes(t *testing.T) {
	userScope, workspaceScope := testMemoryScopes()
	backend := &memoryBackend{
		memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true },
		getMemory: func(_ context.Context, ref memory.RecordRef) (memory.Record, error) {
			if ref.Scope == userScope {
				return memory.Record{}, memory.ErrNotFound
			}
			if ref.Scope != workspaceScope {
				t.Fatalf("unexpected scope %#v", ref.Scope)
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
	m := resizeModel(t, newTestMemoryModel(t, backend), 80, 20)
	m.editor.SetValue("/memory forget rec-1")
	updated, cmd := m.dispatch(keyPress(tea.KeyEnter))
	pending := updated.(Model)
	if cmd == nil {
		t.Fatal("/memory forget cmd = nil")
	}
	result := runCommandWithin(t, cmd, time.Second)
	updated, _ = pending.dispatch(result)
	got := updated.(Model)
	if content := strings.Join(got.pendingPrints, "\n"); !strings.Contains(content, "forgot rec-1") {
		t.Fatalf("committed transcript = %q", content)
	}
}

func TestMemoryForgetCommandNotFoundInAnyScope(t *testing.T) {
	userScope, workspaceScope := testMemoryScopes()
	backend := &memoryBackend{
		memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true },
		getMemory: func(_ context.Context, _ memory.RecordRef) (memory.Record, error) {
			return memory.Record{}, memory.ErrNotFound
		},
	}
	m := resizeModel(t, newTestMemoryModel(t, backend), 80, 20)
	m.editor.SetValue("/memory forget missing")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	pending := updated.(Model)
	if cmd == nil {
		t.Fatal("/memory forget cmd = nil")
	}
	result := runCommandWithin(t, cmd, time.Second)
	updated, _ = pending.Update(result)
	got := updated.(Model)
	if !strings.Contains(got.statusText, "not found") {
		t.Fatalf("status = %q", got.statusText)
	}
}

func TestMemoryReviewCommandTriesScopesAndAppliesDecision(t *testing.T) {
	userScope, workspaceScope := testMemoryScopes()
	backend := &memoryBackend{
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
	m := resizeModel(t, newTestMemoryModel(t, backend), 80, 20)
	m.editor.SetValue("/memory review cand-1 accept")
	updated, cmd := m.dispatch(keyPress(tea.KeyEnter))
	pending := updated.(Model)
	if cmd == nil {
		t.Fatal("/memory review cmd = nil")
	}
	result := runCommandWithin(t, cmd, time.Second)
	updated, _ = pending.dispatch(result)
	got := updated.(Model)
	if content := strings.Join(got.pendingPrints, "\n"); !strings.Contains(content, "reviewed cand-1") {
		t.Fatalf("committed transcript = %q", content)
	}
}

func TestMemoryReviewCommandRejectsInvalidDecision(t *testing.T) {
	userScope, workspaceScope := testMemoryScopes()
	backend := &memoryBackend{memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true }}
	m := resizeModel(t, newTestMemoryModel(t, backend), 80, 12)
	m.editor.SetValue("/memory review cand-1 maybe")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil", cmd)
	}
	if !strings.Contains(got.statusText, "usage:") {
		t.Fatalf("status = %q", got.statusText)
	}
}

func TestRememberCommandDefaultsToWorkspaceScopeAndNoteKind(t *testing.T) {
	userScope, workspaceScope := testMemoryScopes()
	backend := &memoryBackend{
		memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true },
		rememberMemory: func(_ context.Context, request memory.RememberRequest) (memory.Record, error) {
			if request.Scope != workspaceScope || request.Kind != "note" || request.Text != "prefers dark mode" || request.Key != "" {
				t.Fatalf("remember request = %#v", request)
			}
			return memory.Record{ID: "rec-9", Scope: workspaceScope, Kind: "note", Revision: 1}, nil
		},
	}
	m := resizeModel(t, newTestMemoryModel(t, backend), 80, 20)
	m.editor.SetValue("/remember prefers dark mode")
	updated, cmd := m.dispatch(keyPress(tea.KeyEnter))
	pending := updated.(Model)
	if cmd == nil {
		t.Fatal("/remember cmd = nil")
	}
	result := runCommandWithin(t, cmd, time.Second)
	updated, _ = pending.dispatch(result)
	got := updated.(Model)
	if content := strings.Join(got.pendingPrints, "\n"); !strings.Contains(content, "remembered rec-9") {
		t.Fatalf("committed transcript = %q", content)
	}
}

func TestRememberCommandParsesScopeKindKeyFlags(t *testing.T) {
	userScope, workspaceScope := testMemoryScopes()
	backend := &memoryBackend{
		memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true },
		rememberMemory: func(_ context.Context, request memory.RememberRequest) (memory.Record, error) {
			if request.Scope != userScope || request.Kind != "preference" || request.Key != "editor" || request.Text != "vim" {
				t.Fatalf("remember request = %#v", request)
			}
			return memory.Record{ID: "rec-2", Scope: userScope, Kind: "preference"}, nil
		},
	}
	m := resizeModel(t, newTestMemoryModel(t, backend), 80, 20)
	m.editor.SetValue("/remember --scope user --kind preference --key editor vim")
	updated, cmd := m.dispatch(keyPress(tea.KeyEnter))
	pending := updated.(Model)
	if cmd == nil {
		t.Fatal("/remember cmd = nil")
	}
	result := runCommandWithin(t, cmd, time.Second)
	updated, _ = pending.dispatch(result)
	got := updated.(Model)
	if content := strings.Join(got.pendingPrints, "\n"); !strings.Contains(content, "remembered rec-2") {
		t.Fatalf("committed transcript = %q", content)
	}
}

func TestRememberCommandRejectsEmptyText(t *testing.T) {
	userScope, workspaceScope := testMemoryScopes()
	backend := &memoryBackend{memoryScopes: func() (memory.Scope, memory.Scope, bool) { return userScope, workspaceScope, true }}
	m := resizeModel(t, newTestMemoryModel(t, backend), 80, 12)
	m.editor.SetValue("/remember --scope user")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil", cmd)
	}
	if !strings.Contains(got.statusText, "usage:") {
		t.Fatalf("status = %q", got.statusText)
	}
}

func TestMemoryWarningEventSetsStatusDuringPrompt(t *testing.T) {
	warningErr := errors.New("memory recall failed: query too long")
	backend := &fakeBackend{prompt: func(_ context.Context, _ string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventMemoryWarning, Err: warningErr})
		emit(agent.Event{Type: agent.EventTextDelta, Text: "done"})
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")
	// dispatch, not Update: submitting "question" commits a final User
	// entry within this same call, which Update's auto-flush wrapper would
	// batch with the real turn-start command. runCommandWithin below
	// invokes cmd() and expects the real turn message back; a batched cmd
	// would instead yield a tea.BatchMsg that dispatch below silently
	// ignores, dropping the memory-warning event.
	updated, cmd := m.dispatch(keyPress(tea.KeyEnter))
	state := updated.(Model)

	updated, _ = state.Update(runCommandWithin(t, cmd, time.Second))
	state = updated.(Model)
	if state.statusText != warningErr.Error() {
		t.Fatalf("warning status = %q", state.statusText)
	}
}

func TestRememberCommandWithoutManagerReportsUnavailable(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.editor.SetValue("/remember prefers dark mode")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil", cmd)
	}
	if got.statusText != app.ErrMemoryUnavailable.Error() {
		t.Fatalf("status = %q", got.statusText)
	}
}
