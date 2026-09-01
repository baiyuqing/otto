package repl

import (
	"bytes"
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
)

// fakeSwitchBackend adds the app.ProfileSwitcher capability to fakeBackend so
// /model tests can drive profile listing and switching offline.
type fakeSwitchBackend struct {
	fakeBackend
	profiles        []string
	switchProfile   func(context.Context, string) (app.ResumeResult, error)
	switchCalls     []string
	setDefaultCalls []string
}

func (f *fakeSwitchBackend) Profiles() []string { return f.profiles }

func (f *fakeSwitchBackend) SwitchProfile(ctx context.Context, name string) (app.ResumeResult, error) {
	f.switchCalls = append(f.switchCalls, name)
	if f.switchProfile == nil {
		return app.ResumeResult{}, nil
	}
	return f.switchProfile(ctx, name)
}

func (f *fakeSwitchBackend) SetDefaultProfile(_ context.Context, name string) error {
	f.setDefaultCalls = append(f.setDefaultCalls, name)
	return nil
}

func TestREPLModelShowsCurrentAndProfiles(t *testing.T) {
	backend := &fakeSwitchBackend{
		fakeBackend: fakeBackend{info: app.Info{Profile: "default", Provider: "openai-compatible", Model: "gpt-4o"}},
		profiles:    []string{"default", "chatgpt"},
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/model\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	out := stdout.String()
	if !strings.Contains(out, "default") || !strings.Contains(out, "openai-compatible") || !strings.Contains(out, "gpt-4o") {
		t.Fatalf("current line missing fields: %q", out)
	}
	if !strings.Contains(out, "chatgpt") {
		t.Fatalf("profile list missing configured profiles: %q", out)
	}
	if len(backend.switchCalls) != 0 {
		t.Fatalf("bare /model must not switch: %v", backend.switchCalls)
	}
}

func TestREPLModelSwitchesProfile(t *testing.T) {
	backend := &fakeSwitchBackend{
		fakeBackend: fakeBackend{info: app.Info{Profile: "default", Provider: "openai-compatible", Model: "gpt-4o"}},
		profiles:    []string{"default", "chatgpt"},
	}
	backend.switchProfile = func(_ context.Context, name string) (app.ResumeResult, error) {
		backend.info = app.Info{Profile: name, Provider: "chatgpt", Model: "gpt-5", SessionID: "sess-2"}
		return app.ResumeResult{}, nil
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/model chatgpt\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(backend.switchCalls) != 1 || backend.switchCalls[0] != "chatgpt" {
		t.Fatalf("switch calls = %v", backend.switchCalls)
	}
	if len(backend.setDefaultCalls) != 1 || backend.setDefaultCalls[0] != "chatgpt" {
		t.Fatalf("set default calls = %v", backend.setDefaultCalls)
	}
	out := stdout.String()
	if !strings.Contains(out, "chatgpt") || !strings.Contains(out, "gpt-5") {
		t.Fatalf("switch output missing new profile fields: %q", out)
	}
}

func TestREPLModelSwitchErrorReported(t *testing.T) {
	backend := &fakeSwitchBackend{
		fakeBackend: fakeBackend{info: app.Info{Profile: "default", Provider: "openai-compatible", Model: "gpt-4o"}},
		profiles:    []string{"default"},
	}
	wantErr := errors.New(`profile "missing" not found`)
	backend.switchProfile = func(context.Context, string) (app.ResumeResult, error) { return app.ResumeResult{}, wantErr }
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/model missing\n"), &stdout, &stderr, backend)
	err := r.Run(context.Background())
	if err == nil || !strings.Contains(err.Error(), "not found") {
		t.Fatalf("Run() error = %v, want not found error", err)
	}
	if !IsCommandError(err, "/model") {
		t.Fatalf("Run() error = %v, want /model command error", err)
	}
}

func TestREPLModelUnavailableWithoutSwitcher(t *testing.T) {
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/model\n"), &stdout, &stderr, &fakeBackend{})
	err := r.Run(context.Background())
	if !errors.Is(err, app.ErrProfileSwitchUnavailable) {
		t.Fatalf("Run() error = %v, want ErrProfileSwitchUnavailable", err)
	}
	if !IsCommandError(err, "/model") {
		t.Fatalf("Run() error = %v, want /model command error", err)
	}
}

func TestREPLHelpListsModelCommand(t *testing.T) {
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/help\n/exit\n"), &stdout, &stderr, &fakeBackend{})
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "/model") {
		t.Fatalf("help = %q", stdout.String())
	}
}
