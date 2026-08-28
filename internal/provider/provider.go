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
	Message      model.Message
	FinishReason model.FinishReason
	Usage        model.Usage
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

type Provider interface {
	Complete(context.Context, Request, func(StreamEvent)) (Response, error)
}
