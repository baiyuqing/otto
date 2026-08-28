package agent

import (
	"errors"
	"time"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/tool"
)

type EventType string

const (
	EventAgentStarted        EventType = "agent_started"
	EventAgentFinished       EventType = "agent_finished"
	EventTextDelta           EventType = "text_delta"
	EventToolCallStarted     EventType = "tool_call_started"
	EventToolCallFinished    EventType = "tool_call_finished"
	EventProviderUsage       EventType = "provider_usage"
	EventCompactionStarted   EventType = "compaction_started"
	EventCompactionCompleted EventType = "compaction_completed"
	EventCompactionWarning   EventType = "compaction_warning"
	EventAgentError          EventType = "agent_error"
)

type Event struct {
	Type       EventType
	Text       string
	ToolName   string
	ToolCallID string
	ToolArgs   string
	ToolResult tool.Result
	Usage      model.Usage
	Compaction *CompactionEvent
	Err        error
}

// CompactionReason identifies why a checkpoint was requested.
type CompactionReason string

const (
	CompactionManual    CompactionReason = "manual"
	CompactionThreshold CompactionReason = "threshold"
	CompactionOverflow  CompactionReason = "overflow"
)

type CompactionResult struct {
	CheckpointID         string
	Reason               CompactionReason
	TokensBefore         int
	EstimatedTokensAfter int
	Automatic            bool
	Usage                model.Usage
	UsagePresent         bool
	Noop                 bool
}

type CompactionEvent struct {
	CheckpointID         string
	Reason               CompactionReason
	TokensBefore         int
	EstimatedTokensAfter int
	Automatic            bool
	Usage                model.Usage
	UsagePresent         bool
	Noop                 bool
}

type CompactionSettings struct {
	Auto             bool
	HardInputWindow  int
	WorkingWindow    int
	ReserveTokens    int
	KeepRecentTokens int
}

type Options struct {
	Model        string
	SystemPrompt string
	Thinking     string
	RequestSizer provider.RequestSizer
	Compaction   CompactionSettings
	Now          func() time.Time
	NewID        func() string
}

var (
	ErrEmptyUserText            = errors.New("user text is required")
	ErrNothingToCompact         = errors.New("nothing to compact")
	ErrCurrentTurnTooLarge      = errors.New("current turn exceeds the retained input budget")
	ErrInvalidCompactionSummary = errors.New("invalid compaction summary")
)
