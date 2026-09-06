package tui

import (
	"context"
	"strings"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/session"
)

type sessionRenamer interface {
	RenameSession(context.Context, string) error
}

func sessionRenamerFromBackend(backend app.Backend) (sessionRenamer, bool) {
	renamer, ok := backend.(sessionRenamer)
	return renamer, ok
}

func (m Model) handleRenameCommand(argument string) (Model, bool) {
	name := strings.TrimSpace(argument)
	if name == "" {
		m.statusText = "usage: /rename <name>"
		return m, true
	}
	if m.running || m.newSessionPending || m.resume.active() || m.archive.active() {
		m.statusText = app.ErrPromptActive.Error()
		return m, true
	}
	renamer, ok := sessionRenamerFromBackend(m.backend)
	if !ok {
		m.statusText = app.ErrPersistenceDisabled.Error()
		return m, true
	}
	if err := renamer.RenameSession(rootContext(m.rootCtx), name); err != nil {
		if err == session.ErrInvalidSession {
			m.statusText = err.Error()
		} else {
			m.statusText = boundedResumeError(err)
		}
		return m, true
	}
	m.clearEditor()
	m.statusText = "renamed session to " + name
	return m, true
}
