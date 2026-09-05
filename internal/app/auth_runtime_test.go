package app

import (
	"context"
	"testing"

	"github.com/baiyuqing/otto/internal/session"
)

func TestControllerLoginUsesEffectiveRuntimeProvider(t *testing.T) {
	called := false
	authentication := &fakeAuthentication{
		login:  func(context.Context, func(string) error) error { called = true; return nil },
		logout: func(context.Context) (bool, error) { return false, nil },
	}
	controller, err := New(
		SessionReplacement{Session: &fakeSession{header: session.Header{Provider: "openai-compatible"}}, Runner: runnerFunc(noopRun)},
		WithRuntimeInfo(RuntimeInfo{Provider: "chatgpt"}),
		WithAuthentication(authentication),
	)
	if err != nil {
		t.Fatal(err)
	}
	if err := controller.Login(context.Background(), nil); err != nil {
		t.Fatalf("Login() error = %v", err)
	}
	if !called {
		t.Fatal("Login() did not call the authentication service")
	}
}
