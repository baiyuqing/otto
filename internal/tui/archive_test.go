package tui

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

type archiveBackend struct {
	prompt         func(context.Context, string, func(agent.Event)) error
	newSession     func() error
	listSessions   func(context.Context, int) (session.ListResult, error)
	resumeSession  func(context.Context, string) (app.ResumeResult, error)
	archiveSession func(context.Context, string) (session.ArchiveResult, error)
	archiveCurrent func(context.Context) (session.ArchiveResult, error)
	info           app.Info
	history        []model.Message
}

func (b *archiveBackend) Prompt(ctx context.Context, text string, emit func(agent.Event)) error {
	if b.prompt == nil {
		return nil
	}
	return b.prompt(ctx, text, emit)
}

func (b *archiveBackend) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{Noop: true}, nil
}

func (b *archiveBackend) NewSession() error {
	if b.newSession == nil {
		return nil
	}
	return b.newSession()
}

func (b *archiveBackend) Info() app.Info { return b.info }

func (b *archiveBackend) History() []model.Message {
	return append([]model.Message(nil), b.history...)
}

func (b *archiveBackend) ListSessions(ctx context.Context, limit int) (session.ListResult, error) {
	if b.listSessions == nil {
		return session.ListResult{}, app.ErrPersistenceDisabled
	}
	return b.listSessions(ctx, limit)
}

func (b *archiveBackend) ResumeSession(ctx context.Context, path string) (app.ResumeResult, error) {
	if b.resumeSession == nil {
		return app.ResumeResult{}, app.ErrPersistenceDisabled
	}
	return b.resumeSession(ctx, path)
}

func (b *archiveBackend) ArchiveSession(ctx context.Context, path string) (session.ArchiveResult, error) {
	if b.archiveSession != nil {
		return b.archiveSession(ctx, path)
	}
	if b.archiveCurrent != nil && b.info.SessionPath != "" && path == b.info.SessionPath {
		return b.archiveCurrent(ctx)
	}
	return session.ArchiveResult{}, app.ErrPersistenceDisabled
}

func (b *archiveBackend) ArchiveCurrentSession(ctx context.Context) (session.ArchiveResult, error) {
	if b.archiveCurrent == nil {
		return session.ArchiveResult{}, app.ErrPersistenceDisabled
	}
	return b.archiveCurrent(ctx)
}

func newTestArchiveModel(t *testing.T, backend *archiveBackend) Model {
	t.Helper()
	return NewModel(context.Background(), backend, WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	})))
}

func loadArchivePicker(t *testing.T, backend *archiveBackend, result session.ListResult) Model {
	t.Helper()
	// dispatch (not Update) throughout, so pendingPrints keeps whatever
	// history text got committed on the initial resize instead of an
	// auto-flush popping it into an uninspectable tea.Cmd before the test
	// body can look at it.
	resized, _ := newTestArchiveModel(t, backend).dispatch(tea.WindowSizeMsg{Width: 80, Height: 12})
	m := resized.(Model)
	m.editor.SetValue("/archive")
	updated, cmd := m.dispatch(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	if cmd == nil {
		t.Fatalf("/archive cmd = nil")
	}
	updated, _ = loading.dispatch(sessionListResultMsg{generation: loading.archive.generation, result: result})
	loaded := updated.(Model)
	if loaded.archive.mode != archiveLoaded {
		t.Fatalf("archive.mode = %v, want %v", loaded.archive.mode, archiveLoaded)
	}
	return loaded
}

func loadedArchiveModel(t *testing.T, count int) Model {
	t.Helper()
	now := time.Date(2026, time.August, 27, 12, 0, 0, 0, time.UTC)
	sessions := make([]session.SessionInfo, count)
	for index := range sessions {
		sessions[index] = session.SessionInfo{
			ID:           fmt.Sprintf("session-%02d", index+1),
			Path:         fmt.Sprintf("/sessions/%02d.jsonl", index+1),
			Name:         fmt.Sprintf("Session %02d", index+1),
			Modified:     now.Add(-time.Duration(index) * time.Hour),
			MessageCount: index + 1,
			Profile:      "default",
			Provider:     "openai-compatible",
			Model:        "test-model",
		}
	}
	backend := &archiveBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "current"},
	}
	m := NewModel(context.Background(), backend, WithClock(newFakeClock(now)), WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	})))
	m.archive = archivePickerState{mode: archiveLoaded, sessions: sessions}
	return m
}

func updateArchiveKey(t *testing.T, model Model, code rune, modifiers ...tea.KeyMod) (Model, tea.Cmd) {
	t.Helper()
	updated, cmd := model.Update(keyPress(code, modifiers...))
	got, ok := updated.(Model)
	if !ok {
		t.Fatalf("updated model type = %T, want Model", updated)
	}
	return got, cmd
}

func TestArchiveCommandRegistryCompletionAndHelp(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 16)
	m = typeEditorText(t, m, "/arc")
	if suggestions := m.commandSuggestions(); len(suggestions) != 1 || suggestions[0].Name != "/archive" {
		t.Fatalf("/arc suggestions = %#v", suggestions)
	}
	updated, _ := m.Update(keyPress(tea.KeyTab))
	if got := updated.(Model).editor.Value(); got != "/archive" {
		t.Fatalf("completed editor = %q, want /archive", got)
	}

	m = resizeModel(t, newTestModel(t), 80, 16)
	updated, _ = m.Update(showHelpOverlayMsg{})
	content := updated.(Model).View().Content
	if !strings.Contains(content, "/archive") {
		t.Fatalf("help overlay = %q", content)
	}
}

func TestArchiveCommandLoadsActiveSessionsAsynchronously(t *testing.T) {
	backend := &archiveBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "current"},
		listSessions: func(_ context.Context, limit int) (session.ListResult, error) {
			if limit != resumeListLimit {
				t.Fatalf("list limit = %d, want %d", limit, resumeListLimit)
			}
			return session.ListResult{Sessions: []session.SessionInfo{{ID: "other", Path: "/sessions/other.jsonl"}}}, nil
		},
	}
	m := resizeModel(t, newTestArchiveModel(t, backend), 80, 12)
	m.editor.SetValue("/archive")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	if cmd == nil || loading.archive.mode != archiveLoading || !loading.archive.listPending {
		t.Fatalf("cmd=%v archive=%#v", cmd, loading.archive)
	}
	if !loading.reservedStateActive() {
		t.Fatal("reservedStateActive = false while archive picker is loading")
	}
	result := runCommandWithin(t, cmd, time.Second)
	updated, next := loading.Update(result)
	got := updated.(Model)
	if next != nil {
		t.Fatalf("list result scheduled unexpected cmd %v", next)
	}
	if got.archive.mode != archiveLoaded || got.archive.listPending {
		t.Fatalf("archive state = %#v", got.archive)
	}
	if len(got.archive.sessions) != 1 || got.archive.sessions[0].ID != "other" {
		t.Fatalf("archive sessions = %#v", got.archive.sessions)
	}
}

func TestArchiveCommandReportsPersistenceDisabledWithoutOpeningPicker(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.editor.SetValue("/archive")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if cmd != nil || got.archive.active() {
		t.Fatalf("cmd=%v archive=%#v", cmd, got.archive)
	}
	if got.statusText != app.ErrPersistenceDisabled.Error() {
		t.Fatalf("status = %q", got.statusText)
	}
}

func TestArchiveCommandIsRejectedWhileTurnOrNewSessionIsActive(t *testing.T) {
	backend := &archiveBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		listSessions: func(context.Context, int) (session.ListResult, error) {
			t.Fatal("listSessions called while turn active")
			return session.ListResult{}, nil
		},
	}
	for _, active := range []struct {
		name string
		run  func(*Model)
	}{
		{name: "running", run: func(m *Model) { m.running = true }},
		{name: "new-session-pending", run: func(m *Model) { m.newSessionPending = true }},
	} {
		t.Run(active.name, func(t *testing.T) {
			m := resizeModel(t, newTestArchiveModel(t, backend), 80, 12)
			active.run(&m)
			m.editor.SetValue("/archive")
			updated, cmd := m.Update(keyPress(tea.KeyEnter))
			got := updated.(Model)
			if cmd != nil || got.archive.active() {
				t.Fatalf("cmd=%v archive=%#v", cmd, got.archive)
			}
			if got.statusText != app.ErrPromptActive.Error() {
				t.Fatalf("status = %q", got.statusText)
			}
		})
	}
}

func TestArchivePickerNavigationPagesAndKeepsSelectionVisible(t *testing.T) {
	m := loadedArchiveModel(t, 40)
	got, _ := updateArchiveKey(t, m, tea.KeyDown)
	if got.archive.selected != 1 {
		t.Fatalf("down selected = %d, want 1", got.archive.selected)
	}
	got, _ = updateArchiveKey(t, got, tea.KeyPgUp)
	expected := max(0, 1-resumeVisibleRows(got.width, got.height))
	if got.archive.selected != expected {
		t.Fatalf("pgup selected = %d, want %d", got.archive.selected, expected)
	}
	got, _ = updateArchiveKey(t, m, tea.KeyUp)
	if got.archive.selected != 0 {
		t.Fatalf("up clamped selected = %d, want 0", got.archive.selected)
	}
	got, _ = updateArchiveKey(t, m, tea.KeyPgDown)
	if got.archive.selected != resumeVisibleRows(got.width, got.height) {
		t.Fatalf("pgdown selected = %d, want %d", got.archive.selected, resumeVisibleRows(got.width, got.height))
	}
}

func TestArchivePickerAtMinimumSizeShowsSelectionAndControlsWithinBounds(t *testing.T) {
	m := loadedArchiveModel(t, 20)
	m = resizeModel(t, m, 40, 8)
	content := m.View().Content
	assertRenderedBounds(t, content, 40, 8)
	if !strings.Contains(content, "Archive Session") || !strings.Contains(content, "1/20") {
		t.Fatalf("minimum picker view = %q", content)
	}
	if !strings.Contains(content, "Enter archive") || !strings.Contains(content, "Esc close") {
		t.Fatalf("minimum picker controls = %q", content)
	}
}

func TestArchivePickerExactEscapeBehaviorInEveryMode(t *testing.T) {
	backend := &archiveBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		listSessions: func(context.Context, int) (session.ListResult, error) {
			return session.ListResult{}, nil
		},
	}
	for _, tc := range []struct {
		name    string
		state   archivePickerState
		pending bool
	}{
		{name: "loading", state: archivePickerState{mode: archiveLoading}},
		{name: "loaded", state: archivePickerState{mode: archiveLoaded, sessions: []session.SessionInfo{{ID: "other", Path: "/sessions/other.jsonl"}}}},
		{name: "load-error", state: archivePickerState{mode: archiveLoadError}},
		{name: "archiving", state: archivePickerState{mode: archiveArchiving}},
		{name: "error", state: archivePickerState{mode: archiveError, sessions: []session.SessionInfo{{ID: "other", Path: "/sessions/other.jsonl"}}}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			m := resizeModel(t, newTestArchiveModel(t, backend), 80, 12)
			m.archive = tc.state
			got, cmd := updateArchiveKey(t, m, tea.KeyEsc)
			if tc.name == "archiving" {
				if got.archive.mode != archiveArchiving || got.statusText != "archive in progress" {
					t.Fatalf("archiving escape = %#v status %q", got.archive, got.statusText)
				}
				return
			}
			if cmd != nil || got.archive.active() {
				t.Fatalf("cmd=%v archive=%#v", cmd, got.archive)
			}
		})
	}
}

func TestArchiveResultLoadErrorKeepsPickerOpen(t *testing.T) {
	m := resizeModel(t, newTestArchiveModel(t, &archiveBackend{}), 80, 12)
	m.editor.SetValue("/archive")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	result := runCommandWithin(t, cmd, time.Second)
	_ = result
	updated, _ = loading.Update(sessionListResultMsg{
		generation: loading.archive.generation,
		errText:    "list failed",
	})
	got := updated.(Model)
	if got.archive.mode != archiveLoadError || got.archive.errText != "list failed" {
		t.Fatalf("archive state = %#v", got.archive)
	}
	if got.archive.listPending {
		t.Fatal("listPending remained true after load error")
	}
	if !strings.Contains(got.View().Content, "Unable to load sessions") {
		t.Fatalf("view = %q", got.View().Content)
	}
}

func TestArchiveResultEmptyListTracksSkippedSessions(t *testing.T) {
	m := resizeModel(t, newTestArchiveModel(t, &archiveBackend{}), 80, 12)
	m.editor.SetValue("/archive")
	updated, _ := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	updated, _ = loading.Update(sessionListResultMsg{generation: loading.archive.generation, result: session.ListResult{Skipped: 2}})
	got := updated.(Model)
	if got.archive.mode != archiveLoaded || got.archive.skipped != 2 {
		t.Fatalf("archive state = %#v", got.archive)
	}
	if got.statusText != "no active sessions found (skipped 2)" {
		t.Fatalf("status = %q", got.statusText)
	}
}

func TestArchiveResultIgnoresStaleListGeneration(t *testing.T) {
	backend := &archiveBackend{info: app.Info{Profile: "profile", Model: "model", SessionID: "session"}}
	m := resizeModel(t, newTestArchiveModel(t, backend), 80, 12)
	m.editor.SetValue("/archive")
	updated, _ := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	updated, _ = loading.Update(sessionListResultMsg{generation: loading.archive.generation + 1, result: session.ListResult{Sessions: []session.SessionInfo{{ID: "stale"}}}})
	got := updated.(Model)
	if got.archive.mode != archiveLoading || !got.archive.listPending || len(got.archive.sessions) != 0 {
		t.Fatalf("archive state = %#v", got.archive)
	}
}

func TestArchiveNonCurrentSessionSuccessClosesPickerWithStatus(t *testing.T) {
	backend := &archiveBackend{
		info:    app.Info{Profile: "profile", Model: "model", SessionID: "current", SessionPath: "/sessions/current.jsonl"},
		history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "current transcript"}}}},
	}
	backend.archiveSession = func(_ context.Context, path string) (session.ArchiveResult, error) {
		if path != "/sessions/other.jsonl" {
			t.Fatalf("path = %q, want other session path", path)
		}
		return session.ArchiveResult{Path: "/sessions/archive/other.jsonl", ID: "other"}, nil
	}
	m := loadArchivePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "other", Path: "/sessions/other.jsonl"}}})
	updated, cmd := m.dispatch(keyPress(tea.KeyEnter))
	archiving := updated.(Model)
	if cmd == nil || archiving.archive.mode != archiveArchiving {
		t.Fatalf("cmd=%v archive=%#v", cmd, archiving.archive)
	}
	result := runCommandWithin(t, cmd, time.Second)
	updated, next := archiving.dispatch(result)
	got := updated.(Model)
	if next != nil {
		t.Fatalf("archive success scheduled unexpected cmd %v", next)
	}
	if got.archive.mode != archiveClosed || got.editor.Value() != "" {
		t.Fatalf("archive state = %#v editor = %q", got.archive, got.editor.Value())
	}
	if got.statusText != "archived session other" {
		t.Fatalf("status = %q", got.statusText)
	}
	// "other" is a non-current session, so archiving it must not touch this
	// session's own transcript: it stays committed to scrollback exactly as
	// it was before /archive ran.
	if printed := strings.Join(got.pendingPrints, "\n"); !strings.Contains(printed, "current transcript") {
		t.Fatalf("current transcript was replaced: %q", printed)
	}
}

func TestArchiveCurrentSessionSuccessRebuildsFreshView(t *testing.T) {
	backend := &archiveBackend{
		info:    app.Info{Profile: "old-profile", Model: "old-model", SessionID: "session-old", SessionPath: "/sessions/session-old.jsonl"},
		history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}}}},
	}
	backend.archiveCurrent = func(ctx context.Context) (session.ArchiveResult, error) {
		backend.info = app.Info{Profile: "new-profile", Model: "new-model", SessionID: "session-new", SessionPath: "/sessions/session-new.jsonl"}
		backend.history = nil
		return session.ArchiveResult{Path: "/sessions/archive/session-old.jsonl", ID: "session-old"}, nil
	}
	m := loadArchivePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "session-old", Path: "/sessions/session-old.jsonl", Current: true}}})
	m = resizeModel(t, m, 120, 12)
	m.editor.SetValue("draft")
	m.statusText = "stale"
	m.running = true

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	archiving := updated.(Model)
	if cmd == nil || archiving.archive.mode != archiveArchiving {
		t.Fatalf("cmd=%v archive=%#v", cmd, archiving.archive)
	}
	result := runCommandWithin(t, cmd, time.Second)
	// dispatch, not Update: archive success on the current session rebuilds
	// an empty view and queues the empty-session banner for scrollback, which
	// Update's auto-flush wrapper would turn into a real (non-nil) print cmd
	// unrelated to what this test checks (state reset), so inspect the raw
	// dispatch result instead of Update's flush-wrapped one.
	updated, next := archiving.dispatch(result)
	got := updated.(Model)
	if next != nil {
		t.Fatalf("archive success scheduled unexpected cmd %v", next)
	}
	if got.archive.mode != archiveClosed || got.editor.Value() != "" || got.running {
		t.Fatalf("reset state = %#v editor = %q running = %v", got.archive, got.editor.Value(), got.running)
	}
	if got.statusText != "archived session session-old; started new session" {
		t.Fatalf("status = %q", got.statusText)
	}
	if content := got.View().Content; strings.Contains(content, "old transcript") || !strings.Contains(content, "new-profile/new-model") {
		t.Fatalf("view = %q", content)
	}
}

func TestArchiveFailureKeepsPickerAndOldUI(t *testing.T) {
	backend := &archiveBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "current", SessionPath: "/sessions/current.jsonl"},
	}
	backend.archiveSession = func(context.Context, string) (session.ArchiveResult, error) {
		return session.ArchiveResult{}, errors.New("archive failed")
	}
	// dispatch (not Update) throughout: loadArchivePicker leaves the
	// empty-session banner queued in pendingPrints (no seeded history here),
	// so an Update call at this point would batch-wrap its own command with
	// the pending flush command, corrupting the message this test manually
	// feeds back into the next step.
	m := loadArchivePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "other", Path: "/sessions/other.jsonl"}}})
	updated, cmd := m.dispatch(keyPress(tea.KeyEnter))
	archiving := updated.(Model)
	result := runCommandWithin(t, cmd, time.Second)
	updated, _ = archiving.dispatch(result)
	got := updated.(Model)
	if got.archive.mode != archiveError || got.archive.errText != "archive failed" {
		t.Fatalf("archive state = %#v", got.archive)
	}
	if got.statusText != "archive failed" {
		t.Fatalf("status = %q", got.statusText)
	}
	if !strings.Contains(got.View().Content, "Error: archive failed") {
		t.Fatalf("view = %q", got.View().Content)
	}
	// Retry after fixing the backend.
	backend.archiveSession = func(context.Context, string) (session.ArchiveResult, error) {
		return session.ArchiveResult{Path: "/sessions/archive/other.jsonl", ID: "other"}, nil
	}
	updated, cmd = got.dispatch(keyPress(tea.KeyEnter))
	archiving = updated.(Model)
	if cmd == nil || archiving.archive.mode != archiveArchiving {
		t.Fatalf("retry cmd=%v archive=%#v", cmd, archiving.archive)
	}
	result = runCommandWithin(t, cmd, time.Second)
	updated, _ = archiving.dispatch(result)
	got = updated.(Model)
	if got.archive.mode != archiveClosed || got.statusText != "archived session other" {
		t.Fatalf("retry state = %#v status = %q", got.archive, got.statusText)
	}
}

func TestArchiveCommandDuplicateEnterBlockedWhileArchiving(t *testing.T) {
	backend := &archiveBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "current", SessionPath: "/sessions/current.jsonl"},
	}
	backend.archiveSession = func(context.Context, string) (session.ArchiveResult, error) {
		t.Fatal("second archive attempted while first is in flight")
		return session.ArchiveResult{}, nil
	}
	m := loadArchivePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "other", Path: "/sessions/other.jsonl"}}})
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	archiving := updated.(Model)
	if cmd == nil || archiving.archive.mode != archiveArchiving {
		t.Fatalf("cmd=%v archive=%#v", cmd, archiving.archive)
	}
	got, secondCmd := updateArchiveKey(t, archiving, tea.KeyEnter)
	if secondCmd != nil {
		t.Fatalf("second enter scheduled cmd %v", secondCmd)
	}
	if got.archive.mode != archiveArchiving {
		t.Fatalf("second enter changed mode to %v", got.archive.mode)
	}
}

func TestArchivePickerAndResumePickerAreMutuallyExclusive(t *testing.T) {
	backend := &archiveBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
	}
	m := loadArchivePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "other", Path: "/sessions/other.jsonl"}}})
	updated, cmd := m.handleResumeCommand()
	got := updated.(Model)
	if cmd != nil || got.resume.active() {
		t.Fatalf("cmd=%v resume=%#v", cmd, got.resume)
	}
	if got.statusText != app.ErrPromptActive.Error() {
		t.Fatalf("status = %q", got.statusText)
	}
	if !got.archive.active() {
		t.Fatal("archive picker closed while rejecting /resume")
	}

	// Reverse direction: /archive rejected while the resume picker is open.
	m = resizeModel(t, newTestArchiveModel(t, backend), 80, 12)
	m.resume = resumePickerState{mode: resumeLoaded, sessions: []session.SessionInfo{{ID: "other", Path: "/sessions/other.jsonl"}}}
	updated, cmd = m.handleArchiveCommand()
	got = updated.(Model)
	if cmd != nil || got.archive.active() {
		t.Fatalf("cmd=%v archive=%#v", cmd, got.archive)
	}
	if got.statusText != app.ErrPromptActive.Error() {
		t.Fatalf("status = %q", got.statusText)
	}
	if !got.resume.active() {
		t.Fatal("resume picker closed while rejecting /archive")
	}
}

func TestArchivePickerEscClosesPickerAndCancelsLoading(t *testing.T) {
	backend := &archiveBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		listSessions: func(ctx context.Context, _ int) (session.ListResult, error) {
			<-ctx.Done()
			return session.ListResult{}, ctx.Err()
		},
	}
	m := resizeModel(t, newTestArchiveModel(t, backend), 80, 12)
	m.editor.SetValue("/archive")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	if cmd == nil || !loading.archive.listPending {
		t.Fatalf("cmd=%v archive=%#v", cmd, loading.archive)
	}
	got, _ := updateArchiveKey(t, loading, tea.KeyEsc)
	if got.archive.mode != archiveClosed || !got.archive.listPending {
		t.Fatalf("esc state = %#v", got.archive)
	}
}
