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
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

type resumeBackend struct {
	prompt        func(context.Context, string, func(agent.Event)) error
	newSession    func() error
	listSessions  func(context.Context, int) (session.ListResult, error)
	resumeSession func(context.Context, string) (app.ResumeResult, error)
	info          app.Info
	history       []model.Message
}

func (b *resumeBackend) Prompt(ctx context.Context, text string, emit func(agent.Event)) error {
	if b.prompt == nil {
		return nil
	}
	return b.prompt(ctx, text, emit)
}

func (b *resumeBackend) NewSession() error {
	if b.newSession == nil {
		return nil
	}
	return b.newSession()
}

func (b *resumeBackend) Info() app.Info { return b.info }

func (b *resumeBackend) History() []model.Message {
	return append([]model.Message(nil), b.history...)
}

func (b *resumeBackend) ListSessions(ctx context.Context, limit int) (session.ListResult, error) {
	if b.listSessions == nil {
		return session.ListResult{}, app.ErrPersistenceDisabled
	}
	return b.listSessions(ctx, limit)
}

func (b *resumeBackend) ResumeSession(ctx context.Context, path string) (app.ResumeResult, error) {
	if b.resumeSession == nil {
		return app.ResumeResult{}, app.ErrPersistenceDisabled
	}
	return b.resumeSession(ctx, path)
}

func newTestResumeModel(t *testing.T, backend *resumeBackend) Model {
	t.Helper()
	return NewModel(context.Background(), backend, WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	})))
}

func loadResumePicker(t *testing.T, backend *resumeBackend, result session.ListResult) Model {
	t.Helper()
	m := resizeModel(t, newTestResumeModel(t, backend), 80, 12)
	m.editor.SetValue("/resume")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	if cmd == nil {
		t.Fatalf("/resume cmd = nil")
	}
	updated, _ = loading.Update(sessionListResultMsg{generation: loading.resume.generation, result: result})
	loaded := updated.(Model)
	if loaded.resume.mode != resumeLoaded {
		t.Fatalf("resume.mode = %v, want %v", loaded.resume.mode, resumeLoaded)
	}
	return loaded
}

func TestResumeCommandLoadsRecentSessionsAsynchronously(t *testing.T) {
	backend := &resumeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		listSessions: func(ctx context.Context, limit int) (session.ListResult, error) {
			if err := ctx.Err(); err != nil {
				t.Fatalf("ctx.Err() = %v", err)
			}
			if limit != 20 {
				t.Fatalf("limit = %d, want 20", limit)
			}
			return session.ListResult{Sessions: []session.SessionInfo{{ID: "old", Path: "/sessions/old.jsonl"}}}, nil
		},
	}
	m := resizeModel(t, newTestResumeModel(t, backend), 80, 16)
	m.editor.SetValue("/resume")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	if cmd == nil || loading.resume.mode != resumeLoading || loading.editor.Value() != "" {
		t.Fatalf("state = %#v cmd=%v editor=%q", loading.resume, cmd, loading.editor.Value())
	}

	result := runCommandWithin(t, cmd, time.Second)
	updated, _ = loading.Update(result)
	loaded := updated.(Model)
	if loaded.resume.mode != resumeLoaded || len(loaded.resume.sessions) != 1 || loaded.resume.sessions[0].ID != "old" {
		t.Fatalf("state = %#v", loaded.resume)
	}
}

func TestResumeCommandReportsPersistenceDisabledWithoutOpeningPicker(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.editor.SetValue("/resume")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if cmd != nil || got.resume.mode != resumeClosed || got.editor.Value() != "/resume" || got.statusText != app.ErrPersistenceDisabled.Error() {
		t.Fatalf("cmd=%v resume=%#v editor=%q status=%q", cmd, got.resume, got.editor.Value(), got.statusText)
	}
}

func TestResumeCommandIsRejectedWhileTurnOrNewSessionIsActive(t *testing.T) {
	t.Run("turn active", func(t *testing.T) {
		started := make(chan struct{})
		release := make(chan struct{})
		listCalls := 0
		backend := &resumeBackend{
			info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
			prompt: func(context.Context, string, func(agent.Event)) error {
				close(started)
				<-release
				return nil
			},
			listSessions: func(context.Context, int) (session.ListResult, error) {
				listCalls++
				return session.ListResult{}, nil
			},
		}
		m := resizeModel(t, newTestResumeModel(t, backend), 80, 12)
		m.editor.SetValue("question")
		updated, cmd := m.Update(keyPress(tea.KeyEnter))
		running := updated.(Model)
		turnDone := make(chan tea.Msg, 1)
		go func() { turnDone <- cmd() }()
		<-started

		running.editor.SetValue("/resume")
		updated, rejectCmd := running.Update(keyPress(tea.KeyEnter))
		got := updated.(Model)
		if rejectCmd != nil || got.resume.mode != resumeClosed || got.statusText != app.ErrPromptActive.Error() || listCalls != 0 {
			t.Fatalf("cmd=%v resume=%#v status=%q listCalls=%d", rejectCmd, got.resume, got.statusText, listCalls)
		}

		close(release)
		select {
		case <-turnDone:
		case <-time.After(time.Second):
			t.Fatal("turn command did not complete")
		}
	})

	t.Run("new pending", func(t *testing.T) {
		started := make(chan struct{})
		release := make(chan struct{})
		listCalls := 0
		backend := &resumeBackend{
			info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
			newSession: func() error {
				close(started)
				<-release
				return nil
			},
			listSessions: func(context.Context, int) (session.ListResult, error) {
				listCalls++
				return session.ListResult{}, nil
			},
		}
		m := resizeModel(t, newTestResumeModel(t, backend), 80, 12)
		m.editor.SetValue("/new")
		updated, cmd := m.Update(keyPress(tea.KeyEnter))
		pending := updated.(Model)
		if cmd == nil || !pending.newSessionPending {
			t.Fatalf("/new cmd=%v pending=%v", cmd, pending.newSessionPending)
		}
		newDone := make(chan tea.Msg, 1)
		go func() { newDone <- cmd() }()
		<-started

		pending.editor.SetValue("/resume")
		updated, rejectCmd := pending.Update(keyPress(tea.KeyEnter))
		got := updated.(Model)
		if rejectCmd != nil || got.resume.mode != resumeClosed || got.statusText != app.ErrPromptActive.Error() || listCalls != 0 {
			t.Fatalf("cmd=%v resume=%#v status=%q listCalls=%d", rejectCmd, got.resume, got.statusText, listCalls)
		}

		close(release)
		select {
		case <-newDone:
		case <-time.After(time.Second):
			t.Fatal("new session command did not complete")
		}
	})
}

func TestResumeResultLoadErrorKeepsPickerOpen(t *testing.T) {
	backend := &resumeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		listSessions: func(context.Context, int) (session.ListResult, error) {
			return session.ListResult{}, errors.New("list failed")
		},
	}
	m := resizeModel(t, newTestResumeModel(t, backend), 80, 12)
	m.editor.SetValue("/resume")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	if cmd == nil {
		t.Fatal("/resume cmd = nil")
	}

	updated, _ = loading.Update(cmd())
	got := updated.(Model)
	if got.resume.mode != resumeLoadError || got.resume.errText != "list failed" || got.statusText != "list failed" {
		t.Fatalf("resume=%#v status=%q", got.resume, got.statusText)
	}
}

func TestResumeResultEmptyListTracksSkippedSessions(t *testing.T) {
	backend := &resumeBackend{info: app.Info{Profile: "profile", Model: "model", SessionID: "session"}}
	m := resizeModel(t, newTestResumeModel(t, backend), 80, 12)
	m.editor.SetValue("/resume")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	if cmd == nil {
		t.Fatal("/resume cmd = nil")
	}

	updated, _ = loading.Update(sessionListResultMsg{generation: loading.resume.generation, result: session.ListResult{Skipped: 2}})
	got := updated.(Model)
	if got.resume.mode != resumeLoaded || len(got.resume.sessions) != 0 || got.resume.skipped != 2 {
		t.Fatalf("resume=%#v", got.resume)
	}
	if !strings.Contains(got.statusText, "no resumable sessions") || !strings.Contains(got.statusText, "skipped 2") {
		t.Fatalf("status=%q", got.statusText)
	}
}

func TestResumeResultIgnoresStaleListGeneration(t *testing.T) {
	backend := &resumeBackend{info: app.Info{Profile: "profile", Model: "model", SessionID: "session"}}
	m := resizeModel(t, newTestResumeModel(t, backend), 80, 12)
	m.editor.SetValue("/resume")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	if cmd == nil {
		t.Fatal("/resume cmd = nil")
	}

	updated, _ = loading.Update(sessionListResultMsg{generation: loading.resume.generation + 1, result: session.ListResult{Sessions: []session.SessionInfo{{ID: "stale"}}}})
	got := updated.(Model)
	if got.resume.mode != resumeLoading || len(got.resume.sessions) != 0 || got.editor.Value() != "" {
		t.Fatalf("resume=%#v editor=%q", got.resume, got.editor.Value())
	}
}

func TestResumePickerCurrentSelectionClosesWithoutResuming(t *testing.T) {
	resumeCalls := 0
	backend := &resumeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		resumeSession: func(context.Context, string) (app.ResumeResult, error) {
			resumeCalls++
			return app.ResumeResult{}, nil
		},
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "session", Path: "/sessions/current.jsonl", Current: true}}})

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if cmd != nil || got.resume.mode != resumeClosed || resumeCalls != 0 {
		t.Fatalf("cmd=%v resume=%#v resumeCalls=%d", cmd, got.resume, resumeCalls)
	}
}

func TestResumeSuccessReplacesHistoryAndClearsStaleState(t *testing.T) {
	backend := &resumeBackend{
		info: app.Info{Profile: "old-profile", Model: "old-model", SessionID: "session-old"},
		history: []model.Message{{
			Role:   model.RoleAssistant,
			Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}},
			Usage:  &model.Usage{InputTokens: 1, OutputTokens: 2},
		}},
	}
	backend.resumeSession = func(_ context.Context, path string) (app.ResumeResult, error) {
		if path != "/sessions/fresh.jsonl" {
			t.Fatalf("path = %q, want fresh session path", path)
		}
		backend.info = app.Info{Profile: "new-profile", Model: "new-model", SessionID: "session-new"}
		backend.history = []model.Message{{
			Role:   model.RoleAssistant,
			Blocks: []model.Block{{Type: model.BlockText, Text: "fresh transcript"}},
			Usage:  &model.Usage{InputTokens: 7, OutputTokens: 9},
		}}
		return app.ResumeResult{Warnings: []session.Warning{{Message: "warning: repaired trailing newline"}}}, nil
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "fresh", Path: "/sessions/fresh.jsonl"}}})
	m.editor.SetValue("draft")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	if cmd == nil || resuming.resume.mode != resumeResuming || !strings.Contains(resuming.View().Content, "old transcript") {
		t.Fatalf("cmd=%v resume=%#v view=%q", cmd, resuming.resume, resuming.View().Content)
	}
	result := runCommandWithin(t, cmd, time.Second)
	if content := resuming.View().Content; !strings.Contains(content, "old transcript") || strings.Contains(content, "fresh transcript") {
		t.Fatalf("resuming view = %q", content)
	}
	resuming.statusText = "stale"
	resuming.overlay = overlaySession
	resuming.running = true
	resuming.dirtyStreaming = true
	resuming.renderTickActive = true
	resuming.cancel = func() {}
	resuming.ctrlCArmed = true
	resuming.ctrlCArmedAt = time.Unix(5, 0).UTC()
	resuming.activeTurnChannel = make(chan turnEnvelope)
	resuming.activeAssistant = 99
	resuming.turnErrorSeen = true
	resuming.turnEventErr = errors.New("old event error")
	resuming.fatalErr = errors.New("old fatal")
	resuming.turnHistoryBaseline = turnHistoryBaseline{messageCount: 2, valid: true}
	resuming.turnEntryStart = 3
	resuming.liveEntrySequence = 11
	resuming.autoFollow = false

	updated, next := resuming.Update(result)
	got := updated.(Model)
	if next != nil {
		t.Fatalf("resume success scheduled unexpected cmd %v", next)
	}
	if got.resume.mode != resumeClosed || got.editor.Value() != "" || got.overlay != overlayNone || got.running || got.dirtyStreaming || got.renderTickActive || got.cancel != nil || got.ctrlCArmed || got.activeTurnChannel != nil || got.activeAssistant != -1 || got.turnErrorSeen || got.turnEventErr != nil || got.fatalErr != nil || got.turnHistoryBaseline != (turnHistoryBaseline{}) || got.turnEntryStart != 0 || got.liveEntrySequence != 0 || !got.autoFollow {
		t.Fatalf("reset state = %#v", got)
	}
	if got.usage.InputTokens != 7 || got.usage.OutputTokens != 9 {
		t.Fatalf("usage = %#v", got.usage)
	}
	if content := got.View().Content; strings.Contains(content, "old transcript") || !strings.Contains(content, "fresh transcript") || !strings.Contains(content, "new-profile/new-model") {
		t.Fatalf("view = %q", content)
	}
	if !strings.Contains(got.statusText, "warning: repaired trailing newline") {
		t.Fatalf("status = %q", got.statusText)
	}
}

func TestResumeFailureKeepsPickerAndOldUI(t *testing.T) {
	backend := &resumeBackend{
		info: app.Info{Profile: "old-profile", Model: "old-model", SessionID: "session-old"},
		history: []model.Message{{
			Role:   model.RoleAssistant,
			Blocks: []model.Block{{Type: model.BlockText, Text: "keep transcript"}},
			Usage:  &model.Usage{InputTokens: 3, OutputTokens: 4},
		}},
		resumeSession: func(context.Context, string) (app.ResumeResult, error) {
			return app.ResumeResult{}, errors.New("resume failed")
		},
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "fresh", Path: "/sessions/fresh.jsonl"}}})

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	if cmd == nil || resuming.resume.mode != resumeResuming {
		t.Fatalf("cmd=%v resume=%#v", cmd, resuming.resume)
	}

	updated, next := resuming.Update(runCommandWithin(t, cmd, time.Second))
	got := updated.(Model)
	if next != nil {
		t.Fatalf("resume failure scheduled unexpected cmd %v", next)
	}
	if got.resume.mode != resumeResumeError || got.resume.errText != "resume failed" || got.resume.selected != 0 || len(got.resume.sessions) != 1 {
		t.Fatalf("resume=%#v", got.resume)
	}
	if got.usage.InputTokens != 3 || got.usage.OutputTokens != 4 {
		t.Fatalf("usage = %#v", got.usage)
	}
	if content := got.View().Content; !strings.Contains(content, "keep transcript") || !strings.Contains(content, "old-profile/old-model") || !strings.Contains(content, "session-old") {
		t.Fatalf("view = %q", content)
	}
	if got.statusText != "resume failed" {
		t.Fatalf("status = %q", got.statusText)
	}
}

func TestResumeCommandDuplicateEnterBlockedWhileResuming(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	calls := 0
	backend := &resumeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		resumeSession: func(context.Context, string) (app.ResumeResult, error) {
			calls++
			close(started)
			<-release
			return app.ResumeResult{}, nil
		},
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "fresh", Path: "/sessions/fresh.jsonl"}}})
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	if cmd == nil || resuming.resume.mode != resumeResuming {
		t.Fatalf("cmd=%v resume=%#v", cmd, resuming.resume)
	}

	resultCh := make(chan tea.Msg, 1)
	go func() { resultCh <- cmd() }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("resume command did not start")
	}

	updated, duplicateCmd := resuming.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if duplicateCmd != nil || got.resume.mode != resumeResuming || calls != 1 {
		t.Fatalf("cmd=%v resume=%#v calls=%d", duplicateCmd, got.resume, calls)
	}

	close(release)
	select {
	case <-resultCh:
	case <-time.After(time.Second):
		t.Fatal("resume command did not complete")
	}
}

func TestResumeResultIgnoresStaleResumeGeneration(t *testing.T) {
	backend := &resumeBackend{
		info:    app.Info{Profile: "old-profile", Model: "old-model", SessionID: "session-old"},
		history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}}}},
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "fresh", Path: "/sessions/fresh.jsonl"}}})
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	if cmd == nil {
		t.Fatal("resume cmd = nil")
	}

	backend.info = app.Info{Profile: "new-profile", Model: "new-model", SessionID: "session-new"}
	backend.history = []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "fresh transcript"}}}}
	updated, _ = resuming.Update(sessionResumeResultMsg{generation: resuming.resume.generation + 1, path: "/sessions/fresh.jsonl", result: app.ResumeResult{Warnings: []session.Warning{{Message: "stale warning"}}}})
	got := updated.(Model)
	if got.resume.mode != resumeResuming || got.statusText != "" {
		t.Fatalf("resume=%#v status=%q", got.resume, got.statusText)
	}
	if content := got.View().Content; !strings.Contains(content, "old transcript") || strings.Contains(content, "fresh transcript") {
		t.Fatalf("view = %q", content)
	}
}
