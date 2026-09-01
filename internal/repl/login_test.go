package repl

import (
	"bytes"
	"context"
	"errors"
	"path/filepath"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/auth"
	"github.com/baiyuqing/otto/internal/config"
)

// chatgptBackend is a fakeBackend whose provider is chatgpt, the provider the
// OAuth login flow targets.
func chatgptBackend() *fakeBackend {
	return &fakeBackend{info: app.Info{Provider: config.ProviderChatGPT}}
}

// withAuthSeams points the login/logout credential path at a temp file and
// optionally overrides the OAuth entry point, restoring both after the test.
func withAuthSeams(t *testing.T, login func(context.Context, func(string) error) (auth.Credentials, error)) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "chatgpt.json")
	prevPath, prevLogin := replAuthPath, replAuthLogin
	replAuthPath = func() (string, error) { return path, nil }
	if login != nil {
		replAuthLogin = login
	}
	t.Cleanup(func() { replAuthPath, replAuthLogin = prevPath, prevLogin })
	return path
}

func TestREPLLoginStatusReportsNotSignedIn(t *testing.T) {
	withAuthSeams(t, nil)
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login status\n/exit\n"), &stdout, &stderr, &fakeBackend{})
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "Not signed in") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLLoginStatusReportsSignedIn(t *testing.T) {
	path := withAuthSeams(t, nil)
	if err := (auth.Credentials{AccountID: "acct-9"}).Save(path); err != nil {
		t.Fatal(err)
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login status\n/exit\n"), &stdout, &stderr, &fakeBackend{})
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := stdout.String(); strings.Contains(got, "acct-9") || !strings.Contains(got, "Signed in to ChatGPT") {
		t.Fatalf("stdout = %q", got)
	}
}

func TestREPLLoginSavesCredentials(t *testing.T) {
	var openedURL string
	path := withAuthSeams(t, func(_ context.Context, open func(string) error) (auth.Credentials, error) {
		_ = open("https://auth.example/authorize?x=1")
		return auth.Credentials{AccessToken: "secret-token", AccountID: "acct-7"}, nil
	})
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login\n/exit\n"), &stdout, &stderr, chatgptBackend())
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	out := stdout.String()
	if strings.Contains(out, "acct-7") || !strings.Contains(out, "Signed in to ChatGPT") || !strings.Contains(out, "Start a new session") {
		t.Fatalf("stdout = %q", out)
	}
	if !strings.Contains(out, "auth.example") {
		t.Fatalf("login did not print the authorization URL: %q", out)
	}
	if strings.Contains(out, "secret-token") {
		t.Fatalf("stdout leaked the access token: %q", out)
	}
	creds, err := auth.Load(path)
	if err != nil {
		t.Fatalf("credentials not saved: %v", err)
	}
	if creds.AccountID != "acct-7" {
		t.Fatalf("saved account = %q", creds.AccountID)
	}
	_ = openedURL
}

func TestREPLLoginNonChatGPTProviderExplainsAPIKey(t *testing.T) {
	called := false
	withAuthSeams(t, func(context.Context, func(string) error) (auth.Credentials, error) {
		called = true
		return auth.Credentials{}, nil
	})
	backend := &fakeBackend{info: app.Info{Provider: config.ProviderOpenAICompatible}}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if called {
		t.Fatal("login ran the OAuth flow for a non-chatgpt provider")
	}
	if !strings.Contains(stdout.String(), "API key") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLLogoutRemovesCredentials(t *testing.T) {
	path := withAuthSeams(t, nil)
	if err := (auth.Credentials{AccountID: "acct-1"}).Save(path); err != nil {
		t.Fatal(err)
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/logout\n/exit\n"), &stdout, &stderr, &fakeBackend{})
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "Signed out") {
		t.Fatalf("stdout = %q", stdout.String())
	}
	if _, err := auth.Load(path); err != auth.ErrNoCredentials {
		t.Fatalf("credentials still present: %v", err)
	}
}

func TestREPLLogoutWhenNotSignedIn(t *testing.T) {
	withAuthSeams(t, nil)
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/logout\n/exit\n"), &stdout, &stderr, &fakeBackend{})
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "Not signed in") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLLoginFailureIsBounded(t *testing.T) {
	withAuthSeams(t, func(context.Context, func(string) error) (auth.Credentials, error) {
		return auth.Credentials{}, errors.New("repl-login-secret")
	})
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login\n/exit\n"), &stdout, &stderr, chatgptBackend())
	if err := r.Run(context.Background()); err == nil {
		t.Fatal("Run() unexpectedly succeeded")
	}
	if strings.Contains(stderr.String(), "repl-login-secret") {
		t.Fatalf("stderr leaked login error: %q", stderr.String())
	}
}

func TestREPLHelpListsLoginCommands(t *testing.T) {
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/help\n/exit\n"), &stdout, &stderr, &fakeBackend{})
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "/login") || !strings.Contains(stdout.String(), "/logout") {
		t.Fatalf("help = %q", stdout.String())
	}
}
