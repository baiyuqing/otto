package provider

import (
	"context"

	"github.com/baiyuqing/otto/internal/model"
)

type Request struct {
	Model        string
	SystemPrompt string
	Thinking     string
	Messages     []model.Message
	Tools        []model.ToolDefinition
}

type Response struct {
	// Message is the single source of response finish and usage metadata.
	Message model.Message
}

type StreamEventType string

const (
	StreamTextDelta     StreamEventType = "text_delta"
	StreamToolCallDelta StreamEventType = "tool_call_delta"
)

type StreamEvent struct {
	Type       StreamEventType
	Text       string
	ToolCallID string
	ToolName   string
	Arguments  string
}

type RequestSizer interface {
	SerializedRequestSize(Request) (int, error)
}

// Provider instances may be shared by parent and child agents. Complete must
// be safe for concurrent calls. The request and stream-event payloads are
// borrowed read-only; callbacks for one call are ordered, synchronous, and
// finished before Complete returns. Implementations must not invoke a
// callback after returning.
type Provider interface {
	Complete(context.Context, Request, func(StreamEvent)) (Response, error)
}
