package session

import (
	"context"
	"errors"
	"fmt"
	"strings"
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
	if m.hasLatestCompaction && m.latestCompaction.FirstPostCheckpointMessageID == "" {
		if err := m.validateFirstPostCheckpointMessage(cloned); err != nil {
			return err
		}
	}
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

func (m *Memory) validateFirstPostCheckpointMessage(message model.Message) error {
	if strings.TrimSpace(message.ID) == "" {
		return fmt.Errorf("%w: first post-checkpoint message id is required", ErrInvalidSession)
	}
	switch message.Role {
	case model.RoleUser, model.RoleAssistant, model.RoleTool:
	default:
		return fmt.Errorf("%w: first post-checkpoint message must have a normal role", ErrInvalidSession)
	}
	if _, _, err := modelMessageToPiEntry(message, "00000000", nil, m.header); err != nil {
		return err
	}
	candidate := append(cloneMessages(m.messages), message)
	if _, err := pendingToolCalls(candidate); err != nil {
		return err
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
