package tui

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"sync/atomic"
	"testing"
	"time"
	"unicode"

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

func TestResumeResultReconcilesCommittedSuccessWithStaleGeneration(t *testing.T) {
	backend := &resumeBackend{
		info:    app.Info{Profile: "old-profile", Model: "old-model", SessionID: "session-old", SessionPath: "/sessions/old.jsonl"},
		history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}}}},
	}
	backend.resumeSession = func(_ context.Context, path string) (app.ResumeResult, error) {
		backend.info = app.Info{Profile: "new-profile", Model: "new-model", SessionID: "session-new", SessionPath: path}
		backend.history = []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "fresh transcript"}}}}
		return app.ResumeResult{Warnings: []session.Warning{{Message: "stale payload warning"}}}, nil
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "fresh", Path: "/sessions/fresh.jsonl"}}})
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	if cmd == nil {
		t.Fatal("resume cmd = nil")
	}

	result := runCommandWithin(t, cmd, time.Second)
	resuming.resume.generation++
	updated, next := resuming.Update(result)
	got := updated.(Model)
	if next != nil || got.resume.mode != resumeClosed {
		t.Fatalf("cmd=%v resume=%#v", next, got.resume)
	}
	if content := got.View().Content; strings.Contains(content, "old transcript") || !strings.Contains(content, "fresh transcript") {
		t.Fatalf("view = %q", content)
	}
	if got.statusText != "resumed session" || strings.Contains(got.statusText, "stale payload warning") {
		t.Fatalf("status = %q, want generic reconciled status", got.statusText)
	}
}

func TestResumeResultStaleSuccessSupersededByNewerBackendDoesNotRollbackUI(t *testing.T) {
	backend := &resumeBackend{
		info:    app.Info{Profile: "old-profile", Model: "old-model", SessionID: "session-old", SessionPath: "/sessions/old.jsonl"},
		history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}}}},
	}
	backend.resumeSession = func(_ context.Context, path string) (app.ResumeResult, error) {
		backend.info = app.Info{Profile: "middle-profile", Model: "middle-model", SessionID: "session-middle", SessionPath: path}
		backend.history = []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "middle transcript"}}}}
		return app.ResumeResult{}, nil
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "middle", Path: "/sessions/middle.jsonl"}}})
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	staleResult := runCommandWithin(t, cmd, time.Second)

	backend.info = app.Info{Profile: "newest-profile", Model: "newest-model", SessionID: "session-newest", SessionPath: "/sessions/newest.jsonl"}
	backend.history = []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "newest transcript"}}}}
	resuming.resetSessionViewFromBackend("newest status")
	resuming.closeResumePicker()
	resuming.resume.generation++

	updated, next := resuming.Update(staleResult)
	got := updated.(Model)
	if next != nil || got.statusText != "newest status" {
		t.Fatalf("cmd=%v status=%q", next, got.statusText)
	}
	if content := got.View().Content; strings.Contains(content, "middle transcript") || !strings.Contains(content, "newest transcript") {
		t.Fatalf("view = %q", content)
	}
}

func TestResumeResultCommittedStaleSuccessFailsClosedDuringNewerResume(t *testing.T) {
	backend := &resumeBackend{
		info:    app.Info{Profile: "old-profile", Model: "old-model", SessionID: "session-old", SessionPath: "/sessions/old.jsonl"},
		history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}}}},
	}
	backend.resumeSession = func(_ context.Context, path string) (app.ResumeResult, error) {
		backend.info.SessionPath = path
		backend.info.SessionID = "session-middle"
		backend.history = []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "middle transcript"}}}}
		return app.ResumeResult{}, nil
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "middle", Path: "/sessions/middle.jsonl"}}})
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	staleResult := runCommandWithin(t, cmd, time.Second)

	resuming.resume.generation++
	resuming.resume.mode = resumeResuming
	resuming.resume.operationPath = "/sessions/newest.jsonl"
	updated, quit := resuming.Update(staleResult)
	got := updated.(Model)
	if quit == nil {
		t.Fatal("quit command = nil")
	}
	if _, ok := quit().(tea.QuitMsg); !ok {
		t.Fatalf("quit command message = %T, want tea.QuitMsg", quit())
	}
	if got.fatalErr == nil {
		t.Fatal("fatalErr = nil, want fail-closed reconciliation error")
	}
	if content := got.View().Content; strings.Contains(content, "middle transcript") || !strings.Contains(content, "old transcript") {
		t.Fatalf("view before quit = %q", content)
	}
}

func TestResumeWorkersBoundUntrustedSessionResultsAndErrors(t *testing.T) {
	const backendSkipped = 7
	sessions := make([]session.SessionInfo, resumeListLimit*1000)
	for index := range sessions {
		sessions[index] = session.SessionInfo{
			Path:         fmt.Sprintf("/sessions/%05d.jsonl", index),
			ID:           "id\n\x1b" + strings.Repeat("界", resumeMaxFieldBytes),
			CWD:          strings.Repeat("c", resumeMaxFieldBytes+100),
			Name:         strings.Repeat("n", resumeMaxFieldBytes+100),
			LastUserText: strings.Repeat("u", resumeMaxFieldBytes+100),
			Profile:      strings.Repeat("p", resumeMaxFieldBytes+100),
			Provider:     strings.Repeat("v", resumeMaxFieldBytes+100),
			Model:        strings.Repeat("m", resumeMaxFieldBytes+100),
		}
	}
	sessions[1].Path = strings.Repeat("/", resumeMaxPathBytes+1)
	backend := &resumeBackend{
		listSessions: func(context.Context, int) (session.ListResult, error) {
			return session.ListResult{Sessions: sessions, Skipped: backendSkipped}, nil
		},
	}

	message := runCommandWithin(t, runSessionListCommand(context.Background(), backend, 41), time.Second)
	result, ok := message.(sessionListResultMsg)
	if !ok {
		t.Fatalf("message = %T, want sessionListResultMsg", message)
	}
	wantSessions := resumeListLimit - 1 // The overlong path is dropped, not backfilled from unbounded input.
	if result.errText != "" || len(result.result.Sessions) != wantSessions {
		t.Fatalf("err=%q sessions=%d, want %d", result.errText, len(result.result.Sessions), wantSessions)
	}
	if want := backendSkipped + len(sessions) - wantSessions; result.result.Skipped != want {
		t.Fatalf("skipped = %d, want %d", result.result.Skipped, want)
	}
	for index, info := range result.result.Sessions {
		fields := map[string]string{
			"ID": info.ID, "CWD": info.CWD, "Name": info.Name, "LastUserText": info.LastUserText,
			"Profile": info.Profile, "Provider": info.Provider, "Model": info.Model,
		}
		if len(info.Path) > resumeMaxPathBytes {
			t.Fatalf("session %d path bytes = %d", index, len(info.Path))
		}
		assertResumeSingleLineControlSafe(t, fmt.Sprintf("session %d path", index), info.Path)
		for name, value := range fields {
			if len(value) > resumeMaxFieldBytes {
				t.Fatalf("session %d %s bytes = %d", index, name, len(value))
			}
			assertResumeSingleLineControlSafe(t, fmt.Sprintf("session %d %s", index, name), value)
		}
	}

	presentationBackend := &resumeBackend{info: app.Info{Profile: "profile", Model: "model", SessionID: "session"}}
	m := resizeModel(t, newTestResumeModel(t, presentationBackend), 80, 12)
	m.editor.SetValue("/resume")
	updated, _ := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	updated, _ = loading.Update(sessionListResultMsg{
		generation: loading.resume.generation,
		result:     session.ListResult{Sessions: sessions, Skipped: backendSkipped},
	})
	presentedSessions := updated.(Model).resume.sessions
	if len(presentedSessions) != wantSessions || len(presentedSessions[0].ID) > resumeMaxFieldBytes {
		t.Fatalf("presented sessions = %#v", presentedSessions)
	}
	assertResumeSingleLineControlSafe(t, "presented session ID", presentedSessions[0].ID)

	oversizedError := errors.New("failure\n\x1b[31m" + strings.Repeat("x", resumeMaxErrorBytes*10))
	backend.listSessions = func(context.Context, int) (session.ListResult, error) {
		return session.ListResult{}, oversizedError
	}
	message = runCommandWithin(t, runSessionListCommand(context.Background(), backend, 42), time.Second)
	failed := message.(sessionListResultMsg)
	if failed.errText == "" || len(failed.errText) > resumeMaxErrorBytes {
		t.Fatalf("bounded error bytes = %d, value=%q", len(failed.errText), failed.errText)
	}
	assertResumeSingleLineControlSafe(t, "list error", failed.errText)

	m = resizeModel(t, newTestResumeModel(t, backend), 80, 12)
	m.editor.SetValue("/resume")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	loading = updated.(Model)
	updated, _ = loading.Update(runCommandWithin(t, cmd, time.Second))
	presented := updated.(Model)
	if presented.resume.errText != failed.errText || presented.statusText != failed.errText || len(presented.statusText) > resumeMaxErrorBytes {
		t.Fatalf("resume error=%q status=%q", presented.resume.errText, presented.statusText)
	}
}

func TestResumeWorkerBoundsWarningsErrorsAndStatusSuffix(t *testing.T) {
	warningCount := resumeMaxWarningCount * 1000
	warnings := make([]session.Warning, warningCount)
	for index := range warnings {
		warnings[index].Message = "warning\n\x1b" + strings.Repeat("w", resumeMaxWarningBytes*2)
	}
	backend := &resumeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "old", SessionPath: "/sessions/old.jsonl"},
	}
	backend.resumeSession = func(_ context.Context, path string) (app.ResumeResult, error) {
		backend.info = app.Info{Profile: "profile", Model: "model", SessionID: "fresh", SessionPath: path}
		return app.ResumeResult{Warnings: warnings}, nil
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "fresh", Path: "/sessions/fresh.jsonl"}}})
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	message := runCommandWithin(t, cmd, time.Second)
	result := message.(sessionResumeResultMsg)
	if result.errText != "" || len(result.result.Warnings) != resumeMaxWarningCount {
		t.Fatalf("err=%q warnings=%d", result.errText, len(result.result.Warnings))
	}
	if result.warningsSkipped != warningCount-resumeMaxWarningCount {
		t.Fatalf("warningsSkipped = %d, want %d", result.warningsSkipped, warningCount-resumeMaxWarningCount)
	}
	for index, warning := range result.result.Warnings {
		if len(warning.Message) > resumeMaxWarningBytes {
			t.Fatalf("warning %d bytes = %d", index, len(warning.Message))
		}
		assertResumeSingleLineControlSafe(t, fmt.Sprintf("warning %d", index), warning.Message)
	}

	updated, _ = resuming.Update(result)
	got := updated.(Model)
	wantSuffix := fmt.Sprintf("(+%d more warnings)", warningCount-1)
	if !strings.Contains(got.statusText, wantSuffix) {
		t.Fatalf("status = %q, want suffix %q", got.statusText, wantSuffix)
	}
	if len(got.statusText) > resumeMaxWarningBytes+64 {
		t.Fatalf("status bytes = %d", len(got.statusText))
	}
	assertResumeSingleLineControlSafe(t, "warning status", got.statusText)

	oversizedError := errors.New("resume failed\r\x1b" + strings.Repeat("z", resumeMaxErrorBytes*10))
	backend.resumeSession = func(context.Context, string) (app.ResumeResult, error) {
		return app.ResumeResult{}, oversizedError
	}
	m = loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "other", Path: "/sessions/other.jsonl"}}})
	updated, cmd = m.Update(keyPress(tea.KeyEnter))
	resuming = updated.(Model)
	failure := runCommandWithin(t, cmd, time.Second).(sessionResumeResultMsg)
	if failure.errText == "" || len(failure.errText) > resumeMaxErrorBytes {
		t.Fatalf("resume error bytes = %d, value=%q", len(failure.errText), failure.errText)
	}
	assertResumeSingleLineControlSafe(t, "resume error", failure.errText)
	updated, _ = resuming.Update(failure)
	got = updated.(Model)
	if got.resume.errText != failure.errText || got.statusText != failure.errText {
		t.Fatalf("resume error=%q status=%q", got.resume.errText, got.statusText)
	}
}

func TestResumePickerEscapeCancelsLoadingAndWaitsForWorkerAcknowledgment(t *testing.T) {
	var calls atomic.Int32
	started := make(chan struct{})
	cancelObserved := make(chan struct{})
	backend := &resumeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		listSessions: func(ctx context.Context, _ int) (session.ListResult, error) {
			if calls.Add(1) == 1 {
				close(started)
				<-ctx.Done()
				close(cancelObserved)
				return session.ListResult{Sessions: []session.SessionInfo{{ID: "stale", Path: "/sessions/stale.jsonl"}}}, ctx.Err()
			}
			return session.ListResult{Sessions: []session.SessionInfo{{ID: "fresh", Path: "/sessions/fresh.jsonl"}}}, nil
		},
	}
	m := resizeModel(t, newTestResumeModel(t, backend), 80, 12)
	m.editor.SetValue("/resume")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	if cmd == nil || !loading.resume.listPending {
		t.Fatalf("cmd=%v resume=%#v", cmd, loading.resume)
	}
	resultChannel := make(chan tea.Msg, 1)
	go func() { resultChannel <- cmd() }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("list worker did not start")
	}

	updated, escapeCmd := loading.Update(keyPress(tea.KeyEscape))
	closed := updated.(Model)
	if escapeCmd != nil || closed.resume.mode != resumeClosed || !closed.resume.listPending || !closed.reservedStateActive() {
		t.Fatalf("cmd=%v resume=%#v reserved=%v", escapeCmd, closed.resume, closed.reservedStateActive())
	}
	select {
	case <-cancelObserved:
	case <-time.After(time.Second):
		t.Fatal("list worker did not observe picker cancellation")
	}

	closed.editor.SetValue("/resume")
	updated, duplicate := closed.Update(keyPress(tea.KeyEnter))
	blocked := updated.(Model)
	if duplicate != nil || calls.Load() != 1 || blocked.statusText != app.ErrPromptActive.Error() {
		t.Fatalf("cmd=%v calls=%d status=%q", duplicate, calls.Load(), blocked.statusText)
	}

	var canceledResult tea.Msg
	select {
	case canceledResult = <-resultChannel:
	case <-time.After(time.Second):
		t.Fatal("canceled list worker did not return")
	}
	updated, resultCmd := blocked.Update(canceledResult)
	acknowledged := updated.(Model)
	if resultCmd != nil || acknowledged.resume.mode != resumeClosed || acknowledged.resume.listPending || len(acknowledged.resume.sessions) != 0 || acknowledged.reservedStateActive() {
		t.Fatalf("cmd=%v resume=%#v reserved=%v", resultCmd, acknowledged.resume, acknowledged.reservedStateActive())
	}
	if acknowledged.statusText != app.ErrPromptActive.Error() {
		t.Fatalf("stale canceled result changed status to %q", acknowledged.statusText)
	}

	updated, next := acknowledged.Update(keyPress(tea.KeyEnter))
	reloading := updated.(Model)
	if next == nil || reloading.resume.mode != resumeLoading || !reloading.resume.listPending {
		t.Fatalf("cmd=%v resume=%#v", next, reloading.resume)
	}
	message := runCommandWithin(t, next, time.Second).(sessionListResultMsg)
	if calls.Load() != 2 || len(message.result.Sessions) != 1 || message.result.Sessions[0].ID != "fresh" {
		t.Fatalf("calls=%d result=%#v", calls.Load(), message.result)
	}
}

func TestResumeListWorkerRootCancellationReleasesOwnership(t *testing.T) {
	rootCtx, cancelRoot := context.WithCancel(context.Background())
	started := make(chan struct{})
	backend := &resumeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		listSessions: func(ctx context.Context, _ int) (session.ListResult, error) {
			close(started)
			<-ctx.Done()
			return session.ListResult{}, ctx.Err()
		},
	}
	m := resizeModel(t, NewModel(rootCtx, backend, WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	}))), 80, 12)
	m.editor.SetValue("/resume")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	resultChannel := make(chan tea.Msg, 1)
	go func() { resultChannel <- cmd() }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("list worker did not start")
	}

	cancelRoot()
	var result tea.Msg
	select {
	case result = <-resultChannel:
	case <-time.After(time.Second):
		t.Fatal("root-canceled list worker did not return")
	}
	updated, next := loading.Update(result)
	got := updated.(Model)
	if next != nil || got.resume.mode != resumeLoadError || got.resume.listPending || got.resume.errText != context.Canceled.Error() {
		t.Fatalf("cmd=%v resume=%#v", next, got.resume)
	}
}

func TestResumePickerEscapeDoesNotCancelResumeWorker(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	ctxErr := make(chan error, 1)
	backend := &resumeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "old", SessionPath: "/sessions/old.jsonl"},
	}
	backend.resumeSession = func(ctx context.Context, path string) (app.ResumeResult, error) {
		close(started)
		<-release
		ctxErr <- ctx.Err()
		backend.info.SessionID = "fresh"
		backend.info.SessionPath = path
		return app.ResumeResult{}, nil
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "fresh", Path: "/sessions/fresh.jsonl"}}})
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	resultChannel := make(chan tea.Msg, 1)
	go func() { resultChannel <- cmd() }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("resume worker did not start")
	}

	updated, escapeCmd := resuming.Update(keyPress(tea.KeyEscape))
	blocked := updated.(Model)
	if escapeCmd != nil || blocked.resume.mode != resumeResuming || blocked.statusText != "resume in progress" {
		t.Fatalf("cmd=%v resume=%#v status=%q", escapeCmd, blocked.resume, blocked.statusText)
	}
	close(release)
	select {
	case err := <-ctxErr:
		if err != nil {
			t.Fatalf("resume context error after Escape = %v", err)
		}
	case <-time.After(time.Second):
		t.Fatal("resume worker did not report context state")
	}
	select {
	case <-resultChannel:
	case <-time.After(time.Second):
		t.Fatal("resume worker did not return")
	}
}

func assertResumeSingleLineControlSafe(t *testing.T, name, value string) {
	t.Helper()
	for _, r := range value {
		if unicode.IsControl(r) {
			t.Fatalf("%s contains control %U: %q", name, r, value)
		}
	}
}
