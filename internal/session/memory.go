package session

import (
	"context"
	"errors"
	"sync"

	"github.com/baiyuqing/otto/internal/model"
)

type Memory struct {
	mu                  sync.Mutex
	header              Header
	messages            []model.Message
	aggregateUsage      model.Usage
	usagePresent        bool
	latestCompaction    CompactionMetadata
	hasLatestCompaction bool
	closed              bool
}

func NewMemory(header Header) *Memory {
	header.Version = CurrentVersion
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

func (m *Memory) AggregateUsage() (model.Usage, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.aggregateUsage, m.usagePresent
}

func (m *Memory) UpdateRuntime(ctx context.Context, runtime RuntimeMetadata) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return errSessionClosed
	}
	if runtime.Provider == "" || runtime.Model == "" {
		return ErrInvalidSession
	}
	m.header.Profile = runtime.Profile
	m.header.Provider = runtime.Provider
	m.header.Model = runtime.Model
	return nil
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
	cloned := cloneMessage(message)
	m.messages = append(m.messages, cloned)
	if cloned.Role == model.RoleAssistant && hasMeaningfulUsage(cloned.Usage) {
		m.aggregateUsage = addResolvedUsage(m.aggregateUsage, cloned.Usage)
		m.usagePresent = true
	}
	if m.hasLatestCompaction && m.latestCompaction.FirstPostCheckpointMessageID == "" {
		m.latestCompaction.FirstPostCheckpointMessageID = cloned.ID
	}
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
