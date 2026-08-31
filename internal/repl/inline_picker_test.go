package repl

import (
	"context"
	"errors"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/session"
)

type fakeBrowserBackend struct {
	fakeBackend
	sessions  []session.SessionInfo
	listErr   error
	resumed   string
	resumeErr error
}

func (f *fakeBrowserBackend) ListSessions(_ context.Context, _ int) (session.ListResult, error) {
	if f.listErr != nil {
		return session.ListResult{}, f.listErr
	}
	return session.ListResult{Sessions: f.sessions}, nil
}

func (f *fakeBrowserBackend) ResumeSession(_ context.Context, path string) (app.ResumeResult, error) {
	f.resumed = path
	if f.resumeErr != nil {
		return app.ResumeResult{}, f.resumeErr
	}
	return app.ResumeResult{SessionPath: path}, nil
}

func TestPickerResumeShowsSessions(t *testing.T) {
	backend := &fakeBrowserBackend{
		sessions: []session.SessionInfo{
			{Path: "/a.jsonl", ID: "s1", Name: "first", Modified: time.Now()},
			{Path: "/b.jsonl", ID: "s2", Name: "second", Modified: time.Now()},
		},
	}
	m := newInlineModel(context.Background(), backend)
	m.width = 80

	typeText(&m, "/resume")
	m, cmd := updateInline(m, keyEnter())
	if cmd == nil {
		t.Fatal("expected cmd to start list fetch")
	}
	if !m.picker.active() {
		t.Fatal("picker should be active")
	}

	// Simulate the list result arriving
	m, _ = updateInline(m, pickerListMsg{
		result: session.ListResult{Sessions: backend.sessions},
	})
	if m.picker.phase != pickerReady {
		t.Fatalf("phase = %d, want pickerReady", m.picker.phase)
	}
	if len(m.picker.sessions) != 2 {
		t.Fatalf("sessions = %d, want 2", len(m.picker.sessions))
	}
}

func TestPickerNavigateUpDown(t *testing.T) {
	m := pickerReadyModel(t, 3)

	if m.picker.selected != 0 {
		t.Fatalf("selected = %d, want 0", m.picker.selected)
	}

	m, _, _ = m.handlePickerKey(keyDown())
	if m.picker.selected != 1 {
		t.Fatalf("selected = %d, want 1", m.picker.selected)
	}

	m, _, _ = m.handlePickerKey(keyUp())
	if m.picker.selected != 0 {
		t.Fatalf("selected = %d, want 0", m.picker.selected)
	}
}

func TestPickerUpAtTopIsNoop(t *testing.T) {
	m := pickerReadyModel(t, 3)
	m, _, _ = m.handlePickerKey(keyUp())
	if m.picker.selected != 0 {
		t.Fatalf("selected = %d, want 0", m.picker.selected)
	}
}

func TestPickerDownAtBottomIsNoop(t *testing.T) {
	m := pickerReadyModel(t, 2)
	m, _, _ = m.handlePickerKey(keyDown())
	m, _, _ = m.handlePickerKey(keyDown())
	if m.picker.selected != 1 {
		t.Fatalf("selected = %d, want 1", m.picker.selected)
	}
}

func TestPickerEscapeCloses(t *testing.T) {
	m := pickerReadyModel(t, 2)
	m, _, handled := m.handlePickerKey(tea.KeyPressMsg{Code: tea.KeyEscape})
	if !handled {
		t.Fatal("escape not handled")
	}
	if m.picker.active() {
		t.Fatal("picker should be closed")
	}
}

func TestPickerEnterResumesSession(t *testing.T) {
	backend := &fakeBrowserBackend{
		sessions: []session.SessionInfo{
			{Path: "/a.jsonl", ID: "s1", Name: "first"},
			{Path: "/b.jsonl", ID: "s2", Name: "second"},
		},
	}
	m := newInlineModel(context.Background(), backend)
	m.width = 80
	m.picker = &inlinePicker{
		action:   pickerResume,
		phase:    pickerReady,
		sessions: backend.sessions,
		selected: 1,
	}

	m, cmd, handled := m.handlePickerKey(keyEnter())
	if !handled {
		t.Fatal("enter not handled")
	}
	if m.picker.phase != pickerExecuting {
		t.Fatalf("phase = %d, want pickerExecuting", m.picker.phase)
	}
	if cmd == nil {
		t.Fatal("expected cmd")
	}

	// Execute the cmd to get the result
	msg := cmd()
	done, ok := msg.(pickerDoneMsg)
	if !ok {
		t.Fatalf("unexpected msg type %T", msg)
	}
	if done.err != nil {
		t.Fatalf("unexpected error: %v", done.err)
	}
	if backend.resumed != "/b.jsonl" {
		t.Fatalf("resumed = %q, want /b.jsonl", backend.resumed)
	}
}

func TestPickerResumeCurrentSessionShowsMessage(t *testing.T) {
	backend := &fakeBrowserBackend{
		sessions: []session.SessionInfo{
			{Path: "/a.jsonl", ID: "s1", Current: true},
		},
	}
	m := newInlineModel(context.Background(), backend)
	m.width = 80
	m.picker = &inlinePicker{
		action:   pickerResume,
		phase:    pickerReady,
		sessions: backend.sessions,
	}

	m, cmd, _ := m.handlePickerKey(keyEnter())
	if m.picker.active() {
		t.Fatal("picker should close on current session select")
	}
	if cmd == nil {
		t.Fatal("expected println cmd")
	}
}

func TestPickerListErrorClosesPicker(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBrowserBackend{})
	m.width = 80
	m.picker = &inlinePicker{
		action: pickerResume,
		phase:  pickerLoading,
	}

	m, cmd := updateInline(m, pickerListMsg{err: errors.New("disk error")})
	if m.picker.active() {
		t.Fatal("picker should close on error")
	}
	if cmd == nil {
		t.Fatal("expected error message cmd")
	}
}

func TestPickerEmptyListClosesPicker(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBrowserBackend{})
	m.width = 80
	m.picker = &inlinePicker{
		action: pickerResume,
		phase:  pickerLoading,
	}

	m, cmd := updateInline(m, pickerListMsg{result: session.ListResult{}})
	if m.picker.active() {
		t.Fatal("picker should close on empty list")
	}
	if cmd == nil {
		t.Fatal("expected message cmd")
	}
}

func TestPickerViewShowsPickerWhenActive(t *testing.T) {
	m := pickerReadyModel(t, 2)
	view := m.View()
	if view.Content == "" {
		t.Fatal("view should show picker content")
	}
}

func TestPickerNoPersistenceError(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80
	typeText(&m, "/resume")
	m, cmd := updateInline(m, keyEnter())
	if cmd == nil {
		t.Fatal("expected error message cmd")
	}
	if m.picker.active() {
		t.Fatal("picker should not activate without SessionBrowser")
	}
}

func pickerReadyModel(t *testing.T, n int) inlineModel {
	t.Helper()
	sessions := make([]session.SessionInfo, n)
	for i := range sessions {
		sessions[i] = session.SessionInfo{
			Path: "/s.jsonl",
			ID:   "s",
			Name: "session",
		}
	}
	backend := &fakeBrowserBackend{sessions: sessions}
	m := newInlineModel(context.Background(), backend)
	m.width = 80
	m.picker = &inlinePicker{
		action:   pickerResume,
		phase:    pickerReady,
		sessions: sessions,
	}
	return m
}
