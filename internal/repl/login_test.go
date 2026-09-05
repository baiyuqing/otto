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
)

type authBackend struct {
	*fakeBackend
	authentication app.Authentication
	dynamic        bool
}

type unsupportedAuthentication struct{}

func (unsupportedAuthentication) Login(context.Context, func(string) error) error {
	return app.ErrAuthenticationUnsupported
}
func (unsupportedAuthentication) Logout(context.Context) (bool, error)  { return false, nil }
func (unsupportedAuthentication) Status(context.Context) (string, bool) { return "", false }

func (b *authBackend) Login(ctx context.Context, open func(string) error) error {
	return b.authentication.Login(ctx, open)
}

func (b *authBackend) Logout(ctx context.Context) (bool, error) {
	return b.authentication.Logout(ctx)
}

func (b *authBackend) Status(ctx context.Context) (string, bool) {
	return b.authentication.Status(ctx)
}

func newAuthBackend(t *testing.T, provider string, login func(context.Context, func(string) error) (auth.Credentials, error)) (*authBackend, string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "chatgpt.json")
	service := auth.NewService(path, login)
	return &authBackend{fakeBackend: &fakeBackend{info: app.Info{Provider: provider}}, authentication: service, dynamic: true}, path
}

func (b *authBackend) DynamicContentAvailable() bool { return b.dynamic }

func TestREPLLoginStatusReportsNotSignedIn(t *testing.T) {
	backend, _ := newAuthBackend(t, "chatgpt", nil)
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login status\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "Not signed in") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLLoginStatusReportsSignedIn(t *testing.T) {
	backend, path := newAuthBackend(t, "chatgpt", nil)
	if err := (auth.Credentials{AccountID: "acct-9"}).Save(path); err != nil {
		t.Fatal(err)
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login status\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := stdout.String(); strings.Contains(got, "acct-9") || !strings.Contains(got, "Signed in to ChatGPT") {
		t.Fatalf("stdout = %q", got)
	}
}

func TestREPLLoginSavesCredentials(t *testing.T) {
	backend, path := newAuthBackend(t, "chatgpt", func(_ context.Context, open func(string) error) (auth.Credentials, error) {
		_ = open("https://auth.example/authorize?x=1")
		return auth.Credentials{AccessToken: "secret-token", AccountID: "acct-7"}, nil
	})
	browserLaunches := 0
	previous := replOpenBrowser
	replOpenBrowser = func(string) { browserLaunches++ }
	t.Cleanup(func() { replOpenBrowser = previous })
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	out := stdout.String()
	if strings.Contains(out, "acct-7") || !strings.Contains(out, "Signed in to ChatGPT") || !strings.Contains(out, "Restart Otto") || !strings.Contains(out, "auth.example") {
		t.Fatalf("stdout = %q", out)
	}
	if strings.Contains(out, "secret-token") {
		t.Fatalf("stdout leaked the access token: %q", out)
	}
	creds, err := auth.Load(path)
	if err != nil || creds.AccountID != "acct-7" {
		t.Fatalf("saved credentials = %#v, %v", creds, err)
	}
	if browserLaunches != 1 {
		t.Fatalf("browser launch seam calls = %d, want 1", browserLaunches)
	}
}

func TestREPLLoginNonChatGPTProviderExplainsAPIKey(t *testing.T) {
	backend, _ := newAuthBackend(t, "openai-compatible", nil)
	backend.authentication = unsupportedAuthentication{}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "API key") {
		t.Fatalf("stdout=%q", stdout.String())
	}
}

func TestREPLLogoutRemovesCredentials(t *testing.T) {
	backend, path := newAuthBackend(t, "openai-compatible", nil)
	if err := (auth.Credentials{AccountID: "acct-1"}).Save(path); err != nil {
		t.Fatal(err)
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/logout\n/exit\n"), &stdout, &stderr, backend)
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
	backend, _ := newAuthBackend(t, "openai-compatible", nil)
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/logout\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "Not signed in") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLLoginFailureIsBounded(t *testing.T) {
	backend, _ := newAuthBackend(t, "chatgpt", func(context.Context, func(string) error) (auth.Credentials, error) {
		return auth.Credentials{}, errors.New("repl-login-secret")
	})
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err == nil {
		t.Fatal("Run() unexpectedly succeeded")
	}
	if strings.Contains(stderr.String(), "repl-login-secret") {
		t.Fatalf("stderr leaked login error: %q", stderr.String())
	}
}

func TestREPLLoginCommandsUnavailableWhenDynamicContentIsSuppressed(t *testing.T) {
	loginCalls := 0
	backend, path := newAuthBackend(t, "chatgpt", func(context.Context, func(string) error) (auth.Credentials, error) {
		loginCalls++
		return auth.Credentials{}, nil
	})
	backend.dynamic = false
	if err := (auth.Credentials{AccountID: "acct-1"}).Save(path); err != nil {
		t.Fatal(err)
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/login status\n/logout\n/login\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := stdout.String(); strings.Count(got, app.ErrAuthenticationUnavailable.Error()) != 3 || loginCalls != 0 {
		t.Fatalf("stdout=%q loginCalls=%d", got, loginCalls)
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
