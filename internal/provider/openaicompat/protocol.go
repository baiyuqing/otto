package openaicompat

import (
	"encoding/json"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
)

type chatRequest struct {
	Model         string        `json:"model"`
	Messages      []chatMessage `json:"messages"`
	Tools         []chatTool    `json:"tools,omitempty"`
	Stream        bool          `json:"stream"`
	StreamOptions streamOptions `json:"stream_options"`
}

type streamOptions struct {
	IncludeUsage bool `json:"include_usage"`
}

type chatMessage struct {
	Role       string         `json:"role"`
	Content    string         `json:"content,omitempty"`
	ToolCalls  []chatToolCall `json:"tool_calls,omitempty"`
	ToolCallID string         `json:"tool_call_id,omitempty"`
}

type chatTool struct {
	Type     string       `json:"type"`
	Function chatFunction `json:"function"`
}

type chatFunction struct {
	Name        string         `json:"name"`
	Description string         `json:"description"`
	Parameters  map[string]any `json:"parameters"`
}

type chatToolCall struct {
	Index    int                  `json:"index,omitempty"`
	ID       string               `json:"id,omitempty"`
	Type     string               `json:"type,omitempty"`
	Function chatToolCallFunction `json:"function"`
}

type chatToolCallFunction struct {
	Name      string `json:"name,omitempty"`
	Arguments string `json:"arguments,omitempty"`
}

type chatChunk struct {
	Choices []chatChoice `json:"choices"`
	Usage   *chatUsage   `json:"usage,omitempty"`
}

type chatChoice struct {
	Delta        chatDelta `json:"delta"`
	FinishReason string    `json:"finish_reason"`
}

type chatDelta struct {
	Content   string         `json:"content"`
	ToolCalls []chatToolCall `json:"tool_calls"`
}

type chatUsage struct {
	PromptTokens     int `json:"prompt_tokens"`
	CompletionTokens int `json:"completion_tokens"`
}

func translateRequest(request provider.Request) chatRequest {
	translated := chatRequest{
		Model:         request.Model,
		Stream:        true,
		StreamOptions: streamOptions{IncludeUsage: true},
	}
	if request.SystemPrompt != "" {
		translated.Messages = append(translated.Messages, chatMessage{Role: "system", Content: request.SystemPrompt})
	}
	for _, message := range request.Messages {
		switch message.Role {
		case model.RoleUser:
			translated.Messages = append(translated.Messages, chatMessage{Role: "user", Content: message.Text()})
		case model.RoleAssistant:
			wire := chatMessage{Role: "assistant", Content: message.Text()}
			for _, block := range message.Blocks {
				if block.Type == model.BlockToolCall {
					wire.ToolCalls = append(wire.ToolCalls, chatToolCall{
						ID:   block.ToolCallID,
						Type: "function",
						Function: chatToolCallFunction{
							Name:      block.ToolName,
							Arguments: string(block.Arguments),
						},
					})
				}
			}
			translated.Messages = append(translated.Messages, wire)
		case model.RoleTool:
			for _, block := range message.Blocks {
				if block.Type == model.BlockToolResult {
					translated.Messages = append(translated.Messages, chatMessage{Role: "tool", Content: block.Text, ToolCallID: block.ToolCallID})
				}
			}
		}
	}
	for _, tool := range request.Tools {
		translated.Tools = append(translated.Tools, chatTool{
			Type: "function",
			Function: chatFunction{
				Name:        tool.Name,
				Description: tool.Description,
				Parameters:  tool.Parameters,
			},
		})
	}
	return translated
}

func finishReason(reason string) model.FinishReason {
	switch reason {
	case "stop":
		return model.FinishStop
	case "tool_calls":
		return model.FinishToolCalls
	case "length":
		return model.FinishLength
	default:
		return model.FinishUnknown
	}
}

func validArguments(arguments string) bool {
	return json.Valid([]byte(arguments))
}
