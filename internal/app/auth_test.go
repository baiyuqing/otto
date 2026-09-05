package app

import (
	"context"
	"errors"
	"testing"

	"github.com/baiyuqing/otto/internal/session"
)

type fakeAuthentication struct {
	login  func(context.Context, func(string) error) error
	logout func(context.Context) (bool, error)
	line   string
	signed bool
}

func (f *fakeAuthentication) Login(ctx context.Context, open func(string) error) error {
	return f.login(ctx, open)
}

func (f *fakeAuthentication) Logout(ctx context.Context) (bool, error) {
	return f.logout(ctx)
}

func (f *fakeAuthentication) Status(context.Context) (string, bool) {
	return f.line, f.signed
}

func newAuthController(t *testing.T, provider string, authentication Authentication, options ...Option) *Controller {
	t.Helper()
	initial := &fakeSession{header: session.Header{Provider: provider}}
	controller, err := New(SessionReplacement{Session: initial, Runner: runnerFunc(noopRun)},
		append(options, WithAuthentication(authentication))...,
	)
	if err != nil {
		t.Fatal(err)
	}
	return controller
}

func TestControllerAuthenticationGuardsProviderAndDynamicContent(t *testing.T) {
	called := false
	service := &fakeAuthentication{
		login:  func(context.Context, func(string) error) error { called = true; return nil },
		logout: func(context.Context) (bool, error) { return true, nil },
		line:   "Signed in to ChatGPT.", signed: true,
	}
	controller := newAuthController(t, "openai-compatible", service)
	if err := controller.Login(context.Background(), func(string) error { return nil }); !errors.Is(err, ErrAuthenticationUnsupported) {
		t.Fatalf("Login() error = %v, want unsupported", err)
	}
	if called {
		t.Fatal("Login() invoked service for unsupported provider")
	}
	line, signed := controller.Status(context.Background())
	if line != "Signed in to ChatGPT." || !signed {
		t.Fatalf("Status() = %q, %v", line, signed)
	}
	removed, err := controller.Logout(context.Background())
	if err != nil || !removed {
		t.Fatalf("Logout() = %v, %v", removed, err)
	}

	controller.mu.Lock()
	controller.dynamicContent = false
	controller.mu.Unlock()
	if err := controller.Login(context.Background(), nil); !errors.Is(err, ErrAuthenticationUnavailable) {
		t.Fatalf("suppressed Login() error = %v, want unavailable", err)
	}
	line, signed = controller.Status(context.Background())
	if line != ErrAuthenticationUnavailable.Error() || signed {
		t.Fatalf("suppressed Status() = %q, %v", line, signed)
	}
	if _, err := controller.Logout(context.Background()); !errors.Is(err, ErrAuthenticationUnavailable) {
		t.Fatalf("suppressed Logout() error = %v, want unavailable", err)
	}
}

func TestControllerAuthenticationPreservesCancellation(t *testing.T) {
	cancelled := errors.New("cancelled")
	service := &fakeAuthentication{
		login:  func(ctx context.Context, _ func(string) error) error { return ctx.Err() },
		logout: func(context.Context) (bool, error) { return false, cancelled },
	}
	controller := newAuthController(t, "chatgpt", service)
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if err := controller.Login(ctx, nil); !errors.Is(err, context.Canceled) {
		t.Fatalf("Login() error = %v, want context cancellation", err)
	}
}
