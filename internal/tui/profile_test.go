package tui

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/app"
)

// switchBackend adds the app.ProfileSwitcher capability to fakeBackend.
type switchBackend struct {
	fakeBackend
	profiles        []string
	switchProfile   func(context.Context, string) (app.ResumeResult, error)
	switchCalls     []string
	setDefaultCalls []string
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
	return nil
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

func TestModelCommandUnavailableWithoutSwitcher(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 24)
	got, cmd := submitCommand(t, m, "/model")
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil", cmd)
	}
	if !strings.Contains(got.statusText, app.ErrProfileSwitchUnavailable.Error()) {
		t.Fatalf("statusText = %q", got.statusText)
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
