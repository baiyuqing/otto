package session

import (
	"context"
	"errors"
	"sync"

	"github.com/baiyuqing/otto/internal/model"
)

type Memory struct {
	mu       sync.Mutex
	header   Header
	messages []model.Message
	closed   bool
}

func NewMemory(header Header) *Memory {
	return &Memory{header: header}
}

func (m *Memory) Header() Header {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.header
}

func (m *Memory) Messages() []model.Message {
	m.mu.Lock()
	defer m.mu.Unlock()
	return cloneMessages(m.messages)
}

func (m *Memory) Append(ctx context.Context, message model.Message) error {
	if err := ctx.Err(); err != nil {
		return err
	}

	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return errSessionClosed
	}
	m.messages = append(m.messages, cloneMessage(message))
	return nil
}

func (m *Memory) Path() string {
	return ""
}

func (m *Memory) Close() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.closed = true
	return nil
}

var errSessionClosed = errors.New("session is closed")
