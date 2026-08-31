package repl

import (
	"bytes"
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/session"
)

type replArchiveBackend struct {
	fakeBackend
	archiveCurrent func(context.Context) (session.ArchiveResult, error)
}

func (b *replArchiveBackend) ListSessions(context.Context, int) (session.ListResult, error) {
	return session.ListResult{}, nil
}

func (b *replArchiveBackend) ResumeSession(context.Context, string) (app.ResumeResult, error) {
	return app.ResumeResult{}, nil
}

func (b *replArchiveBackend) ArchiveSession(context.Context, string) (session.ArchiveResult, error) {
	return session.ArchiveResult{}, app.ErrPersistenceDisabled
}

func (b *replArchiveBackend) ArchiveCurrentSession(ctx context.Context) (session.ArchiveResult, error) {
	if b.archiveCurrent == nil {
		return session.ArchiveResult{}, app.ErrPersistenceDisabled
	}
	return b.archiveCurrent(ctx)
}

func TestREPLArchiveCurrentSessionPrintsPathsAndContinues(t *testing.T) {
	backend := &replArchiveBackend{fakeBackend: fakeBackend{info: app.Info{SessionID: "old"}}}
	backend.archiveCurrent = func(ctx context.Context) (session.ArchiveResult, error) {
		backend.info.SessionID = "new"
		return session.ArchiveResult{Path: "/sessions/archive/old.jsonl", ID: "old"}, nil
	}
	input := strings.NewReader("/archive\n/session\n/exit\n")
	var output bytes.Buffer
	console := New(input, &output, &output, backend)
	if err := console.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(output.String(), "Archived: /sessions/archive/old.jsonl") {
		t.Fatalf("output missing archived path: %q", output.String())
	}
	if !strings.Contains(output.String(), "ID: new") {
		t.Fatalf("output missing fresh session id: %q", output.String())
	}
}

func TestREPLArchiveRequiresArchiverBackend(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{}
	r := New(strings.NewReader("/archive\n"), &stdout, &stderr, backend)
	err := r.Run(context.Background())
	if !errors.Is(err, app.ErrPersistenceDisabled) {
		t.Fatalf("Run() error = %v, want ErrPersistenceDisabled", err)
	}
	if !IsCommandError(err, "/archive") {
		t.Fatalf("Run() error = %v, want /archive command error", err)
	}
}

func TestREPLArchiveFailureIsCommandError(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &replArchiveBackend{}
	backend.archiveCurrent = func(context.Context) (session.ArchiveResult, error) {
		return session.ArchiveResult{}, errors.New("archive failed")
	}
	r := New(strings.NewReader("/archive\n"), &stdout, &stderr, backend)
	err := r.Run(context.Background())
	if err == nil || err.Error() != "archive failed" {
		t.Fatalf("Run() error = %v, want archive failed", err)
	}
	if !IsCommandError(err, "/archive") {
		t.Fatalf("Run() error = %v, want /archive command error", err)
	}
}

func TestREPLHelpListsArchiveCommand(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{}
	r := New(strings.NewReader("/help\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "/archive") || !strings.Contains(stdout.String(), "archive current session") {
		t.Fatalf("help = %q", stdout.String())
	}
}
