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

// withAuthSeams points the in-process auth commands at a temp file through the
// captured startup context and optionally overrides the OAuth entry point.
func withAuthSeams(t *testing.T, login func(context.Context, func(string) error) (auth.Credentials, error)) (context.Context, string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "chatgpt.json")
	prevLogin := replAuthLogin
	prevOpenBrowser := replOpenBrowser
	if login != nil {
		replAuthLogin = login
	}
	replOpenBrowser = func(string) {}
	t.Cleanup(func() {
		replAuthLogin = prevLogin
		replOpenBrowser = prevOpenBrowser
	})
	return auth.ContextWithPath(context.Background(), path), path
}

func TestREPLLoginStatusReportsNotSignedIn(t *testing.T) {
	ctx, _ := withAuthSeams(t, nil)
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login status\n/exit\n"), &stdout, &stderr, &fakeBackend{})
	if err := r.Run(ctx); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "Not signed in") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLLoginStatusReportsSignedIn(t *testing.T) {
	ctx, path := withAuthSeams(t, nil)
	if err := (auth.Credentials{AccountID: "acct-9"}).Save(path); err != nil {
		t.Fatal(err)
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login status\n/exit\n"), &stdout, &stderr, &fakeBackend{})
	if err := r.Run(ctx); err != nil {
		t.Fatal(err)
	}
	if got := stdout.String(); strings.Contains(got, "acct-9") || !strings.Contains(got, "Signed in to ChatGPT") {
		t.Fatalf("stdout = %q", got)
	}
}

func TestREPLLoginSavesCredentials(t *testing.T) {
	// A missing PATH entry makes any accidental direct exec.Command("open", ...)
	// fail closed instead of launching the host browser during this test.
	t.Setenv("PATH", t.TempDir())
	ctx, path := withAuthSeams(t, func(_ context.Context, open func(string) error) (auth.Credentials, error) {
		_ = open("https://auth.example/authorize?x=1")
		return auth.Credentials{AccessToken: "secret-token", AccountID: "acct-7"}, nil
	})
	browserLaunches := 0
	replOpenBrowser = func(string) { browserLaunches++ }
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login\n/exit\n"), &stdout, &stderr, chatgptBackend())
	if err := r.Run(ctx); err != nil {
		t.Fatal(err)
	}
	out := stdout.String()
	if strings.Contains(out, "acct-7") || !strings.Contains(out, "Signed in to ChatGPT") || !strings.Contains(out, "Restart Otto") {
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
	if browserLaunches != 1 {
		t.Fatalf("browser launch seam calls = %d, want 1", browserLaunches)
	}
}

func TestREPLLoginNonChatGPTProviderExplainsAPIKey(t *testing.T) {
	called := false
	ctx, _ := withAuthSeams(t, func(context.Context, func(string) error) (auth.Credentials, error) {
		called = true
		return auth.Credentials{}, nil
	})
	backend := &fakeBackend{info: app.Info{Provider: config.ProviderOpenAICompatible}}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(ctx); err != nil {
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
	ctx, path := withAuthSeams(t, nil)
	if err := (auth.Credentials{AccountID: "acct-1"}).Save(path); err != nil {
		t.Fatal(err)
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/logout\n/exit\n"), &stdout, &stderr, &fakeBackend{})
	if err := r.Run(ctx); err != nil {
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
	ctx, _ := withAuthSeams(t, nil)
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/logout\n/exit\n"), &stdout, &stderr, &fakeBackend{})
	if err := r.Run(ctx); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "Not signed in") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLLoginFailureIsBounded(t *testing.T) {
	ctx, _ := withAuthSeams(t, func(context.Context, func(string) error) (auth.Credentials, error) {
		return auth.Credentials{}, errors.New("repl-login-secret")
	})
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login\n/exit\n"), &stdout, &stderr, chatgptBackend())
	if err := r.Run(ctx); err == nil {
		t.Fatal("Run() unexpectedly succeeded")
	}
	if strings.Contains(stderr.String(), "repl-login-secret") {
		t.Fatalf("stderr leaked login error: %q", stderr.String())
	}
}

func TestREPLLoginCommandsUnavailableWhenDynamicContentIsSuppressed(t *testing.T) {
	loginCalls := 0
	ctx, path := withAuthSeams(t, func(context.Context, func(string) error) (auth.Credentials, error) {
		loginCalls++
		return auth.Credentials{}, nil
	})
	if err := (auth.Credentials{AccountID: "acct-1"}).Save(path); err != nil {
		t.Fatal(err)
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login status\n/logout\n/login\n/exit\n"), &stdout, &stderr, &dynamicBackend{fakeBackend: fakeBackend{info: app.Info{Provider: config.ProviderChatGPT}}, dynamic: false})
	if err := r.Run(ctx); err != nil {
		t.Fatal(err)
	}
	if got := stdout.String(); strings.Count(got, auth.ErrInteractiveUnavailable.Error()) != 3 {
		t.Fatalf("stdout = %q, want three unavailable notices", got)
	}
	if loginCalls != 0 {
		t.Fatalf("login callback calls = %d, want 0", loginCalls)
	}
	if _, err := auth.Load(path); err != nil {
		t.Fatalf("suppressed commands mutated credentials: %v", err)
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

type dynamicBackend struct {
	fakeBackend
	dynamic bool
}

func (b *dynamicBackend) DynamicContentAvailable() bool { return b.dynamic }
