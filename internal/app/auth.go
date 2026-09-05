package app

import (
	"context"
	"errors"
)

var (
	ErrAuthenticationUnavailable = errors.New("chatgpt sign-in is unavailable in this session")
	ErrAuthenticationUnsupported = errors.New("chatgpt sign-in is unavailable for the selected provider")
)

// Authentication is the narrow interactive authentication capability exposed
// to frontends. Implementations own credential storage and OAuth details.
type Authentication interface {
	Login(context.Context, func(string) error) error
	Logout(context.Context) (bool, error)
	Status(context.Context) (line string, signedIn bool)
}

func (c *Controller) Login(ctx context.Context, open func(string) error) error {
	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return ErrClosed
	}
	if !c.dynamicContent || c.authentication == nil {
		c.mu.Unlock()
		return ErrAuthenticationUnavailable
	}
	authentication := c.authentication
	provider := ""
	if c.runtimeInfo != nil {
		provider = c.runtimeInfo.Provider
	}
	current := c.current
	c.mu.Unlock()
	if provider == "" && current != nil {
		provider = current.Header().Provider
	}
	if provider != "chatgpt" {
		return ErrAuthenticationUnsupported
	}
	return authentication.Login(ctx, open)
}

func (c *Controller) Logout(ctx context.Context) (bool, error) {
	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return false, ErrClosed
	}
	if !c.dynamicContent || c.authentication == nil {
		c.mu.Unlock()
		return false, ErrAuthenticationUnavailable
	}
	authentication := c.authentication
	c.mu.Unlock()
	return authentication.Logout(ctx)
}

func (c *Controller) Status(ctx context.Context) (string, bool) {
	c.mu.Lock()
	if c.closed || !c.dynamicContent || c.authentication == nil {
		c.mu.Unlock()
		return ErrAuthenticationUnavailable.Error(), false
	}
	authentication := c.authentication
	c.mu.Unlock()
	return authentication.Status(ctx)
}
