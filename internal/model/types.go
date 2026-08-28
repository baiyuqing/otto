package model

import (
	"encoding/json"
	"errors"
	"strings"
	"time"
)

type Role string

const (
	RoleUser      Role = "user"
	RoleAssistant Role = "assistant"
	RoleTool      Role = "tool"
	RoleContext   Role = "context"
)

type BlockType string

const (
	BlockText       BlockType = "text"
	BlockToolCall   BlockType = "tool_call"
	BlockToolResult BlockType = "tool_result"
)

type Block struct {
	Type       BlockType       `json:"type"`
	Text       string          `json:"text,omitempty"`
	ToolCallID string          `json:"tool_call_id,omitempty"`
	ToolName   string          `json:"tool_name,omitempty"`
	Arguments  json.RawMessage `json:"arguments,omitempty"`
	IsError    bool            `json:"is_error,omitempty"`
}

type Message struct {
	ID                  string       `json:"id"`
	Role                Role         `json:"role"`
	Blocks              []Block      `json:"blocks"`
	CreatedAt           time.Time    `json:"created_at"`
	FinishReason        FinishReason `json:"finish_reason,omitempty"`
	Usage               *Usage       `json:"usage,omitempty"`
	ContextType         string       `json:"context_type,omitempty"`
	ContextTokensBefore int          `json:"context_tokens_before,omitempty"`
	Display             bool         `json:"display,omitempty"`
}

func (m Message) Text() string {
	var builder strings.Builder
	for _, block := range m.Blocks {
		if block.Type == BlockText {
			builder.WriteString(block.Text)
		}
	}
	return builder.String()
}

type ToolDefinition struct {
	Name        string         `json:"name"`
	Description string         `json:"description"`
	Parameters  map[string]any `json:"parameters"`
}

type FinishReason string

const (
	FinishStop      FinishReason = "stop"
	FinishToolCalls FinishReason = "tool_calls"
	FinishLength    FinishReason = "length"
	FinishUnknown   FinishReason = "unknown"
)

type Usage struct {
	InputTokens       int `json:"input_tokens"`
	OutputTokens      int `json:"output_tokens"`
	CachedInputTokens int `json:"cached_input_tokens,omitempty"`
}

func (u Usage) Validate() error {
	if u.InputTokens < 0 || u.OutputTokens < 0 || u.CachedInputTokens < 0 {
		return errors.New("usage token counts must be nonnegative")
	}
	if u.CachedInputTokens > u.InputTokens {
		return errors.New("cached input tokens must not exceed input tokens")
	}
	return nil
}
