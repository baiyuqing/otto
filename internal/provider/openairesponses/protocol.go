package openairesponses

import (
	"encoding/json"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
)

// Request/response wire structs for the ChatGPT backend Responses API. Kept in
// this package so no Responses-specific shape leaks out of the provider.

type responsesRequest struct {
	Model        string              `json:"model"`
	Instructions string              `json:"instructions,omitempty"`
	Input        []responsesItem     `json:"input"`
	Tools        []responsesTool     `json:"tools,omitempty"`
	Reasoning    *responsesReasoning `json:"reasoning,omitempty"`
	Stream       bool                `json:"stream"`
	Store        bool                `json:"store"`
}

type responsesReasoning struct {
	Effort string `json:"effort"`
}

type responsesTool struct {
	Type        string         `json:"type"`
	Name        string         `json:"name"`
	Description string         `json:"description,omitempty"`
	Parameters  map[string]any `json:"parameters"`
}

// responsesItem covers every input item shape (message, function_call,
// function_call_output); omitempty keeps unused fields out of the wire form.
type responsesItem struct {
	Type      string             `json:"type"`
	Role      string             `json:"role,omitempty"`
	Content   []responsesContent `json:"content,omitempty"`
	CallID    string             `json:"call_id,omitempty"`
	Name      string             `json:"name,omitempty"`
	Arguments string             `json:"arguments,omitempty"`
	Output    string             `json:"output,omitempty"`
}

type responsesContent struct {
	Type string `json:"type"`
	Text string `json:"text"`
}

func translateRequest(request provider.Request) responsesRequest {
	translated := responsesRequest{
		Model:        request.Model,
		Instructions: request.SystemPrompt,
		Stream:       true,
		Store:        false,
	}
	if request.Thinking != "" {
		translated.Reasoning = &responsesReasoning{Effort: request.Thinking}
	}
	for _, message := range request.Messages {
		switch message.Role {
		case model.RoleUser, model.RoleContext:
			translated.Input = append(translated.Input, responsesItem{
				Type:    "message",
				Role:    "user",
				Content: []responsesContent{{Type: "input_text", Text: message.Text()}},
			})
		case model.RoleAssistant:
			if text := message.Text(); text != "" {
				translated.Input = append(translated.Input, responsesItem{
					Type:    "message",
					Role:    "assistant",
					Content: []responsesContent{{Type: "output_text", Text: text}},
				})
			}
			for _, block := range message.Blocks {
				if block.Type == model.BlockToolCall {
					translated.Input = append(translated.Input, responsesItem{
						Type:      "function_call",
						CallID:    block.ToolCallID,
						Name:      block.ToolName,
						Arguments: string(block.Arguments),
					})
				}
			}
		case model.RoleTool:
			for _, block := range message.Blocks {
				if block.Type == model.BlockToolResult {
					translated.Input = append(translated.Input, responsesItem{
						Type:   "function_call_output",
						CallID: block.ToolCallID,
						Output: block.Text,
					})
				}
			}
		}
	}
	for _, tool := range request.Tools {
		translated.Tools = append(translated.Tools, responsesTool{
			Type:        "function",
			Name:        tool.Name,
			Description: tool.Description,
			Parameters:  tool.Parameters,
		})
	}
	return translated
}

// Streaming event payloads. The SSE `data:` object carries a `type` field that
// duplicates the SSE `event:` line, so parsing switches on it directly.

type responsesEvent struct {
	Type     string            `json:"type"`
	Delta    string            `json:"delta"`
	ItemID   string            `json:"item_id"`
	Item     *responsesOutItem `json:"item"`
	Response *responsesResult  `json:"response"`
}

type responsesOutItem struct {
	Type      string `json:"type"`
	ID        string `json:"id"`
	CallID    string `json:"call_id"`
	Name      string `json:"name"`
	Arguments string `json:"arguments"`
}

type responsesResult struct {
	Status            string               `json:"status"`
	Usage             *responsesUsage      `json:"usage"`
	IncompleteDetails *responsesIncomplete `json:"incomplete_details"`
}

type responsesIncomplete struct {
	Reason string `json:"reason"`
}

type responsesUsage struct {
	InputTokens        int `json:"input_tokens"`
	OutputTokens       int `json:"output_tokens"`
	InputTokensDetails *struct {
		CachedTokens int `json:"cached_tokens"`
	} `json:"input_tokens_details"`
}

func (u responsesUsage) cachedTokens() int {
	if u.InputTokensDetails != nil {
		return u.InputTokensDetails.CachedTokens
	}
	return 0
}

func validArguments(arguments string) bool {
	return json.Valid([]byte(arguments))
}
