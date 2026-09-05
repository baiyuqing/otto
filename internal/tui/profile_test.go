package tui

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
)

// switchBackend adds the app.ProfileSwitcher capability to fakeBackend.
type switchBackend struct {
	fakeBackend
	profiles        []string
	switchProfile   func(context.Context, string) (app.ResumeResult, error)
	switchCalls     []string
	setDefaultCalls []string
	setDefaultErr   error
}

type suppressedModelBackend struct {
	*switchBackend
	infoCalls     int
	profilesCalls int
}

func (f *switchBackend) Profiles() []string { return f.profiles }

func (f *switchBackend) SwitchProfile(ctx context.Context, name string) (app.ResumeResult, error) {
	f.switchCalls = append(f.switchCalls, name)
	if f.switchProfile == nil {
		return app.ResumeResult{}, nil
	}
	return f.switchProfile(ctx, name)
}

func (f *switchBackend) SetDefaultProfile(_ context.Context, name string) error {
	f.setDefaultCalls = append(f.setDefaultCalls, name)
	return f.setDefaultErr
}

func (s *suppressedModelBackend) DynamicContentAvailable() bool { return false }

func (s *suppressedModelBackend) Info() app.Info {
	s.infoCalls++
	return s.switchBackend.Info()
}

func (s *suppressedModelBackend) Profiles() []string {
	s.profilesCalls++
	return s.switchBackend.Profiles()
}

func TestModelCommandShowsCurrentAndProfiles(t *testing.T) {
	backend := &switchBackend{
		fakeBackend: fakeBackend{info: app.Info{Profile: "default", Provider: "openai-compatible", Model: "gpt-4o"}},
		profiles:    []string{"default", "chatgpt"},
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 24)
	got, cmd := submitCommand(t, m, "/model")
	if cmd != nil {
		t.Fatalf("bare /model cmd = %v, want nil", cmd)
	}
	content := got.View().Content
	for _, want := range []string{"default", "openai-compatible", "gpt-4o", "chatgpt"} {
		if !strings.Contains(content, want) {
			t.Fatalf("view = %q, want %q", content, want)
		}
	}
	if len(backend.switchCalls) != 0 {
		t.Fatalf("bare /model must not switch: %v", backend.switchCalls)
	}
}

func TestModelCommandSwitchesProfile(t *testing.T) {
	backend := &switchBackend{
		fakeBackend: fakeBackend{info: app.Info{Profile: "default", Provider: "openai-compatible", Model: "gpt-4o"}},
		profiles:    []string{"default", "chatgpt"},
	}
	backend.switchProfile = func(_ context.Context, name string) (app.ResumeResult, error) {
		backend.info = app.Info{Profile: name, Provider: "chatgpt", Model: "gpt-5", SessionID: "sess-2"}
		return app.ResumeResult{}, nil
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 24)
	pending, cmd := submitCommand(t, m, "/model chatgpt")
	if cmd == nil {
		t.Fatal("/model <profile> cmd = nil, want async switch")
	}
	if !pending.profileSwitchPending {
		t.Fatal("profileSwitchPending = false, want true")
	}
	msg := runCommandWithin(t, cmd, time.Second)
	updated, _ := pending.Update(msg)
	got := updated.(Model)
	if got.profileSwitchPending {
		t.Fatal("profileSwitchPending = true after completion")
	}
	if len(backend.switchCalls) != 1 || backend.switchCalls[0] != "chatgpt" {
		t.Fatalf("switch calls = %v", backend.switchCalls)
	}
	if len(backend.setDefaultCalls) != 1 || backend.setDefaultCalls[0] != "chatgpt" {
		t.Fatalf("set default calls = %v", backend.setDefaultCalls)
	}
	if !strings.Contains(got.statusText, "chatgpt") || !strings.Contains(got.statusText, "gpt-5") || !strings.Contains(got.statusText, "default") {
		t.Fatalf("statusText = %q, want new profile fields", got.statusText)
	}
}

func TestModelCommandSwitchErrorReported(t *testing.T) {
	backend := &switchBackend{
		fakeBackend: fakeBackend{info: app.Info{Profile: "default", Provider: "openai-compatible", Model: "gpt-4o"}},
		profiles:    []string{"default"},
	}
	backend.switchProfile = func(context.Context, string) (app.ResumeResult, error) {
		return app.ResumeResult{}, errors.New(`profile "missing" not found`)
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 24)
	pending, cmd := submitCommand(t, m, "/model missing")
	if cmd == nil {
		t.Fatal("cmd = nil")
	}
	msg := runCommandWithin(t, cmd, time.Second)
	updated, _ := pending.Update(msg)
	got := updated.(Model)
	if got.profileSwitchPending {
		t.Fatal("profileSwitchPending = true after error")
	}
	if !strings.Contains(got.statusText, "not found") {
		t.Fatalf("statusText = %q, want not found error", got.statusText)
	}
}

func TestModelCommandReportsDefaultSaveFailureAfterSwitch(t *testing.T) {
	backend := &switchBackend{
		fakeBackend:   fakeBackend{info: app.Info{Profile: "default", Provider: "openai-compatible", Model: "gpt-4o"}},
		profiles:      []string{"default", "chatgpt"},
		setDefaultErr: errors.New("default save failed"),
	}
	backend.switchProfile = func(_ context.Context, name string) (app.ResumeResult, error) {
		backend.info = app.Info{Profile: name, Provider: "chatgpt", Model: "gpt-5", SessionID: "sess-2"}
		return app.ResumeResult{}, nil
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 24)
	pending, cmd := submitCommand(t, m, "/model chatgpt")
	if cmd == nil {
		t.Fatal("cmd = nil")
	}
	updated, _ := pending.Update(runCommandWithin(t, cmd, time.Second))
	got := updated.(Model)
	if got.profileSwitchPending || !strings.Contains(got.statusText, "default profile was not saved") || !strings.Contains(got.statusText, "gpt-5") {
		t.Fatalf("pending=%v status=%q", got.profileSwitchPending, got.statusText)
	}
	if len(backend.setDefaultCalls) != 1 {
		t.Fatalf("set default calls = %v", backend.setDefaultCalls)
	}
}

func TestModelCommandSwitchNotesCanceledTasks(t *testing.T) {
	tasks := agent.NewTasks()
	addTask(t, tasks, agent.TaskRunning, "explorer")
	backend := &switchBackend{
		fakeBackend: fakeBackend{info: app.Info{Profile: "default", Provider: "openai-compatible", Model: "gpt-4o"}, tasks: tasks},
		profiles:    []string{"default", "chatgpt"},
	}
	backend.switchProfile = func(_ context.Context, name string) (app.ResumeResult, error) {
		backend.info = app.Info{Profile: name, Provider: "chatgpt", Model: "gpt-5"}
		return app.ResumeResult{}, nil
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 24)
	pending, cmd := submitCommand(t, m, "/model chatgpt")
	if cmd == nil {
		t.Fatal("/model <profile> cmd = nil, want async switch")
	}
	msg := runCommandWithin(t, cmd, time.Second)
	// dispatch (not Update): Update batches in the pending-print flush cmd,
	// which is unrelated to the assertion below.
	updated, resultCmd := pending.dispatch(msg)
	got := updated.(Model)
	if text := lastEntryText(t, got); text != "canceled 1 running tasks" {
		t.Fatalf("entry = %q", text)
	}
	if resultCmd != nil {
		t.Fatal("applyProfileSwitchResult() cmd = non-nil, want nil (dispatch's taskUpdateMsg case is the sole re-arm point)")
	}
}

func TestModelCommandUnavailableWithoutSwitcher(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 24)
	got, cmd := submitCommand(t, m, "/model")
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil", cmd)
	}
	if !strings.Contains(got.statusText, app.ErrProfileSwitchUnavailable.Error()) {
		t.Fatalf("statusText = %q", got.statusText)
	}
	if got.editor.Value() != "" {
		t.Fatalf("editor = %q, want cleared after unavailable /model", got.editor.Value())
	}
}

func TestModelCommandUnavailableWhenDynamicContentSuppressedWithoutBackendCallbacks(t *testing.T) {
	backend := &suppressedModelBackend{switchBackend: &switchBackend{
		fakeBackend: fakeBackend{info: app.Info{Profile: "default", Provider: "openai-compatible", Model: "gpt-4o"}},
		profiles:    []string{"default", "chatgpt"},
	}}
	m := newTestModelWithBackend(t, backend)
	backend.infoCalls = 0
	backend.profilesCalls = 0
	updated, cmd := m.handleModelCommand("")
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil", cmd)
	}
	got := updated.(Model)
	if got.statusText != app.ErrProfileSwitchUnavailable.Error() {
		t.Fatalf("statusText = %q, want %q", got.statusText, app.ErrProfileSwitchUnavailable.Error())
	}
	if backend.infoCalls != 0 || backend.profilesCalls != 0 || len(backend.switchCalls) != 0 || len(backend.setDefaultCalls) != 0 {
		t.Fatalf("backend callbacks = info %d profiles %d switch %v default %v, want none", backend.infoCalls, backend.profilesCalls, backend.switchCalls, backend.setDefaultCalls)
	}
}

func TestModelCommandRejectedWhileRunning(t *testing.T) {
	backend := &switchBackend{
		fakeBackend: fakeBackend{info: app.Info{Profile: "default", Provider: "openai-compatible", Model: "gpt-4o"}},
		profiles:    []string{"default", "chatgpt"},
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 24)
	m.running = true
	got, cmd := submitCommand(t, m, "/model chatgpt")
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil while running", cmd)
	}
	if len(backend.switchCalls) != 0 {
		t.Fatalf("switch attempted while running: %v", backend.switchCalls)
	}
	if !strings.Contains(got.statusText, app.ErrPromptActive.Error()) {
		t.Fatalf("statusText = %q", got.statusText)
	}
}

func TestModelCommandRegistryCompletionAndHelp(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 20)
	m = typeEditorText(t, m, "/mod")
	found := false
	for _, s := range m.commandSuggestions() {
		if s.Name == "/model" {
			found = true
		}
	}
	if !found {
		t.Fatalf("/mod suggestions = %#v", m.commandSuggestions())
	}

	m = resizeModel(t, newTestModel(t), 80, 24)
	updated, _ := m.Update(showHelpOverlayMsg{})
	if !strings.Contains(updated.(Model).View().Content, "/model") {
		t.Fatalf("help overlay = %q", updated.(Model).View().Content)
	}
}
