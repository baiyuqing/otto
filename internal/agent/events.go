package agent

import (
	"errors"
	"time"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/tool"
)

type EventType string

const (
	EventAgentStarted     EventType = "agent_started"
	EventAgentFinished    EventType = "agent_finished"
	EventTextDelta        EventType = "text_delta"
	EventToolCallStarted  EventType = "tool_call_started"
	EventToolCallFinished EventType = "tool_call_finished"
	EventProviderUsage    EventType = "provider_usage"
	EventAgentError       EventType = "agent_error"
)

type Event struct {
	Type       EventType
	Text       string
	ToolName   string
	ToolCallID string
	ToolResult tool.Result
	Usage      model.Usage
	Err        error
}

type Options struct {
	Model        string
	SystemPrompt string
	MaxTurns     int
	Now          func() time.Time
	NewID        func() string
}

var (
	ErrMaxTurns      = errors.New("agent max turns exceeded")
	ErrEmptyUserText = errors.New("user text is required")
)
