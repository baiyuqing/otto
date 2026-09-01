package app

import (
	"context"
	"errors"
	"testing"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/session"
)

func newControllerWithArchiver(t *testing.T, initial session.Session, runner Runner, archive ArchiveFactory) *Controller {
	t.Helper()
	controller, err := New(initial, func() (session.Session, error) {
		return &fakeSession{header: testHeader("next")}, nil
	}, func(session.Session) Runner { return runner },
		WithRuntimeInfo(RuntimeInfo{Provider: "openai-compatible", Profile: "old", Model: "old-model"}),
		WithSessionArchiver(archive))
	if err != nil {
		t.Fatal(err)
	}
	return controller
}

func TestArchiveSessionArchivesNonCurrentPath(t *testing.T) {
	var gotPath string
	archive := func(ctx context.Context, path string) (session.ArchiveResult, error) {
		gotPath = path
		return session.ArchiveResult{Path: "/sessions/archive/other.jsonl", ID: "other"}, nil
	}
	controller := newControllerWithArchiver(t, &fakeSession{header: testHeader("initial")}, runnerFunc(noopRun), archive)

	result, err := controller.ArchiveSession(context.Background(), "/sessions/other.jsonl")
	if err != nil {
		t.Fatal(err)
	}
	if gotPath != "/sessions/other.jsonl" {
		t.Fatalf("archive path = %q", gotPath)
	}
	if result.ID != "other" {
		t.Fatalf("result = %#v", result)
	}
	if info := controller.Info(); info.SessionID != "initial" {
		t.Fatalf("current session changed to %q", info.SessionID)
	}
}

func TestArchiveSessionDelegatesCurrentPathToArchiveCurrent(t *testing.T) {
	var archiveCalls []string
	archive := func(ctx context.Context, path string) (session.ArchiveResult, error) {
		archiveCalls = append(archiveCalls, path)
		return session.ArchiveResult{Path: path + ".archived", ID: "initial"}, nil
	}
	controller := newControllerWithArchiver(t, &fakeSession{header: testHeader("initial")}, runnerFunc(noopRun), archive)

	result, err := controller.ArchiveSession(context.Background(), "/sessions/initial.jsonl")
	if err != nil {
		t.Fatal(err)
	}
	if len(archiveCalls) != 1 || archiveCalls[0] != "/sessions/initial.jsonl" {
		t.Fatalf("archive calls = %#v", archiveCalls)
	}
	if result.ID != "initial" {
		t.Fatalf("result = %#v", result)
	}
	if info := controller.Info(); info.SessionID != "next" {
		t.Fatalf("current session id = %q, want next", info.SessionID)
	}
}

func TestArchiveSessionRejectsActivePrompt(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	runner := runnerFunc(func(ctx context.Context, text string, emit func(agent.Event)) error {
		close(started)
		<-release
		return nil
	})
	controller := newControllerWithArchiver(t, &fakeSession{header: testHeader("initial")}, runner, func(ctx context.Context, path string) (session.ArchiveResult, error) {
		return session.ArchiveResult{}, nil
	})
	done := make(chan error, 1)
	go func() { done <- controller.Prompt(context.Background(), "one", func(agent.Event) {}) }()
	<-started
	if _, err := controller.ArchiveSession(context.Background(), "/sessions/other.jsonl"); !errors.Is(err, ErrPromptActive) {
		t.Fatalf("error = %v, want ErrPromptActive", err)
	}
	close(release)
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func TestArchiveSessionRequiresArchiver(t *testing.T) {
	controller := newTestController(t, runnerFunc(noopRun))
	if _, err := controller.ArchiveSession(context.Background(), "/sessions/other.jsonl"); !errors.Is(err, ErrPersistenceDisabled) {
		t.Fatalf("error = %v, want ErrPersistenceDisabled", err)
	}
}

func TestArchiveSessionFactoryFailureLeavesCurrentSession(t *testing.T) {
	controller := newControllerWithArchiver(t, &fakeSession{header: testHeader("initial")}, runnerFunc(noopRun), func(ctx context.Context, path string) (session.ArchiveResult, error) {
		return session.ArchiveResult{}, errors.New("archive failed")
	})
	if _, err := controller.ArchiveSession(context.Background(), "/sessions/other.jsonl"); err == nil || err.Error() != "archive failed" {
		t.Fatalf("error = %v", err)
	}
	if info := controller.Info(); info.SessionID != "initial" {
		t.Fatalf("current session changed to %q", info.SessionID)
	}
}

func TestArchiveCurrentSessionArchivesAndStartsFreshSession(t *testing.T) {
	var gotPath string
	archive := func(ctx context.Context, path string) (session.ArchiveResult, error) {
		gotPath = path
		return session.ArchiveResult{Path: "/sessions/archive/initial.jsonl", ID: "initial"}, nil
	}
	current := &fakeSession{header: testHeader("initial"), path: "/sessions/initial.jsonl"}
	controller := newControllerWithArchiver(t, current, runnerFunc(noopRun), archive)

	result, err := controller.ArchiveCurrentSession(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if gotPath != "/sessions/initial.jsonl" {
		t.Fatalf("archive path = %q", gotPath)
	}
	if result.ID != "initial" {
		t.Fatalf("result = %#v", result)
	}
	if info := controller.Info(); info.SessionID != "next" {
		t.Fatalf("current session id = %q, want next", info.SessionID)
	}
	if !current.closed {
		t.Fatal("current session was not closed")
	}
}

func TestArchiveCurrentSessionBuildFailureRetainsCurrent(t *testing.T) {
	archiveCalled := false
	archive := func(ctx context.Context, path string) (session.ArchiveResult, error) {
		archiveCalled = true
		return session.ArchiveResult{}, nil
	}
	current := &fakeSession{header: testHeader("initial"), path: "/sessions/initial.jsonl"}
	controller, err := New(current, func() (session.Session, error) {
		return nil, errors.New("create failed")
	}, func(session.Session) Runner { return runnerFunc(noopRun) }, WithSessionArchiver(archive))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := controller.ArchiveCurrentSession(context.Background()); err == nil || err.Error() != "create failed" {
		t.Fatalf("error = %v", err)
	}
	if archiveCalled {
		t.Fatal("archive called despite build failure")
	}
	if info := controller.Info(); info.SessionID != "initial" {
		t.Fatalf("current session changed to %q", info.SessionID)
	}
	if current.closed {
		t.Fatal("current session was closed despite build failure")
	}
}

func TestArchiveCurrentSessionArchiveFailureRetainsCurrentAndClosesCandidate(t *testing.T) {
	var closeCalls int
	current := &fakeSession{header: testHeader("initial"), path: "/sessions/initial.jsonl"}
	candidate := &fakeSession{header: testHeader("next")}
	candidate.onClose = func() { closeCalls++ }
	runner := &archiveClosableRunner{}
	controller, err := New(current, func() (session.Session, error) {
		return candidate, nil
	}, func(session.Session) Runner { return runner }, WithSessionArchiver(func(ctx context.Context, path string) (session.ArchiveResult, error) {
		return session.ArchiveResult{}, errors.New("archive failed")
	}))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := controller.ArchiveCurrentSession(context.Background()); err == nil || err.Error() != "archive failed" {
		t.Fatalf("error = %v", err)
	}
	if info := controller.Info(); info.SessionID != "initial" {
		t.Fatalf("current session changed to %q", info.SessionID)
	}
	if closeCalls != 1 {
		t.Fatalf("candidate close calls = %d, want 1", closeCalls)
	}
	if runner.closeCalls != 1 {
		t.Fatalf("candidate runner close calls = %d, want 1", runner.closeCalls)
	}
	if current.closed {
		t.Fatal("current session was closed despite archive failure")
	}
}

func TestArchiveCurrentSessionCloseFailureFatalState(t *testing.T) {
	current := &fakeSession{header: testHeader("initial"), path: "/sessions/initial.jsonl", closeErr: errors.New("close failed")}
	controller := newControllerWithArchiver(t, current, runnerFunc(noopRun), func(ctx context.Context, path string) (session.ArchiveResult, error) {
		return session.ArchiveResult{Path: "/sessions/archive/initial.jsonl", ID: "initial"}, nil
	})
	if _, err := controller.ArchiveCurrentSession(context.Background()); err == nil || err.Error() != "close failed" {
		t.Fatalf("error = %v", err)
	}
	if _, err := controller.ArchiveCurrentSession(context.Background()); !errors.Is(err, ErrClosed) {
		t.Fatalf("second error = %v, want ErrClosed", err)
	}
}

func TestArchiveCurrentSessionRequiresArchiver(t *testing.T) {
	controller := newTestController(t, runnerFunc(noopRun))
	if _, err := controller.ArchiveCurrentSession(context.Background()); !errors.Is(err, ErrPersistenceDisabled) {
		t.Fatalf("error = %v, want ErrPersistenceDisabled", err)
	}
}

type archiveClosableRunner struct{ closeCalls int }

func (*archiveClosableRunner) Run(context.Context, string, func(agent.Event)) error { return nil }

func (*archiveClosableRunner) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{}, nil
}

func (r *archiveClosableRunner) Close() error {
	r.closeCalls++
	return nil
}
