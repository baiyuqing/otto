package app

import (
	"context"
	"errors"
	"strings"

	"github.com/baiyuqing/otto/internal/session"
)

var ErrSessionRenameUnavailable = errors.New("session rename is unavailable")

// RenameSession updates the current session's display name using the session's
// append-only metadata mechanism. It is mutually exclusive with active turns
// and replacement operations.
func (c *Controller) RenameSession(ctx context.Context, name string) error {
	name = strings.TrimSpace(name)
	if name == "" {
		return session.ErrInvalidSession
	}

	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return ErrClosed
	}
	if !c.dynamicContent {
		c.mu.Unlock()
		return ErrPersistenceDisabled
	}
	if c.active != nil || c.replace != nil {
		c.mu.Unlock()
		return ErrPromptActive
	}
	renamer, ok := c.current.(session.Renamer)
	c.mu.Unlock()
	if !ok {
		return ErrSessionRenameUnavailable
	}
	return renamer.Rename(ctx, name)
}
