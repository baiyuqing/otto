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
	"github.com/baiyuqing/otto/internal/config"
)

// chatgptModel builds a test Model whose provider is chatgpt, the provider the
// OAuth login flow targets.
func chatgptModel(t *testing.T) Model {
	t.Helper()
	return newTestModelWithBackend(t, &fakeBackend{info: app.Info{Provider: config.ProviderChatGPT}})
}

func withTUIAuthSeams(t *testing.T, login func(context.Context, func(string) error) (auth.Credentials, error)) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "chatgpt.json")
	prevPath, prevLogin := authPathFn, authLoginFn
	authPathFn = func() (string, error) { return path, nil }
	if login != nil {
		authLoginFn = login
	}
	t.Cleanup(func() { authPathFn, authLoginFn = prevPath, prevLogin })
	return path
}

func submitCommand(t *testing.T, m Model, value string) (Model, tea.Cmd) {
	t.Helper()
	m.editor.SetValue(value)
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	return updated.(Model), cmd
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
	path := withTUIAuthSeams(t, nil)
	if err := (auth.Credentials{AccountID: "acct-42", Expiry: time.Date(2030, 5, 6, 7, 8, 9, 0, time.UTC)}).Save(path); err != nil {
		t.Fatal(err)
	}
	m := resizeModel(t, newTestModel(t), 80, 20)
	got, cmd := submitCommand(t, m, "/login status")
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil", cmd)
	}
	if content := got.View().Content; strings.Contains(content, "acct-42") || !strings.Contains(content, "Signed in to ChatGPT") {
		t.Fatalf("view = %q", content)
	}
}

func TestLoginStatusReportsNotSignedIn(t *testing.T) {
	withTUIAuthSeams(t, nil)
	m := resizeModel(t, newTestModel(t), 80, 20)
	got, _ := submitCommand(t, m, "/login status")
	if !strings.Contains(got.View().Content, "Not signed in") {
		t.Fatalf("view = %q", got.View().Content)
	}
}

func TestLoginCommandSavesCredentialsAndReports(t *testing.T) {
	path := withTUIAuthSeams(t, func(_ context.Context, open func(string) error) (auth.Credentials, error) {
		_ = open("https://auth.example/authorize?x=1")
		return auth.Credentials{AccessToken: "secret-token", AccountID: "acct-7"}, nil
	})
	m := resizeModel(t, chatgptModel(t), 80, 24)
	pending, cmd := submitCommand(t, m, "/login")
	if cmd == nil {
		t.Fatal("/login cmd = nil")
	}
	if !pending.loginPending {
		t.Fatal("loginPending = false, want true")
	}

	// First message: the authorization URL notice.
	urlMsg := runCommandWithin(t, cmd, time.Second)
	updated, cmd := pending.Update(urlMsg)
	pending = updated.(Model)
	if cmd == nil {
		t.Fatal("expected a follow-up wait command after URL notice")
	}
	if !strings.Contains(pending.View().Content, "auth.example") {
		t.Fatalf("view missing URL: %q", pending.View().Content)
	}

	// Second message: the final result.
	doneMsg := runCommandWithin(t, cmd, time.Second)
	updated, _ = pending.Update(doneMsg)
	got := updated.(Model)
	if got.loginPending {
		t.Fatal("loginPending = true after completion")
	}
	content := got.View().Content
	if strings.Contains(content, "acct-7") || !strings.Contains(content, "Signed in to ChatGPT") || !strings.Contains(content, "Start a new session") {
		t.Fatalf("view = %q", content)
	}
	if strings.Contains(content, "secret-token") {
		t.Fatalf("view leaked the access token: %q", content)
	}
	creds, err := auth.Load(path)
	if err != nil {
		t.Fatalf("credentials not saved: %v", err)
	}
	if creds.AccountID != "acct-7" {
		t.Fatalf("saved account = %q", creds.AccountID)
	}
}

func TestLoginNonChatGPTProviderExplainsAPIKey(t *testing.T) {
	called := false
	withTUIAuthSeams(t, func(context.Context, func(string) error) (auth.Credentials, error) {
		called = true
		return auth.Credentials{}, nil
	})
	backend := &fakeBackend{info: app.Info{Provider: config.ProviderOpenAICompatible}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 24)
	got, cmd := submitCommand(t, m, "/login")
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil (no OAuth for non-chatgpt provider)", cmd)
	}
	if got.loginPending {
		t.Fatal("loginPending = true, want false")
	}
	if called {
		t.Fatal("login ran the OAuth flow for a non-chatgpt provider")
	}
	if !strings.Contains(got.View().Content, "API key") {
		t.Fatalf("view = %q", got.View().Content)
	}
}

func TestLogoutCommandRemovesCredentials(t *testing.T) {
	path := withTUIAuthSeams(t, nil)
	if err := (auth.Credentials{AccountID: "acct-1"}).Save(path); err != nil {
		t.Fatal(err)
	}
	m := resizeModel(t, newTestModel(t), 80, 20)
	got, _ := submitCommand(t, m, "/logout")
	if !strings.Contains(got.View().Content, "Signed out") {
		t.Fatalf("view = %q", got.View().Content)
	}
	if _, err := auth.Load(path); err != auth.ErrNoCredentials {
		t.Fatalf("credentials still present: %v", err)
	}
}

func TestLoginCommandFailureIsBounded(t *testing.T) {
	withTUIAuthSeams(t, func(context.Context, func(string) error) (auth.Credentials, error) {
		return auth.Credentials{}, errors.New("tui-login-secret")
	})
	m := resizeModel(t, chatgptModel(t), 80, 24)
	pending, cmd := submitCommand(t, m, "/login")
	if cmd == nil {
		t.Fatal("/login cmd = nil")
	}
	updated, _ := pending.Update(runCommandWithin(t, cmd, time.Second))
	got := updated.(Model)
	if strings.Contains(got.View().Content, "tui-login-secret") {
		t.Fatalf("view leaked login error: %q", got.View().Content)
	}
}

func TestLogoutCommandWhenNotSignedIn(t *testing.T) {
	withTUIAuthSeams(t, nil)
	m := resizeModel(t, newTestModel(t), 80, 20)
	got, _ := submitCommand(t, m, "/logout")
	if !strings.Contains(got.View().Content, "Not signed in") {
		t.Fatalf("view = %q", got.View().Content)
	}
}
