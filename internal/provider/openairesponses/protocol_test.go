package openairesponses

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
)

// The Responses API requires every function_call_output item to carry an
// `output` field, even when the tool produced no text. omitempty used to drop
// it, yielding HTTP 400 "Missing required parameter: 'input[N].output'".
func TestTranslateRequestEmptyToolResultKeepsOutput(t *testing.T) {
	request := provider.Request{
		Model: "gpt-5",
		Messages: []model.Message{
			{
				Role: model.RoleTool,
				Blocks: []model.Block{
					{Type: model.BlockToolResult, ToolCallID: "call_1", Text: ""},
				},
			},
		},
	}

	translated := translateRequest(request)
	raw, err := json.Marshal(translated.Input[0])
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if !strings.Contains(string(raw), `"output"`) {
		t.Fatalf("function_call_output missing output field: %s", raw)
	}
}
