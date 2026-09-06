package app

import (
	"context"
	"errors"
	"testing"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/session"
)

func TestControllerRenameCurrentSession(t *testing.T) {
	current := &fakeSession{header: testHeader("initial")}
	controller, err := New(SessionReplacement{Session: current, Runner: runnerFunc(noopRun)}, WithDynamicContent(true))
	if err != nil {
		t.Fatal(err)
	}

	if err := controller.RenameSession(context.Background(), "dev"); err != nil {
		t.Fatal(err)
	}
	if current.name != "dev" {
		t.Fatalf("session name = %q, want dev", current.name)
	}
}

func TestControllerRenameCurrentSessionRejectsBlankName(t *testing.T) {
	current := &fakeSession{header: testHeader("initial")}
	controller, err := New(SessionReplacement{Session: current, Runner: runnerFunc(noopRun)}, WithDynamicContent(true))
	if err != nil {
		t.Fatal(err)
	}

	if err := controller.RenameSession(context.Background(), " \t\n "); !errors.Is(err, session.ErrInvalidSession) {
		t.Fatalf("RenameSession() error = %v, want ErrInvalidSession", err)
	}
	if current.name != "" {
		t.Fatalf("session name = %q, want unchanged", current.name)
	}
}

func TestControllerRenameCurrentSessionRequiresPersistence(t *testing.T) {
	controller, err := New(SessionReplacement{Session: &fakeSession{header: testHeader("initial")}, Runner: runnerFunc(noopRun)}, WithDynamicContent(false))
	if err != nil {
		t.Fatal(err)
	}
	if err := controller.RenameSession(context.Background(), "dev"); !errors.Is(err, ErrPersistenceDisabled) {
		t.Fatalf("RenameSession() error = %v, want ErrPersistenceDisabled", err)
	}
}

func TestControllerRenameCurrentSessionRejectsActivePrompt(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	runner := runnerFunc(func(context.Context, string, func(agent.Event)) error {
		close(started)
		<-release
		return nil
	})
	controller, err := New(SessionReplacement{Session: &fakeSession{header: testHeader("initial")}, Runner: runner}, WithDynamicContent(true))
	if err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { done <- controller.Prompt(context.Background(), "hello", nil) }()
	<-started
	if err := controller.RenameSession(context.Background(), "dev"); !errors.Is(err, ErrPromptActive) {
		close(release)
		t.Fatalf("RenameSession() error = %v, want ErrPromptActive", err)
	}
	close(release)
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}
