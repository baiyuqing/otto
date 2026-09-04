package server

import (
	"encoding/json"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
)

type wireToolResult struct {
	Content string `json:"content"`
	IsError bool   `json:"is_error"`
}

type wireCompaction struct {
	CheckpointID         string       `json:"checkpoint_id,omitempty"`
	Reason               string       `json:"reason"`
	TokensBefore         int          `json:"tokens_before"`
	EstimatedTokensAfter int          `json:"estimated_tokens_after"`
	Automatic            bool         `json:"automatic"`
	Usage                *model.Usage `json:"usage,omitempty"`
	Noop                 bool         `json:"noop"`
}

type wirePlan struct {
	Reason               string `json:"reason"`
	Automatic            bool   `json:"automatic"`
	TokensBefore         int    `json:"tokens_before"`
	EstimatedTokensAfter int    `json:"estimated_tokens_after"`
	SummarizedMessages   int    `json:"summarized_messages"`
	RetainedMessages     int    `json:"retained_messages"`
	Mode                 string `json:"mode"`
}

type wireEvent struct {
	Type       string          `json:"type"`
	TurnID     string          `json:"turn_id,omitempty"`
	TaskID     string          `json:"task_id,omitempty"`
	Text       string          `json:"text,omitempty"`
	ToolName   string          `json:"tool_name,omitempty"`
	ToolCallID string          `json:"tool_call_id,omitempty"`
	ToolArgs   json.RawMessage `json:"tool_args,omitempty"`
	Result     *wireToolResult `json:"result,omitempty"`
	Usage      *model.Usage    `json:"usage,omitempty"`
	Compaction *wireCompaction `json:"compaction,omitempty"`
	Plan       *wirePlan       `json:"plan,omitempty"`
	Error      string          `json:"error,omitempty"`
}

// toWire converts an agent event to its wire form. It never copies pointers
// from event: every nested struct is rebuilt so the returned value owns its
// own memory.
func toWire(event agent.Event) wireEvent {
	wire := wireEvent{Type: string(event.Type)}
	switch event.Type {
	case agent.EventTextDelta:
		wire.Text = event.Text
	case agent.EventToolCallStarted:
		wire.ToolName = event.ToolName
		wire.ToolCallID = event.ToolCallID
		wire.ToolArgs = toolArgsRaw(event.ToolArgs)
	case agent.EventToolCallFinished:
		wire.ToolName = event.ToolName
		wire.ToolCallID = event.ToolCallID
		wire.Result = &wireToolResult{Content: event.ToolResult.Content, IsError: event.ToolResult.IsError}
	case agent.EventProviderUsage:
		usage := event.Usage
		wire.Usage = &usage
	case agent.EventNotification:
		wire.TaskID = event.TaskID
		wire.Text = event.Text
		usage := event.Usage
		wire.Usage = &usage
	case agent.EventCompactionStarted, agent.EventCompactionCompleted:
		wire.Compaction = toWireCompaction(event.Compaction)
	case agent.EventCompactionPlanned:
		wire.Plan = toWirePlan(event.Plan)
	}
	if event.Err != nil {
		wire.Error = event.Err.Error()
	}
	return wire
}

func toolArgsRaw(args string) json.RawMessage {
	if args == "" {
		return nil
	}
	if json.Valid([]byte(args)) {
		return json.RawMessage(args)
	}
	quoted, err := json.Marshal(args)
	if err != nil {
		return nil
	}
	return json.RawMessage(quoted)
}

func toWireCompaction(c *agent.CompactionEvent) *wireCompaction {
	if c == nil {
		return nil
	}
	wire := &wireCompaction{
		CheckpointID:         c.CheckpointID,
		Reason:               string(c.Reason),
		TokensBefore:         c.TokensBefore,
		EstimatedTokensAfter: c.EstimatedTokensAfter,
		Automatic:            c.Automatic,
		Noop:                 c.Noop,
	}
	if c.UsagePresent {
		usage := c.Usage
		wire.Usage = &usage
	}
	return wire
}

func toWirePlan(p *agent.CompactionPlan) *wirePlan {
	if p == nil {
		return nil
	}
	return &wirePlan{
		Reason:               string(p.Reason),
		Automatic:            p.Automatic,
		TokensBefore:         p.TokensBefore,
		EstimatedTokensAfter: p.EstimatedTokensAfter,
		SummarizedMessages:   p.SummarizedMessages,
		RetainedMessages:     p.RetainedMessages,
		Mode:                 string(p.Mode),
	}
}
