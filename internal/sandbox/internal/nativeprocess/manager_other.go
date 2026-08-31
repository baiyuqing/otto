//go:build !darwin && !linux

package nativeprocess

import (
	"context"
	"errors"
	"io"
	"sync"

	"github.com/baiyuqing/otto/internal/sandbox"
)

type Spec struct {
	Path        string
	Args        []string
	Directory   string
	Environment []string
	Stdout      io.Writer
	Stderr      io.Writer
}

type Result struct {
	Code     int
	Signaled bool
	Signal   string
}

type Manager struct {
	mu        sync.Mutex
	closing   bool
	closeDone chan struct{}
	closeErr  error
}

func New() *Manager {
	return &Manager{closeDone: make(chan struct{})}
}

func (m *Manager) Run(ctx context.Context, _ Spec) (Result, error) {
	if ctx == nil {
		return Result{}, sandbox.ErrInvalidRequest
	}
	if err := safeContextError(ctx); err != nil {
		return Result{}, err
	}
	m.mu.Lock()
	closing := m.closing
	m.mu.Unlock()
	if closing {
		return Result{}, sandbox.ErrClosed
	}
	return Result{}, &sandbox.UnavailableError{Reason: sandbox.ReasonUnsupportedPlatform}
}

func (m *Manager) Close() error {
	m.mu.Lock()
	if m.closing {
		done := m.closeDone
		m.mu.Unlock()
		<-done
		m.mu.Lock()
		err := m.closeErr
		m.mu.Unlock()
		return err
	}
	m.closing = true
	close(m.closeDone)
	m.mu.Unlock()
	return nil
}

func safeContextError(ctx context.Context) error {
	if ctx.Err() == nil {
		return nil
	}
	if errors.Is(ctx.Err(), context.DeadlineExceeded) || errors.Is(context.Cause(ctx), context.DeadlineExceeded) {
		return context.DeadlineExceeded
	}
	return context.Canceled
}
