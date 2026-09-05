package model

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
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

func (b Block) Validate() error {
	switch b.Type {
	case BlockText:
		if b.ToolCallID != "" || b.ToolName != "" || len(b.Arguments) != 0 || b.IsError {
			return errors.New("text block contains incompatible fields")
		}
	case BlockToolCall:
		if strings.TrimSpace(b.ToolCallID) == "" || strings.TrimSpace(b.ToolName) == "" || b.Text != "" || b.IsError || !validJSONObject(b.Arguments) {
			return errors.New("tool-call block is malformed")
		}
	case BlockToolResult:
		if strings.TrimSpace(b.ToolCallID) == "" || strings.TrimSpace(b.ToolName) == "" || len(b.Arguments) != 0 {
			return errors.New("tool-result block is malformed")
		}
	default:
		return errors.New("unsupported message block type")
	}
	return nil
}

type Message struct {
	ID                  string           `json:"id"`
	Role                Role             `json:"role"`
	Blocks              []Block          `json:"blocks"`
	CreatedAt           time.Time        `json:"created_at"`
	FinishReason        FinishReason     `json:"finish_reason,omitempty"`
	Usage               *Usage           `json:"usage,omitempty"`
	ContextType         string           `json:"context_type,omitempty"`
	ContextTokensBefore int              `json:"context_tokens_before,omitempty"`
	Display             bool             `json:"display,omitempty"`
	ContextMetadata     *ContextMetadata `json:"context_metadata,omitempty"`
}

type ContextMetadata struct {
	TaskID string `json:"task_id,omitempty"`
}

func (m Message) Validate() error {
	switch m.Role {
	case RoleUser, RoleAssistant, RoleTool, RoleContext:
	default:
		return errors.New("unsupported message role")
	}
	if m.ContextTokensBefore < 0 {
		return errors.New("context tokens before must be nonnegative")
	}
	if m.Role != RoleContext && (m.ContextType != "" || m.ContextTokensBefore != 0 || m.Display || m.ContextMetadata != nil) {
		return errors.New("context-only fields require a context message")
	}
	if m.Usage != nil {
		if err := m.Usage.Validate(); err != nil {
			return err
		}
	}
	if m.ContextMetadata != nil {
		if err := m.ContextMetadata.Validate(); err != nil {
			return err
		}
	}
	for _, block := range m.Blocks {
		if err := block.Validate(); err != nil {
			return err
		}
	}

	switch m.Role {
	case RoleUser:
		if len(m.Blocks) == 0 {
			return errors.New("user message content is required")
		}
		if m.FinishReason != "" || m.Usage != nil {
			return errors.New("user message contains assistant-only metadata")
		}
		for _, block := range m.Blocks {
			if block.Type != BlockText {
				return errors.New("user message contains incompatible block")
			}
		}
	case RoleAssistant:
		if !validFinishReason(m.FinishReason) {
			return errors.New("unsupported assistant finish reason")
		}
		hasToolCall := false
		for _, block := range m.Blocks {
			if block.Type == BlockToolCall {
				hasToolCall = true
			}
			if block.Type != BlockText && block.Type != BlockToolCall {
				return errors.New("assistant message contains incompatible block")
			}
		}
		if hasToolCall && m.FinishReason != "" && m.FinishReason != FinishToolCalls {
			return errors.New("assistant tool calls require tool_calls finish reason")
		}
		if !hasToolCall && m.FinishReason == FinishToolCalls {
			return errors.New("tool_calls finish reason requires a tool call")
		}
	case RoleTool:
		if m.FinishReason != "" || m.Usage != nil || len(m.Blocks) == 0 {
			return errors.New("tool result message is malformed")
		}
		for _, block := range m.Blocks {
			if block.Type != BlockToolResult {
				return errors.New("tool result message is malformed")
			}
		}
	case RoleContext:
		if strings.TrimSpace(m.ContextType) == "" {
			return errors.New("context message type is required")
		}
		if m.FinishReason != "" {
			return errors.New("context message contains assistant-only metadata")
		}
		if len(m.Blocks) == 0 {
			return errors.New("context message content is required")
		}
		for _, block := range m.Blocks {
			if block.Type != BlockText {
				return errors.New("context message must contain only text blocks")
			}
		}
	}
	return nil
}

func (m ContextMetadata) Validate() error {
	if len(m.TaskID) < 2 || len(m.TaskID) > 64 || m.TaskID[0] != 't' {
		return errors.New("context task id must be a bounded generated id")
	}
	for _, r := range m.TaskID[1:] {
		if r < '0' || r > '9' {
			return errors.New("context task id must be a bounded generated id")
		}
	}
	return nil
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
	Name        string `json:"name"`
	Description string `json:"description"`
	// Parameters is provider-facing JSON Schema data. When decoded from JSON,
	// numbers are kept as json.Number so large integer constraints are not
	// rounded through float64 before being sent back to a provider.
	Parameters map[string]any `json:"parameters"`
}

func (t *ToolDefinition) UnmarshalJSON(raw []byte) error {
	type wire struct {
		Name        string          `json:"name"`
		Description string          `json:"description"`
		Parameters  json.RawMessage `json:"parameters"`
	}
	var decoded wire
	if err := json.Unmarshal(raw, &decoded); err != nil {
		return err
	}
	*t = ToolDefinition{Name: decoded.Name, Description: decoded.Description}
	if len(decoded.Parameters) == 0 || bytes.Equal(bytes.TrimSpace(decoded.Parameters), []byte("null")) {
		return nil
	}
	parameters, err := decodeJSONObjectUseNumber(decoded.Parameters)
	if err != nil {
		return fmt.Errorf("decode tool parameters: %w", err)
	}
	t.Parameters = parameters
	return nil
}

func decodeJSONObjectUseNumber(raw json.RawMessage) (map[string]any, error) {
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.UseNumber()
	var object map[string]any
	if err := decoder.Decode(&object); err != nil {
		return nil, err
	}
	if object == nil {
		return nil, errors.New("must be a JSON object")
	}
	var extra any
	if err := decoder.Decode(&extra); err == nil {
		return nil, errors.New("multiple JSON values")
	} else if !errors.Is(err, io.EOF) {
		return nil, err
	}
	return object, nil
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

func validFinishReason(reason FinishReason) bool {
	switch reason {
	case "", FinishStop, FinishToolCalls, FinishLength, FinishUnknown:
		return true
	default:
		return false
	}
}

func validJSONObject(raw json.RawMessage) bool {
	if len(raw) == 0 || !json.Valid(raw) {
		return false
	}
	var object map[string]json.RawMessage
	return json.Unmarshal(raw, &object) == nil && object != nil
}

func CloneUsage(usage *Usage) *Usage {
	if usage == nil {
		return nil
	}
	cloned := *usage
	return &cloned
}

func CloneMessage(message Message) Message {
	cloned := message
	if message.Blocks != nil {
		cloned.Blocks = make([]Block, len(message.Blocks))
		for i, block := range message.Blocks {
			cloned.Blocks[i] = block
			cloned.Blocks[i].Arguments = append(json.RawMessage(nil), block.Arguments...)
		}
	}
	cloned.Usage = CloneUsage(message.Usage)
	if message.ContextMetadata != nil {
		metadata := *message.ContextMetadata
		cloned.ContextMetadata = &metadata
	}
	return cloned
}

func CloneMessages(messages []Message) []Message {
	if messages == nil {
		return nil
	}
	cloned := make([]Message, len(messages))
	for i, message := range messages {
		cloned[i] = CloneMessage(message)
	}
	return cloned
}
