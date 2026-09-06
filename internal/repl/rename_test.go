package repl

import (
	"context"
	"strings"
	"testing"
)

type renameBackend struct {
	fakeBackend
	rename func(context.Context, string) error
}

func (b *renameBackend) RenameSession(ctx context.Context, name string) error {
	if b.rename == nil {
		return nil
	}
	return b.rename(ctx, name)
}

func TestREPLRenameCommand(t *testing.T) {
	var renamed string
	backend := &renameBackend{rename: func(_ context.Context, name string) error {
		renamed = name
		return nil
	}}
	var stdout, stderr strings.Builder
	r := New(strings.NewReader("/rename dev\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if renamed != "dev" {
		t.Fatalf("renamed = %q, want dev", renamed)
	}
	if !strings.Contains(stdout.String(), "Renamed session: dev") || stderr.String() != "" {
		t.Fatalf("stdout=%q stderr=%q", stdout.String(), stderr.String())
	}
}

func TestREPLRenameCommandRequiresName(t *testing.T) {
	var stdout, stderr strings.Builder
	r := New(strings.NewReader("/rename\n/exit\n"), &stdout, &stderr, &fakeBackend{})
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stderr.String(), "unknown command: /rename") {
		t.Fatalf("stderr = %q", stderr.String())
	}
}
