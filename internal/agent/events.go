package agent

import (
	"errors"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
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
	EventCompactionPlanned   EventType = "compaction_planned"
	EventCompactionCompleted EventType = "compaction_completed"
	EventCompactionWarning   EventType = "compaction_warning"
	EventMemoryWarning       EventType = "memory_warning"
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
	Plan       *CompactionPlan
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

// CompactionMode names the summarization shape the selection resolved to. It is
// deterministic and known before the provider summary call runs.
type CompactionMode string

const (
	CompactionModeStructured CompactionMode = "structured"
	CompactionModeTurnPrefix CompactionMode = "turn-prefix"
	CompactionModeSplitTurn  CompactionMode = "split-turn"
)

// CompactionPlan is the pre-execution view of a compaction: what it will
// summarize versus retain, and a token estimate. It carries no summary text,
// which does not exist until the provider call runs. EstimatedTokensAfter is a
// floor that excludes the not-yet-generated summary; CompactionEvent on
// completion carries the exact post-checkpoint estimate.
type CompactionPlan struct {
	Reason               CompactionReason
	Automatic            bool
	TokensBefore         int
	EstimatedTokensAfter int
	SummarizedMessages   int
	RetainedMessages     int
	Mode                 CompactionMode
}

type CompactionSettings struct {
	Auto             bool
	HardInputWindow  int
	WorkingWindow    int
	ReserveTokens    int
	KeepRecentTokens int
}

type Options struct {
	Model                   string
	SystemPrompt            string
	Thinking                string
	RequestSizer            provider.RequestSizer
	Compaction              CompactionSettings
	Now                     func() time.Time
	NewID                   func() string
	Memory                  memory.Binding
	MemoryRecallLimit       int
	MemoryRecallTokenBudget int
}

var (
	ErrEmptyUserText            = errors.New("user text is required")
	ErrNothingToCompact         = errors.New("nothing to compact")
	ErrCurrentTurnTooLarge      = errors.New("current turn exceeds the retained input budget")
	ErrInvalidCompactionSummary = errors.New("invalid compaction summary")
)
