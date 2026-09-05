package tui

import (
	"context"
	"errors"
	"path/filepath"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/auth"
)

type authTUIBackend struct {
	*fakeBackend
	authentication app.Authentication
	dynamic        bool
}

type unsupportedTUIAuthentication struct{}

func (unsupportedTUIAuthentication) Login(context.Context, func(string) error) error {
	return app.ErrAuthenticationUnsupported
}
func (unsupportedTUIAuthentication) Logout(context.Context) (bool, error)  { return false, nil }
func (unsupportedTUIAuthentication) Status(context.Context) (string, bool) { return "", false }

func (b *authTUIBackend) Login(ctx context.Context, open func(string) error) error {
	return b.authentication.Login(ctx, open)
}

func (b *authTUIBackend) Logout(ctx context.Context) (bool, error) {
	return b.authentication.Logout(ctx)
}

func (b *authTUIBackend) Status(ctx context.Context) (string, bool) {
	return b.authentication.Status(ctx)
}

func newAuthTUIBackend(t *testing.T, provider string, login func(context.Context, func(string) error) (auth.Credentials, error)) (*authTUIBackend, string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "chatgpt.json")
	service := auth.NewService(path, login)
	return &authTUIBackend{fakeBackend: &fakeBackend{info: app.Info{Provider: provider}}, authentication: service, dynamic: true}, path
}

func (b *authTUIBackend) DynamicContentAvailable() bool { return b.dynamic }

func submitLoginCommand(t *testing.T, m Model, value string) (Model, tea.Cmd) {
	t.Helper()
	m.editor.SetValue(value)
	updated, cmd := m.dispatch(keyPress(tea.KeyEnter))
	return updated.(Model), cmd
}

func submitCommand(t *testing.T, m Model, value string) (Model, tea.Cmd) {
	return submitLoginCommand(t, m, value)
}

func newAuthModel(t *testing.T, backend app.Backend) Model {
	t.Helper()
	return resizeModel(t, NewModel(context.Background(), backend, WithRenderer(rendererFunc(func(text string, _ int) (string, error) { return text, nil }))), 80, 24)
}

func TestLoginCommandRegistryCompletionAndHelp(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 20)
	m = typeEditorText(t, m, "/log")
	names := map[string]bool{}
	for _, s := range m.commandSuggestions() {
		names[s.Name] = true
	}
	if !names["/login"] || !names["/logout"] {
		t.Fatalf("/log suggestions = %#v", m.commandSuggestions())
	}

	m = resizeModel(t, newTestModel(t), 80, 24)
	updated, _ := m.Update(showHelpOverlayMsg{})
	content := updated.(Model).View().Content
	if !strings.Contains(content, "/login") || !strings.Contains(content, "/logout") {
		t.Fatalf("help overlay = %q", content)
	}
}

func TestLoginStatusCommandRendersStatus(t *testing.T) {
	backend, path := newAuthTUIBackend(t, "openai-compatible", nil)
	if err := (auth.Credentials{AccountID: "acct-42", Expiry: time.Date(2030, 5, 6, 7, 8, 9, 0, time.UTC)}).Save(path); err != nil {
		t.Fatal(err)
	}
	got, cmd := submitLoginCommand(t, newAuthModel(t, backend), "/login status")
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil", cmd)
	}
	content := strings.Join(got.pendingPrints, "\n")
	if strings.Contains(content, "acct-42") || !strings.Contains(content, "Signed in to ChatGPT") {
		t.Fatalf("committed transcript = %q", content)
	}
}

func TestLoginStatusReportsNotSignedIn(t *testing.T) {
	backend, _ := newAuthTUIBackend(t, "openai-compatible", nil)
	got, _ := submitLoginCommand(t, newAuthModel(t, backend), "/login status")
	if content := strings.Join(got.pendingPrints, "\n"); !strings.Contains(content, "Not signed in") {
		t.Fatalf("committed transcript = %q", content)
	}
}

func TestLoginCommandSavesCredentialsAndReports(t *testing.T) {
	backend, path := newAuthTUIBackend(t, "chatgpt", func(_ context.Context, open func(string) error) (auth.Credentials, error) {
		_ = open("https://auth.example/authorize?x=1")
		return auth.Credentials{AccessToken: "secret-token", AccountID: "acct-7"}, nil
	})
	browserLaunches := 0
	previous := tuiOpenBrowser
	tuiOpenBrowser = func(string) { browserLaunches++ }
	t.Cleanup(func() { tuiOpenBrowser = previous })
	pending, cmd := submitLoginCommand(t, newAuthModel(t, backend), "/login")
	if cmd == nil || !pending.loginPending {
		t.Fatalf("login command = %v pending=%v", cmd, pending.loginPending)
	}
	urlMsg := runCommandWithin(t, cmd, time.Second)
	updated, cmd := pending.dispatch(urlMsg)
	pending = updated.(Model)
	if cmd == nil || !strings.Contains(strings.Join(pending.pendingPrints, "\n"), "auth.example") {
		t.Fatalf("url result cmd=%v transcript=%q", cmd, strings.Join(pending.pendingPrints, "\n"))
	}
	doneMsg := runCommandWithin(t, cmd, time.Second)
	updated, _ = pending.dispatch(doneMsg)
	got := updated.(Model)
	content := strings.Join(got.pendingPrints, "\n")
	if got.loginPending || strings.Contains(content, "acct-7") || !strings.Contains(content, "Restart Otto") || strings.Contains(content, "secret-token") {
		t.Fatalf("pending=%v transcript=%q", got.loginPending, content)
	}
	creds, err := auth.Load(path)
	if err != nil || creds.AccountID != "acct-7" {
		t.Fatalf("saved credentials = %#v, %v", creds, err)
	}
	if browserLaunches != 1 {
		t.Fatalf("browser launch seam calls = %d, want 1", browserLaunches)
	}
}

func TestLoginNonChatGPTProviderExplainsAPIKey(t *testing.T) {
	backend, _ := newAuthTUIBackend(t, "openai-compatible", nil)
	backend.authentication = unsupportedTUIAuthentication{}
	got, cmd := submitLoginCommand(t, newAuthModel(t, backend), "/login")
	if cmd == nil || !got.loginPending {
		t.Fatalf("cmd=%v pending=%v", cmd, got.loginPending)
	}
	updated, _ := got.dispatch(runCommandWithin(t, cmd, time.Second))
	got = updated.(Model)
	if !strings.Contains(strings.Join(got.pendingPrints, "\n"), "API key") {
		t.Fatalf("transcript=%q", strings.Join(got.pendingPrints, "\n"))
	}
}

func TestLogoutCommandRemovesCredentials(t *testing.T) {
	backend, path := newAuthTUIBackend(t, "openai-compatible", nil)
	if err := (auth.Credentials{AccountID: "acct-1"}).Save(path); err != nil {
		t.Fatal(err)
	}
	got, _ := submitLoginCommand(t, newAuthModel(t, backend), "/logout")
	if !strings.Contains(strings.Join(got.pendingPrints, "\n"), "Signed out") {
		t.Fatalf("transcript = %q", strings.Join(got.pendingPrints, "\n"))
	}
	if _, err := auth.Load(path); err != auth.ErrNoCredentials {
		t.Fatalf("credentials still present: %v", err)
	}
}

func TestLoginCommandFailureIsBounded(t *testing.T) {
	backend, _ := newAuthTUIBackend(t, "chatgpt", func(context.Context, func(string) error) (auth.Credentials, error) {
		return auth.Credentials{}, errors.New("tui-login-secret")
	})
	pending, cmd := submitLoginCommand(t, newAuthModel(t, backend), "/login")
	updated, _ := pending.Update(runCommandWithin(t, cmd, time.Second))
	if strings.Contains(updated.(Model).View().Content, "tui-login-secret") {
		t.Fatalf("view leaked login error: %q", updated.(Model).View().Content)
	}
}

func TestLogoutCommandWhenNotSignedIn(t *testing.T) {
	backend, _ := newAuthTUIBackend(t, "openai-compatible", nil)
	got, _ := submitLoginCommand(t, newAuthModel(t, backend), "/logout")
	if !strings.Contains(strings.Join(got.pendingPrints, "\n"), "Not signed in") {
		t.Fatalf("transcript = %q", strings.Join(got.pendingPrints, "\n"))
	}
}

func TestLoginCommandsUnavailableWhenDynamicContentIsSuppressed(t *testing.T) {
	loginCalls := 0
	backend, path := newAuthTUIBackend(t, "chatgpt", func(context.Context, func(string) error) (auth.Credentials, error) {
		loginCalls++
		return auth.Credentials{}, nil
	})
	backend.dynamic = false
	if err := (auth.Credentials{AccountID: "acct-1"}).Save(path); err != nil {
		t.Fatal(err)
	}
	m := newAuthModel(t, backend)
	for _, command := range []string{"/login status", "/logout", "/login"} {
		updated, cmd := submitLoginCommand(t, m, command)
		if cmd != nil {
			t.Fatalf("%s scheduled cmd %v", command, cmd)
		}
		m = updated
		if !strings.Contains(strings.Join(m.pendingPrints, "\n"), app.ErrAuthenticationUnavailable.Error()) {
			t.Fatalf("%s transcript=%q", command, strings.Join(m.pendingPrints, "\n"))
		}
	}
	if loginCalls != 0 {
		t.Fatalf("login callback calls = %d, want 0", loginCalls)
	}
}
