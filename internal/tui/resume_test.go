package tui

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
	"unicode"
	"unsafe"

	"charm.land/bubbles/v2/textarea"
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

func (b *resumeBackend) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{Noop: true}, nil
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

func loadedResumeModel(t *testing.T, count int) Model {
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
	backend := &resumeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "current"},
		resumeSession: func(_ context.Context, path string) (app.ResumeResult, error) {
			return app.ResumeResult{SessionPath: path}, nil
		},
	}
	m := NewModel(context.Background(), backend, WithClock(newFakeClock(now)), WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	})))
	m.resume = resumePickerState{mode: resumeLoaded, sessions: sessions}
	return m
}

func updateResumeKey(t *testing.T, model Model, code rune, modifiers ...tea.KeyMod) (Model, tea.Cmd) {
	t.Helper()
	updated, cmd := model.Update(keyPress(code, modifiers...))
	got, ok := updated.(Model)
	if !ok {
		t.Fatalf("updated model type = %T, want Model", updated)
	}
	return got, cmd
}

func resumeRowForLabel(t *testing.T, content, label string) string {
	t.Helper()
	for _, line := range strings.Split(content, "\n") {
		if strings.Contains(line, label) {
			return line
		}
	}
	t.Fatalf("view = %q, want row labeled %q", content, label)
	return ""
}

func assertResumeRowMarkers(t *testing.T, content, label string, selected, current bool) {
	t.Helper()
	row := resumeRowForLabel(t, content, label)
	if got := strings.Contains(row, ">"); got != selected {
		t.Fatalf("row %q selected marker = %v, want %v", row, got, selected)
	}
	if got := strings.Contains(row, "*"); got != current {
		t.Fatalf("row %q current marker = %v, want %v", row, got, current)
	}
}

func TestResumePickerClampsMalformedSelectionForTitleRangeAndRowMarker(t *testing.T) {
	m := resizeModel(t, loadedResumeModel(t, 3), 80, 12)
	m.resume.selected = 99
	content := m.View().Content
	if !strings.Contains(content, "3/3") {
		t.Fatalf("view = %q, want clamped title count", content)
	}
	assertResumeRowMarkers(t, content, "Session 03", true, false)
	assertResumeRowMarkers(t, content, "Session 01", false, false)
}

func TestResumePickerAtMinimumSizeShowsSelectionAndControlsWithinBounds(t *testing.T) {
	m := resizeModel(t, loadedResumeModel(t, 20), 40, 8)
	content := m.View().Content
	assertRenderedBounds(t, content, 40, 8)
	for _, text := range []string{"Resume", "1/20", "Session 01", "Enter", "Esc"} {
		if !strings.Contains(content, text) {
			t.Fatalf("view = %q, want %q", content, text)
		}
	}
}

func TestResumePickerNavigationPagesAndKeepsSelectionVisible(t *testing.T) {
	m := resizeModel(t, loadedResumeModel(t, 20), 80, 12)
	m, _ = updateResumeKey(t, m, tea.KeyDown)
	assertResumeRowMarkers(t, m.View().Content, "Session 02", true, false)

	m, _ = updateResumeKey(t, m, tea.KeyPgDown)
	start, end := resumeVisibleRange(len(m.resume.sessions), m.resume.selected, resumeVisibleRows(m.width, m.height))
	if m.resume.selected < start || m.resume.selected >= end {
		t.Fatalf("range=%d:%d selected=%d", start, end, m.resume.selected)
	}
	if m.resume.selected != 1+resumeVisibleRows(80, 12) {
		t.Fatalf("selected = %d, want one page after index 1", m.resume.selected)
	}
	assertResumeRowMarkers(t, m.View().Content, m.resume.sessions[m.resume.selected].Name, true, false)

	m, _ = updateResumeKey(t, m, tea.KeyPgUp)
	assertResumeRowMarkers(t, m.View().Content, "Session 02", true, false)
	m, _ = updateResumeKey(t, m, tea.KeyUp)
	if m.resume.selected != 0 {
		t.Fatalf("selected after page/up = %d, want 0", m.resume.selected)
	}
	assertResumeRowMarkers(t, m.View().Content, "Session 01", true, false)
}

type resumeEditorSnapshot struct {
	value                        string
	line, column                 int
	selectionStart, selectionEnd [2]int
	hasSelection                 bool
	selectedText                 string
	scrollOffset                 int
}

func snapshotResumeEditor(editor textarea.Model) resumeEditorSnapshot {
	start, end, selected := editor.Selection()
	return resumeEditorSnapshot{
		value:          editor.Value(),
		line:           editor.Line(),
		column:         editor.Column(),
		selectionStart: [2]int{start.Row, start.Col},
		selectionEnd:   [2]int{end.Row, end.Col},
		hasSelection:   selected,
		selectedText:   editor.SelectedText(),
		scrollOffset:   editor.ScrollYOffset(),
	}
}

func modalEditorModel(t *testing.T) Model {
	t.Helper()
	m := resizeModel(t, loadedResumeModel(t, 3), 80, 12)
	m.editor.SetValue(strings.Repeat("hidden draft line\n", 10) + "final")
	m.editor.CursorEnd()
	m.editor.SelectAll()
	if !m.editor.HasSelection() {
		t.Fatal("modal editor fixture has no selection")
	}
	return m
}

func TestResumePickerModalInputsPreserveCompleteEditorStateAndHideCursor(t *testing.T) {
	inputs := []struct {
		name string
		msg  tea.Msg
	}{
		{name: "shift down", msg: keyPress(tea.KeyDown, tea.ModShift)},
		{name: "alt up", msg: keyPress(tea.KeyUp, tea.ModAlt)},
		{name: "ctrl page up", msg: keyPress(tea.KeyPgUp, tea.ModCtrl)},
		{name: "ctrl page down", msg: keyPress(tea.KeyPgDown, tea.ModCtrl)},
		{name: "shift enter", msg: keyPress(tea.KeyEnter, tea.ModShift)},
		{name: "alt escape", msg: keyPress(tea.KeyEscape, tea.ModAlt)},
		{name: "tab", msg: keyPress(tea.KeyTab)},
		{name: "text", msg: keyPress('x')},
		{name: "paste", msg: tea.PasteMsg{Content: "pasted"}},
		{name: "mouse", msg: tea.MouseClickMsg(tea.Mouse{X: 2, Y: 2, Button: tea.MouseLeft})},
	}
	for _, tc := range inputs {
		t.Run(tc.name, func(t *testing.T) {
			m := modalEditorModel(t)
			before := snapshotResumeEditor(m.editor)
			updated, cmd := m.Update(tc.msg)
			got := updated.(Model)
			if after := snapshotResumeEditor(got.editor); after != before {
				t.Fatalf("editor state changed:\n before=%#v\n after=%#v", before, after)
			}
			if cmd != nil || got.resume.mode != resumeLoaded || got.resume.selected != 0 {
				t.Fatalf("cmd=%v resume=%#v", cmd, got.resume)
			}
			if cursor := got.View().Cursor; cursor != nil {
				t.Fatalf("modal view cursor = %#v, want nil", cursor)
			}
		})
	}
}

func TestResumePickerRoutesOnlyExactUnmodifiedModalKeys(t *testing.T) {
	modified := []struct {
		name string
		code rune
		mod  tea.KeyMod
	}{
		{name: "shift down", code: tea.KeyDown, mod: tea.ModShift},
		{name: "alt up", code: tea.KeyUp, mod: tea.ModAlt},
		{name: "ctrl page up", code: tea.KeyPgUp, mod: tea.ModCtrl},
		{name: "ctrl page down", code: tea.KeyPgDown, mod: tea.ModCtrl},
		{name: "shift enter", code: tea.KeyEnter, mod: tea.ModShift},
		{name: "alt escape", code: tea.KeyEscape, mod: tea.ModAlt},
		{name: "tab", code: tea.KeyTab},
		{name: "text", code: 'x'},
	}
	for _, tc := range modified {
		t.Run(tc.name, func(t *testing.T) {
			m := resizeModel(t, loadedResumeModel(t, 3), 80, 12)
			m.editor.SetValue("hidden draft")
			got, cmd := updateResumeKey(t, m, tc.code, tc.mod)
			if cmd != nil || got.resume.mode != resumeLoaded || got.resume.selected != 0 || got.editor.Value() != "hidden draft" {
				t.Fatalf("cmd=%v resume=%#v editor=%q", cmd, got.resume, got.editor.Value())
			}
		})
	}

	m := resizeModel(t, loadedResumeModel(t, 3), 80, 12)
	m, _ = updateResumeKey(t, m, tea.KeyDown)
	if m.resume.selected != 1 {
		t.Fatalf("exact Down selected = %d, want 1", m.resume.selected)
	}
	m, _ = updateResumeKey(t, m, tea.KeyEscape)
	if m.resume.active() {
		t.Fatalf("exact Escape left picker active: %#v", m.resume)
	}

	empty := resizeModel(t, loadedResumeModel(t, 0), 80, 12)
	empty, _ = updateResumeKey(t, empty, tea.KeyUp)
	if empty.resume.selected != 0 {
		t.Fatalf("empty picker selected = %d, want clamped 0", empty.resume.selected)
	}

	m = resizeModel(t, loadedResumeModel(t, 3), 80, 12)
	m.editor.SetValue("hidden draft")
	updated, cmd := m.Update(tea.PasteMsg{Content: "pasted"})
	pasted := updated.(Model)
	if cmd != nil || pasted.editor.Value() != "hidden draft" || pasted.resume.mode != resumeLoaded {
		t.Fatalf("modal paste: cmd=%v editor=%q resume=%#v", cmd, pasted.editor.Value(), pasted.resume)
	}
}

func TestResumePickerExactEscapeBehaviorInEveryMode(t *testing.T) {
	for _, mode := range []resumeMode{resumeLoading, resumeLoaded, resumeLoadError, resumeResumeError} {
		t.Run(fmt.Sprintf("mode-%d", mode), func(t *testing.T) {
			m := resizeModel(t, loadedResumeModel(t, 1), 80, 12)
			m.resume.mode = mode
			got, cmd := updateResumeKey(t, m, tea.KeyEscape)
			if cmd != nil || got.resume.active() {
				t.Fatalf("cmd=%v resume=%#v", cmd, got.resume)
			}
		})
	}

	m := resizeModel(t, loadedResumeModel(t, 1), 80, 12)
	m.resume.mode = resumeResuming
	got, cmd := updateResumeKey(t, m, tea.KeyEscape)
	if cmd != nil || got.resume.mode != resumeResuming || got.statusText != "resume in progress" {
		t.Fatalf("resuming Escape: cmd=%v resume=%#v status=%q", cmd, got.resume, got.statusText)
	}
}

func TestResumePickerCtrlCRemainsGlobalFirstPriority(t *testing.T) {
	m := resizeModel(t, loadedResumeModel(t, 3), 80, 12)
	got, cmd := updateResumeKey(t, m, 'c', tea.ModCtrl)
	if cmd == nil || !got.ctrlCArmed || got.resume.mode != resumeLoaded {
		t.Fatalf("cmd=%v armed=%v resume=%#v", cmd, got.ctrlCArmed, got.resume)
	}
}

func TestResumePickerResumingDisablesActionsAndEscapeReportsStatus(t *testing.T) {
	m := resizeModel(t, loadedResumeModel(t, 3), 80, 12)
	m.resume.mode = resumeResuming
	m.resume.selected = 1
	for _, code := range []rune{tea.KeyUp, tea.KeyDown, tea.KeyPgUp, tea.KeyPgDown, tea.KeyEnter} {
		got, cmd := updateResumeKey(t, m, code)
		if cmd != nil || got.resume.mode != resumeResuming || got.resume.selected != 1 {
			t.Fatalf("key %v: cmd=%v resume=%#v", code, cmd, got.resume)
		}
		m = got
	}
	got, cmd := updateResumeKey(t, m, tea.KeyEscape)
	if cmd != nil || got.resume.mode != resumeResuming || got.statusText != "resume in progress" {
		t.Fatalf("cmd=%v resume=%#v status=%q", cmd, got.resume, got.statusText)
	}
}

func TestResumePickerProgressivelyAddsMetadata(t *testing.T) {
	for _, tc := range []struct {
		width   int
		present []string
		absent  []string
	}{
		{width: 40, present: []string{"Session 01"}, absent: []string{"default/test-model", "openai-compatible", "1 msgs"}},
		{width: 80, present: []string{"default/test-model", "Session 01"}, absent: []string{"openai-compatible", "1 msgs"}},
		{width: 120, present: []string{"default/test-model", "openai-compatible", "1 msgs", "Session 01"}},
	} {
		t.Run(fmt.Sprintf("width-%d", tc.width), func(t *testing.T) {
			content := resizeModel(t, loadedResumeModel(t, 1), tc.width, 12).View().Content
			assertRenderedBounds(t, content, tc.width, 12)
			for _, want := range tc.present {
				if !strings.Contains(content, want) {
					t.Fatalf("view = %q, want %q", content, want)
				}
			}
			for _, unwanted := range tc.absent {
				if strings.Contains(content, unwanted) {
					t.Fatalf("view = %q, do not want %q", content, unwanted)
				}
			}
		})
	}
}

func TestResumePickerRendersSelectedAndCurrentMarkersOnIdentifiedRows(t *testing.T) {
	m := resizeModel(t, loadedResumeModel(t, 3), 80, 12)
	m.resume.sessions[0].Current = true
	m, _ = updateResumeKey(t, m, tea.KeyDown)
	content := m.View().Content
	assertResumeRowMarkers(t, content, "Session 01", false, true)
	assertResumeRowMarkers(t, content, "Session 02", true, false)
}

func TestResumePickerRendersLoadingEmptyErrorsCurrentAndResumingStates(t *testing.T) {
	base := resizeModel(t, loadedResumeModel(t, 1), 80, 12)
	tests := []struct {
		name string
		set  func(*Model)
		want []string
	}{
		{name: "loading", set: func(m *Model) { m.resume = resumePickerState{mode: resumeLoading} }, want: []string{base.spinner.View(), "Loading sessions", "Esc"}},
		{name: "empty skipped", set: func(m *Model) { m.resume = resumePickerState{mode: resumeLoaded, skipped: 2} }, want: []string{"No resumable sessions", "skipped 2", "Esc"}},
		{name: "load error", set: func(m *Model) { m.resume = resumePickerState{mode: resumeLoadError, errText: "list failed"} }, want: []string{"Unable to load sessions", "list failed", "Esc"}},
		{name: "resume error", set: func(m *Model) { m.resume.mode = resumeResumeError; m.resume.errText = "resume failed" }, want: []string{"resume failed", "Enter", "Esc"}},
		{name: "current", set: func(m *Model) { m.resume.sessions[0].Current = true }, want: []string{"current", "Session 01", "1/1"}},
		{name: "resuming", set: func(m *Model) { m.resume.mode = resumeResuming }, want: []string{"Resuming", "disabled", "Esc"}},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			m := base
			m.resume.sessions = append([]session.SessionInfo(nil), base.resume.sessions...)
			tc.set(&m)
			content := m.View().Content
			assertRenderedBounds(t, content, 80, 12)
			for _, want := range tc.want {
				if !strings.Contains(content, want) {
					t.Fatalf("view = %q, want %q", content, want)
				}
			}
		})
	}
}

func TestFormatRelativeSessionAge(t *testing.T) {
	now := time.Date(2026, time.August, 27, 12, 0, 0, 0, time.UTC)
	for _, tc := range []struct {
		name string
		at   time.Time
		want string
	}{
		{name: "zero", at: now, want: "now"},
		{name: "future", at: now.Add(time.Hour), want: "now"},
		{name: "minutes", at: now.Add(-5 * time.Minute), want: "5m"},
		{name: "hours", at: now.Add(-3 * time.Hour), want: "3h"},
		{name: "days", at: now.Add(-2 * 24 * time.Hour), want: "2d"},
		{name: "weeks", at: now.Add(-14 * 24 * time.Hour), want: "2w"},
		{name: "months", at: now.Add(-60 * 24 * time.Hour), want: "2mo"},
		{name: "years", at: now.Add(-730 * 24 * time.Hour), want: "2y"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := formatRelativeSessionAge(now, tc.at); got != tc.want {
				t.Fatalf("formatRelativeSessionAge() = %q, want %q", got, tc.want)
			}
		})
	}
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

func TestResumeSuccessRendersFoldedCompactionBeforeRetainedTail(t *testing.T) {
	backend := &resumeBackend{
		info: app.Info{Profile: "old-profile", Model: "old-model", SessionID: "session-old"},
		history: []model.Message{{
			Role:   model.RoleAssistant,
			Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}},
		}},
	}
	backend.resumeSession = func(_ context.Context, path string) (app.ResumeResult, error) {
		backend.info = app.Info{Profile: "new-profile", Model: "new-model", SessionID: "session-new"}
		backend.history = []model.Message{
			{ID: "checkpoint", Role: model.RoleContext, ContextType: "compaction", Display: true, ContextTokensBefore: 258000, Blocks: []model.Block{{Type: model.BlockText, Text: "[Compaction summary]\n## Goal\nship it"}}},
			{ID: "tail-user", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "retained request"}}},
			{ID: "tail-assistant", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "retained answer"}}},
		}
		return app.ResumeResult{SessionPath: path}, nil
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "fresh", Path: "/sessions/fresh.jsonl"}}})

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	if cmd == nil {
		t.Fatal("resume cmd = nil")
	}
	updated, _ = resuming.Update(runCommandWithin(t, cmd, time.Second))
	got := updated.(Model)
	content := got.transcriptContent(got.transcriptWidth())
	checkpoint := strings.Index(content, "[context] compacted 258k tokens")
	tail := strings.Index(content, "retained request")
	if checkpoint < 0 || tail < 0 || checkpoint >= tail {
		t.Fatalf("view = %q, want folded compaction before retained tail", content)
	}
	if strings.Contains(content, "[Compaction summary]") {
		t.Fatalf("view = %q, want internal compaction prefix hidden", content)
	}
}

func TestResumeSuccessReplacesHistoryAndClearsStaleState(t *testing.T) {
	backend := &resumeBackend{
		info: app.Info{Profile: "old-profile", Model: "old-model", SessionID: "session-old", ContextWindow: 128_000, ContextInputTokens: 64_000, ContextInputTokensPresent: true},
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
		backend.info = app.Info{
			Profile: "new-profile", Model: "new-model", SessionID: "session-new",
			Usage: model.Usage{InputTokens: 20, OutputTokens: 6}, UsagePresent: true,
			ContextWindow: 128_000, ContextInputTokens: 16_000, ContextInputTokensPresent: true,
		}
		backend.history = []model.Message{{
			Role:   model.RoleAssistant,
			Blocks: []model.Block{{Type: model.BlockText, Text: "fresh transcript"}},
			Usage:  &model.Usage{InputTokens: 7, OutputTokens: 9},
		}}
		return app.ResumeResult{
			SessionPath: path,
			Warnings:    []session.Warning{{Message: "warning: repaired trailing newline"}},
		}, nil
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "fresh", Path: "/sessions/fresh.jsonl"}}})
	m = resizeModel(t, m, 120, 12)
	m.editor.SetValue("draft")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	if cmd == nil || resuming.resume.mode != resumeResuming || !strings.Contains(resuming.View().Content, "Resuming") {
		t.Fatalf("cmd=%v resume=%#v view=%q", cmd, resuming.resume, resuming.View().Content)
	}
	result := runCommandWithin(t, cmd, time.Second)
	if content := resuming.View().Content; strings.Contains(content, "old transcript") || strings.Contains(content, "fresh transcript") {
		t.Fatalf("resuming modal leaked transcript = %q", content)
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
	resuming.turnHistoryBaseline = turnHistoryBaseline{idsJSON: `["before"]`, valid: true}
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
	if got.usage.InputTokens != 20 || got.usage.OutputTokens != 6 {
		t.Fatalf("usage = %#v", got.usage)
	}
	if content := got.View().Content; strings.Contains(content, "old transcript") || !strings.Contains(content, "fresh transcript") || !strings.Contains(content, "new-profile/new-model") || !strings.Contains(content, "12.5%") || strings.Contains(content, "50.0%") {
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
	if content := got.View().Content; !strings.Contains(content, "resume failed") || strings.Contains(content, "keep transcript") {
		t.Fatalf("error modal = %q", content)
	}
	restored := got
	restored.closeResumePicker()
	if content := restored.View().Content; !strings.Contains(content, "keep transcript") || !strings.Contains(content, "old-profile/old-model") || !strings.Contains(content, "session-old") {
		t.Fatalf("restored view = %q", content)
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
		return app.ResumeResult{
			SessionPath: path,
			Warnings:    []session.Warning{{Message: "stale payload warning"}},
		}, nil
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "fresh", Path: "/sessions/fresh.jsonl"}}})
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	if cmd == nil {
		t.Fatal("resume cmd = nil")
	}

	result := runCommandWithin(t, cmd, time.Second)
	resuming.closeResumePicker()
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

func TestResumeResultReconcilesAliasRequestUsingCanonicalCommittedPath(t *testing.T) {
	directory := t.TempDir()
	canonicalPath := filepath.Join(directory, "canonical.jsonl")
	if err := os.WriteFile(canonicalPath, []byte("session"), 0o600); err != nil {
		t.Fatal(err)
	}
	aliasPath := filepath.Join(directory, "alias.jsonl")
	if err := os.Symlink(canonicalPath, aliasPath); err != nil {
		t.Fatal(err)
	}
	canonicalPath, err := filepath.EvalSymlinks(canonicalPath)
	if err != nil {
		t.Fatal(err)
	}

	backend := &resumeBackend{
		info:    app.Info{Profile: "old-profile", Model: "old-model", SessionID: "session-old", SessionPath: "/sessions/old.jsonl"},
		history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}}}},
	}
	backend.resumeSession = func(_ context.Context, path string) (app.ResumeResult, error) {
		if path != aliasPath {
			t.Fatalf("resume path = %q, want alias %q", path, aliasPath)
		}
		backend.info = app.Info{Profile: "new-profile", Model: "new-model", SessionID: "session-new", SessionPath: canonicalPath}
		backend.history = []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "canonical transcript"}}}}
		return app.ResumeResult{SessionPath: canonicalPath}, nil
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "canonical", Path: aliasPath}}})
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	result := runCommandWithin(t, cmd, time.Second)

	resuming.closeResumePicker()
	resuming.resume.generation++
	updated, next := resuming.Update(result)
	got := updated.(Model)
	if next != nil || got.resume.mode != resumeClosed || got.statusText != "resumed session" {
		t.Fatalf("cmd=%v resume=%#v status=%q", next, got.resume, got.statusText)
	}
	if content := got.View().Content; strings.Contains(content, "old transcript") || !strings.Contains(content, "canonical transcript") {
		t.Fatalf("view = %q", content)
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
		return app.ResumeResult{SessionPath: path}, nil
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

func TestResumeResultCommittedStaleSuccessFailsClosedDuringSamePathNewerResume(t *testing.T) {
	backend := &resumeBackend{
		info:    app.Info{Profile: "old-profile", Model: "old-model", SessionID: "session-old", SessionPath: "/sessions/old.jsonl"},
		history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}}}},
	}
	backend.resumeSession = func(_ context.Context, path string) (app.ResumeResult, error) {
		backend.info.SessionPath = path
		backend.info.SessionID = "session-middle"
		backend.history = []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "middle transcript"}}}}
		return app.ResumeResult{SessionPath: path}, nil
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "middle", Path: "/sessions/middle.jsonl"}}})
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	staleResult := runCommandWithin(t, cmd, time.Second)

	newerGeneration := resuming.resume.generation + 1
	resuming.resume.generation = newerGeneration
	resuming.resume.mode = resumeResuming
	resuming.resume.operationPath = "/sessions/middle.jsonl"
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
	if got.resume.mode != resumeResuming || got.resume.generation != newerGeneration || got.resume.operationPath != "/sessions/middle.jsonl" {
		t.Fatalf("newer same-path resume was reset or closed: %#v", got.resume)
	}
	if content := got.View().Content; strings.Contains(content, "middle transcript") || strings.Contains(content, "old transcript") || !strings.Contains(content, "Resuming") {
		t.Fatalf("modal before quit = %q", content)
	}
	restored := got
	restored.closeResumePicker()
	if content := restored.View().Content; strings.Contains(content, "middle transcript") || !strings.Contains(content, "old transcript") {
		t.Fatalf("underlying view before quit = %q", content)
	}
}

func TestResumeResultFailClosedCancelsActiveListOwnership(t *testing.T) {
	listCtx, cancelList := context.WithCancel(context.Background())
	backend := &resumeBackend{
		info:    app.Info{Profile: "profile", Model: "model", SessionID: "committed", SessionPath: "/sessions/committed.jsonl"},
		history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}}}},
	}
	m := resizeModel(t, newTestResumeModel(t, backend), 80, 12)
	m.resume = resumePickerState{
		mode:        resumeLoading,
		generation:  2,
		listPending: true,
		listCancel:  cancelList,
	}

	updated, quit := m.Update(sessionResumeResultMsg{
		generation: 1,
		path:       "/sessions/committed.jsonl",
		result:     app.ResumeResult{SessionPath: "/sessions/committed.jsonl"},
	})
	got := updated.(Model)
	if quit == nil {
		t.Fatal("quit command = nil")
	}
	if err := listCtx.Err(); !errors.Is(err, context.Canceled) {
		t.Fatalf("list context error = %v, want context.Canceled before quit", err)
	}
	if !got.resume.listPending || got.resume.mode != resumeLoading || got.resume.generation != 2 {
		t.Fatalf("active list ownership was reset: %#v", got.resume)
	}
	if got.fatalErr == nil {
		t.Fatal("fatalErr = nil, want fail-closed reconciliation error")
	}
}

func TestResumeResultStaleSuccessWithUnusableCommittedPathFailsClosed(t *testing.T) {
	tests := []struct {
		name          string
		committedPath string
	}{
		{name: "oversized", committedPath: strings.Repeat("/", resumeMaxPathBytes+1)},
		{name: "control-bearing", committedPath: "/sessions/unsafe\n.jsonl"},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			listCtx, cancelList := context.WithCancel(context.Background())
			backend := &resumeBackend{
				info:    app.Info{Profile: "profile", Model: "model", SessionID: "old", SessionPath: "/sessions/old.jsonl"},
				history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}}}},
				resumeSession: func(context.Context, string) (app.ResumeResult, error) {
					return app.ResumeResult{
						SessionPath: tc.committedPath,
						Warnings:    []session.Warning{{Message: "unsafe warning\n\x1b[31m"}},
					}, nil
				},
			}
			message := runCommandWithin(t, runSessionResumeCommand(context.Background(), backend, 1, "/sessions/request.jsonl"), time.Second).(sessionResumeResultMsg)
			if message.result.SessionPath != "" {
				t.Fatalf("bounded committed path = %q, want empty unsafe-path drop", message.result.SessionPath)
			}
			if message.committedPathState != resumeCommittedPathUnusable {
				t.Fatalf("committedPathState = %v, want %v", message.committedPathState, resumeCommittedPathUnusable)
			}

			m := resizeModel(t, newTestResumeModel(t, backend), 80, 12)
			m.statusText = "keep status"
			m.resume = resumePickerState{
				mode:        resumeLoading,
				generation:  2,
				listPending: true,
				listCancel:  cancelList,
			}
			updated, quit := m.Update(message)
			got := updated.(Model)
			if quit == nil {
				t.Fatal("quit command = nil")
			}
			if _, ok := quit().(tea.QuitMsg); !ok {
				t.Fatalf("quit command message = %T, want tea.QuitMsg", quit())
			}
			if err := listCtx.Err(); !errors.Is(err, context.Canceled) {
				t.Fatalf("list context error = %v, want context.Canceled before quit", err)
			}
			if got.statusText != errResumeReconciliationUnsafe.Error() {
				t.Fatalf("status = %q, want %q", got.statusText, errResumeReconciliationUnsafe.Error())
			}
			if strings.Contains(got.statusText, tc.committedPath) || strings.Contains(got.statusText, "unsafe warning") {
				t.Fatalf("status retained unsafe stale payload: %q", got.statusText)
			}
			assertResumeSingleLineControlSafe(t, "status", got.statusText)
			if got.fatalErr == nil {
				t.Fatal("fatalErr = nil, want fail-closed reconciliation error")
			}
			if got.resume.mode != resumeLoading || !got.resume.listPending || got.resume.generation != 2 {
				t.Fatalf("active list ownership was reset: %#v", got.resume)
			}
		})
	}
}

func TestResumeResultStaleSuccessWithMissingCommittedPathFailsClosed(t *testing.T) {
	backend := &resumeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "old", SessionPath: "/sessions/old.jsonl"},
		resumeSession: func(context.Context, string) (app.ResumeResult, error) {
			return app.ResumeResult{Warnings: []session.Warning{{Message: "malformed success warning"}}}, nil
		},
	}
	message := runCommandWithin(t, runSessionResumeCommand(context.Background(), backend, 1, "/sessions/request.jsonl"), time.Second).(sessionResumeResultMsg)
	if message.result.SessionPath != "" {
		t.Fatalf("bounded committed path = %q, want empty malformed-path payload", message.result.SessionPath)
	}
	if message.committedPathState != resumeCommittedPathMissing {
		t.Fatalf("committedPathState = %v, want %v", message.committedPathState, resumeCommittedPathMissing)
	}

	m := resizeModel(t, newTestResumeModel(t, backend), 80, 12)
	m.statusText = "old status"
	m.resume.generation = 2
	updated, quit := m.Update(message)
	got := updated.(Model)
	if quit == nil {
		t.Fatal("quit command = nil")
	}
	if _, ok := quit().(tea.QuitMsg); !ok {
		t.Fatalf("quit command message = %T, want tea.QuitMsg", quit())
	}
	if got.statusText != errResumeReconciliationUnsafe.Error() {
		t.Fatalf("status = %q, want %q", got.statusText, errResumeReconciliationUnsafe.Error())
	}
	if strings.Contains(got.statusText, "malformed success warning") {
		t.Fatalf("status retained stale warning payload: %q", got.statusText)
	}
	assertResumeSingleLineControlSafe(t, "status", got.statusText)
	if got.fatalErr == nil {
		t.Fatal("fatalErr = nil, want fail-closed reconciliation error")
	}
}

func TestResumeResultForgedValidCommittedStateFailsClosedForInvalidCommittedPath(t *testing.T) {
	tests := []struct {
		name          string
		committedPath string
	}{
		{name: "empty", committedPath: ""},
		{name: "oversized", committedPath: strings.Repeat("/", resumeMaxPathBytes+1)},
		{name: "control-bearing", committedPath: "/sessions/unsafe\n.jsonl"},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			listCtx, cancelList := context.WithCancel(context.Background())
			backend := &resumeBackend{
				info:    app.Info{Profile: "profile", Model: "model", SessionID: "committed", SessionPath: "/sessions/committed.jsonl"},
				history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "current transcript"}}}},
			}
			m := resizeModel(t, newTestResumeModel(t, backend), 80, 12)
			m.statusText = "keep status"
			m.resume = resumePickerState{
				mode:        resumeLoading,
				generation:  2,
				listPending: true,
				listCancel:  cancelList,
			}

			updated, quit := m.Update(sessionResumeResultMsg{
				generation:         1,
				path:               "/sessions/request.jsonl",
				result:             app.ResumeResult{SessionPath: tc.committedPath, Warnings: []session.Warning{{Message: "forged success warning"}}},
				committedPathState: resumeCommittedPathValid,
			})
			got := updated.(Model)
			if quit == nil {
				t.Fatal("quit command = nil")
			}
			if _, ok := quit().(tea.QuitMsg); !ok {
				t.Fatalf("quit command message = %T, want tea.QuitMsg", quit())
			}
			if err := listCtx.Err(); !errors.Is(err, context.Canceled) {
				t.Fatalf("list context error = %v, want context.Canceled before quit", err)
			}
			if got.statusText != errResumeReconciliationUnsafe.Error() {
				t.Fatalf("status = %q, want %q", got.statusText, errResumeReconciliationUnsafe.Error())
			}
			if (tc.committedPath != "" && strings.Contains(got.statusText, tc.committedPath)) || strings.Contains(got.statusText, "forged success warning") {
				t.Fatalf("status retained forged payload: %q", got.statusText)
			}
			assertResumeSingleLineControlSafe(t, "status", got.statusText)
			if got.fatalErr == nil {
				t.Fatal("fatalErr = nil, want fail-closed reconciliation error")
			}
			if got.resume.mode != resumeLoading || !got.resume.listPending || got.resume.generation != 2 {
				t.Fatalf("active list ownership was reset: %#v", got.resume)
			}
		})
	}
}

func TestResumeWorkersDetachBoundedPayloadStorage(t *testing.T) {
	path := hugeBackedResumeString("/sessions/detached.jsonl")
	id := hugeBackedResumeString("detached-id")
	cwd := hugeBackedResumeString("/workspace")
	name := hugeBackedResumeString("detached name")
	lastUserText := hugeBackedResumeString("detached prompt")
	profile := hugeBackedResumeString("detached-profile")
	provider := hugeBackedResumeString("openai-compatible")
	modelName := hugeBackedResumeString("detached-model")
	location := time.FixedZone(strings.Repeat("huge-location", 1<<16), 9*60*60)
	created := time.Date(2026, time.August, 27, 12, 34, 56, 789, location)
	modified := created.Add(time.Hour)
	source := session.SessionInfo{
		Path: path, ID: id, CWD: cwd, Name: name, Created: created, Modified: modified,
		MessageCount: 7, LastUserText: lastUserText, Profile: profile, Provider: provider, Model: modelName, Current: true,
	}
	backend := &resumeBackend{listSessions: func(context.Context, int) (session.ListResult, error) {
		return session.ListResult{Sessions: []session.SessionInfo{source}}, nil
	}}

	message := runCommandWithin(t, runSessionListCommand(context.Background(), backend, 91), time.Second).(sessionListResultMsg)
	if len(message.result.Sessions) != 1 {
		t.Fatalf("sessions = %#v", message.result.Sessions)
	}
	got := message.result.Sessions[0]
	for name, pair := range map[string][2]string{
		"Path": {path, got.Path}, "ID": {id, got.ID}, "CWD": {cwd, got.CWD}, "Name": {name, got.Name},
		"LastUserText": {lastUserText, got.LastUserText}, "Profile": {profile, got.Profile},
		"Provider": {provider, got.Provider}, "Model": {modelName, got.Model},
	} {
		assertDetachedResumeString(t, name, pair[0], pair[1])
	}
	if !got.Created.Equal(created) || !got.Modified.Equal(modified) {
		t.Fatalf("times = created %v modified %v, want instants %v and %v", got.Created, got.Modified, created, modified)
	}
	if got.Created.Location() != time.UTC || got.Modified.Location() != time.UTC {
		t.Fatalf("locations = created %q modified %q, want UTC", got.Created.Location(), got.Modified.Location())
	}

	operationPath := hugeBackedResumeString("/sessions/operation.jsonl")
	committedPath := hugeBackedResumeString("/sessions/canonical.jsonl")
	warningText := hugeBackedResumeString("detached warning")
	backend.resumeSession = func(context.Context, string) (app.ResumeResult, error) {
		return app.ResumeResult{
			SessionPath: committedPath,
			Warnings:    []session.Warning{{Message: warningText}},
		}, nil
	}
	resumeMessage := runCommandWithin(t, runSessionResumeCommand(context.Background(), backend, 92, operationPath), time.Second).(sessionResumeResultMsg)
	assertDetachedResumeString(t, "operation path", operationPath, resumeMessage.path)
	assertDetachedResumeString(t, "committed path", committedPath, resumeMessage.result.SessionPath)
	assertDetachedResumeString(t, "warning", warningText, resumeMessage.result.Warnings[0].Message)
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
		return app.ResumeResult{SessionPath: path, Warnings: warnings}, nil
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

func TestResumeListWorkerSecondCtrlCCancelsBeforeQuit(t *testing.T) {
	clock := newFakeClock(time.Unix(900, 0))
	started := make(chan context.Context, 1)
	workerDone := make(chan struct{})
	backend := &resumeBackend{
		listSessions: func(ctx context.Context, _ int) (session.ListResult, error) {
			started <- ctx
			<-ctx.Done()
			close(workerDone)
			return session.ListResult{}, ctx.Err()
		},
	}
	m := resizeModel(t, NewModel(context.Background(), backend, WithClock(clock), WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	}))), 80, 12)
	m.editor.SetValue("/resume")
	updated, listCmd := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	resultChannel := make(chan tea.Msg, 1)
	go func() { resultChannel <- listCmd() }()
	var workerCtx context.Context
	select {
	case workerCtx = <-started:
	case <-time.After(time.Second):
		t.Fatal("list worker did not start")
	}

	updated, armCmd := loading.Update(keyPress('c', tea.ModCtrl))
	armed := updated.(Model)
	if armCmd == nil || !armed.ctrlCArmed || workerCtx.Err() != nil {
		t.Fatalf("first Ctrl+C: cmd=%v armed=%v worker error=%v", armCmd, armed.ctrlCArmed, workerCtx.Err())
	}
	updated, quit := armed.Update(keyPress('c', tea.ModCtrl))
	quitting := updated.(Model)
	if quit == nil {
		t.Fatal("second Ctrl+C quit command = nil")
	}
	if err := workerCtx.Err(); !errors.Is(err, context.Canceled) {
		t.Fatalf("worker context error before quit = %v, want context.Canceled", err)
	}
	if !quitting.resume.listPending {
		t.Fatal("quit reset list ownership before worker acknowledgment")
	}
	select {
	case <-workerDone:
	case <-time.After(time.Second):
		t.Fatal("blocked lister did not observe ctx.Done before quit command was run")
	}
	if _, ok := quit().(tea.QuitMsg); !ok {
		t.Fatalf("quit command message = %T, want tea.QuitMsg", quit())
	}
	select {
	case <-resultChannel:
	case <-time.After(time.Second):
		t.Fatal("canceled list worker did not return")
	}
}

func TestResumeListWorkerDirectExitCancelsBeforeQuit(t *testing.T) {
	started := make(chan context.Context, 1)
	backend := &resumeBackend{listSessions: func(ctx context.Context, _ int) (session.ListResult, error) {
		started <- ctx
		<-ctx.Done()
		return session.ListResult{}, ctx.Err()
	}}
	m := resizeModel(t, newTestResumeModel(t, backend), 80, 12)
	m.editor.SetValue("/resume")
	updated, listCmd := m.Update(keyPress(tea.KeyEnter))
	loading := updated.(Model)
	resultChannel := make(chan tea.Msg, 1)
	go func() { resultChannel <- listCmd() }()
	var workerCtx context.Context
	select {
	case workerCtx = <-started:
	case <-time.After(time.Second):
		t.Fatal("list worker did not start")
	}

	updated, quit := loading.handleCommand("/exit")
	got := updated.(Model)
	if quit == nil {
		t.Fatal("direct /exit quit command = nil")
	}
	if err := workerCtx.Err(); !errors.Is(err, context.Canceled) {
		t.Fatalf("worker context error before direct quit = %v, want context.Canceled", err)
	}
	if !got.resume.listPending {
		t.Fatal("direct quit reset list ownership before acknowledgment")
	}
	if _, ok := quit().(tea.QuitMsg); !ok {
		t.Fatalf("quit command message = %T, want tea.QuitMsg", quit())
	}
	select {
	case <-resultChannel:
	case <-time.After(time.Second):
		t.Fatal("direct-quit list worker did not return")
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
		return app.ResumeResult{SessionPath: path}, nil
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

func hugeBackedResumeString(value string) string {
	backing := strings.Repeat("x", 1<<20) + value
	return backing[len(backing)-len(value):]
}

func assertDetachedResumeString(t *testing.T, name, source, got string) {
	t.Helper()
	if got != source {
		t.Fatalf("%s = %q, want %q", name, got, source)
	}
	if source != "" && unsafe.StringData(source) == unsafe.StringData(got) {
		t.Fatalf("%s retained source backing storage", name)
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
