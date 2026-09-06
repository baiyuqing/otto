package tui

import (
	"context"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/session"
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

func TestRenameCommandRenamesCurrentSession(t *testing.T) {
	var gotName string
	backend := &renameBackend{rename: func(_ context.Context, name string) error {
		gotName = name
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)

	updated, cmd := submitCommand(t, m, "/rename dev")
	got := updated
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil", cmd)
	}
	if gotName != "dev" {
		t.Fatalf("renamed to %q, want dev", gotName)
	}
	if got.statusText != "renamed session to dev" || got.editor.Value() != "" {
		t.Fatalf("status=%q editor=%q", got.statusText, got.editor.Value())
	}
}

func TestRenameCommandValidatesNameAndBackend(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	got, cmd := submitCommand(t, m, "/rename")
	if cmd != nil || got.statusText != "usage: /rename <name>" {
		t.Fatalf("blank rename cmd=%v status=%q", cmd, got.statusText)
	}

	got, cmd = submitCommand(t, m, "/rename dev")
	if cmd != nil || got.statusText != app.ErrPersistenceDisabled.Error() {
		t.Fatalf("missing backend cmd=%v status=%q", cmd, got.statusText)
	}

	backend := &renameBackend{rename: func(context.Context, string) error { return session.ErrInvalidSession }}
	got, cmd = submitCommand(t, resizeModel(t, newTestModelWithBackend(t, backend), 80, 12), "/rename dev")
	if cmd != nil || got.statusText != session.ErrInvalidSession.Error() {
		t.Fatalf("rename error cmd=%v status=%q", cmd, got.statusText)
	}
}
