package agent

import (
	"encoding/json"
	"math"
	"testing"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/session"
)

func TestEstimateStringUsesUTF8CeilingTable(t *testing.T) {
	tests := []struct {
		name  string
		value string
		want  int
	}{
		{name: "empty", value: "", want: 0},
		{name: "ascii exact three", value: "abc", want: 1},
		{name: "ascii rounds up", value: "abcd", want: 2},
		{name: "cjk", value: "你好", want: 2},
		{name: "emoji", value: "🙂", want: 2},
		{name: "code", value: "func main() {\n\tprintln(\"hi\")\n}\n", want: estimateStringFormula("func main() {\n\tprintln(\"hi\")\n}\n")},
	}
	for _, test := range tests {
		if got := estimateString(test.value); got != test.want {
			t.Fatalf("estimateString(%q) = %d, want %d", test.value, got, test.want)
		}
	}
}

func TestEstimateMessageUsesExactFraming(t *testing.T) {
	tests := []struct {
		name    string
		message model.Message
		want    int
	}{
		{
			name: "user text",
			message: model.Message{Role: model.RoleUser, Blocks: []model.Block{{
				Type: model.BlockText,
				Text: "hello",
			}}},
			want: 6 + 2 + estimateStringFormula("hello"),
		},
		{
			name: "assistant text and tool call",
			message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{
				{Type: model.BlockText, Text: "done"},
				{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)},
			}},
			want: 6 +
				(2 + estimateStringFormula("done")) +
				(12 + estimateStringFormula("call-1") + estimateStringFormula("read") + estimateStringFormula(`{"path":"README.md"}`)),
		},
		{
			name: "tool result",
			message: model.Message{Role: model.RoleTool, Blocks: []model.Block{{
				Type:       model.BlockToolResult,
				Text:       "contents",
				ToolCallID: "call-1",
				ToolName:   "read",
				IsError:    true,
			}}},
			want: 6 + 8 + estimateStringFormula("contents") + estimateStringFormula("call-1") + estimateStringFormula("read"),
		},
		{
			name: "context summary",
			message: model.Message{Role: model.RoleContext, Blocks: []model.Block{{
				Type: model.BlockText,
				Text: "[Compaction summary]\nkeep this",
			}}},
			want: 6 + 2 + estimateStringFormula("[Compaction summary]\nkeep this"),
		},
	}
	for _, test := range tests {
		if got := estimateMessage(test.message); got != test.want {
			t.Fatalf("%s: estimateMessage() = %d, want %d", test.name, got, test.want)
		}
	}
}

func TestEstimateRequestFallsBackToStableSystemMessagesAndTools(t *testing.T) {
	parametersA := map[string]any{
		"type": "object",
		"properties": map[string]any{
			"path": map[string]any{"type": "string"},
			"mode": map[string]any{"enum": []string{"r", "w"}},
		},
		"required": []string{"path"},
	}
	parametersB := map[string]any{
		"required": []string{"path"},
		"properties": map[string]any{
			"mode": map[string]any{"enum": []string{"r", "w"}},
			"path": map[string]any{"type": "string"},
		},
		"type": "object",
	}
	requestA := provider.Request{
		SystemPrompt: "system prompt",
		Messages: []model.Message{
			{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "inspect src"}}},
			{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "working"}}},
		},
		Tools: []model.ToolDefinition{{
			Name:        "read",
			Description: "Read a file",
			Parameters:  parametersA,
		}},
	}
	requestB := provider.Request{
		SystemPrompt: requestA.SystemPrompt,
		Messages:     requestA.Messages,
		Tools: []model.ToolDefinition{{
			Name:        "read",
			Description: "Read a file",
			Parameters:  parametersB,
		}},
	}

	schema, err := json.Marshal(parametersA)
	if err != nil {
		t.Fatal(err)
	}
	want := 3 +
		estimateStringFormula("system prompt") +
		(6 + 2 + estimateStringFormula("inspect src")) +
		(6 + 2 + estimateStringFormula("working")) +
		(16 + estimateStringFormula("read") + estimateStringFormula("Read a file") + estimateStringFormula(string(schema)))

	if got := estimateRequest(requestA, session.CompactionMetadata{}, false); got != want {
		t.Fatalf("estimateRequest(requestA) = %d, want %d", got, want)
	}
	if got := estimateRequest(requestB, session.CompactionMetadata{}, false); got != want {
		t.Fatalf("estimateRequest(requestB) = %d, want %d", got, want)
	}
}

func TestEstimateRequestUsesPromptUsageAnchorWithoutCachedDoubleCount(t *testing.T) {
	request := provider.Request{Messages: []model.Message{
		{ID: "a", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "abc"}}, Usage: &model.Usage{InputTokens: 100, OutputTokens: 999, CachedInputTokens: 80}},
		{ID: "b", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "next"}}},
	}}
	want := 100 + estimateMessage(request.Messages[0]) + estimateMessage(request.Messages[1])
	if got := estimateRequest(request, session.CompactionMetadata{}, false); got != want {
		t.Fatalf("estimateRequest() = %d, want %d", got, want)
	}
}

func TestEstimateRequestIgnoresContextUsageAnchors(t *testing.T) {
	request := provider.Request{
		SystemPrompt: "system",
		Messages: []model.Message{
			{Role: model.RoleContext, Blocks: []model.Block{{Type: model.BlockText, Text: "[Compaction summary]\nsummary"}}, Usage: &model.Usage{InputTokens: 400, CachedInputTokens: 200}},
			{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "next"}}},
		},
	}
	want := 3 + estimateStringFormula("system") + estimateMessageFormula(request.Messages[0]) + estimateMessageFormula(request.Messages[1])
	if got := estimateRequest(request, session.CompactionMetadata{}, false); got != want {
		t.Fatalf("estimateRequest() = %d, want %d", got, want)
	}
}

func TestEstimateRequestWithLatestCheckpointAndNoPostCheckpointMessagePermitsNoAnchor(t *testing.T) {
	request := provider.Request{
		SystemPrompt: "system",
		Messages: []model.Message{
			{ID: "a", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "retained"}}, Usage: &model.Usage{InputTokens: 250, CachedInputTokens: 180}},
			{ID: "b", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "next"}}},
		},
	}
	want := 3 + estimateStringFormula("system") + estimateMessageFormula(request.Messages[0]) + estimateMessageFormula(request.Messages[1])
	if got := estimateRequest(request, session.CompactionMetadata{}, true); got != want {
		t.Fatalf("estimateRequest() = %d, want %d", got, want)
	}
}

func TestEstimateRequestUsesPostCheckpointAssistantAnchorAndIgnoresRetainedPreCheckpointUsage(t *testing.T) {
	request := provider.Request{Messages: []model.Message{
		{ID: "old-assistant", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "retained tail"}}, Usage: &model.Usage{InputTokens: 500, CachedInputTokens: 300}},
		{ID: "post-user", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "continue"}}},
		{ID: "post-assistant", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "current"}}, Usage: &model.Usage{InputTokens: 70, CachedInputTokens: 20}},
		{ID: "tail", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "next"}}},
	}}
	latest := session.CompactionMetadata{FirstPostCheckpointMessageID: "post-user"}
	want := 70 + estimateMessage(request.Messages[2]) + estimateMessage(request.Messages[3])
	if got := estimateRequest(request, latest, true); got != want {
		t.Fatalf("estimateRequest() = %d, want %d", got, want)
	}
}

func TestEstimateRequestFallsBackWhenCheckpointFloorHasNoEligibleAssistantAnchor(t *testing.T) {
	request := provider.Request{
		SystemPrompt: "system",
		Messages: []model.Message{
			{ID: "old-assistant", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "retained tail"}}, Usage: &model.Usage{InputTokens: 500, CachedInputTokens: 300}},
			{ID: "post-user", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "continue"}}},
			{ID: "tail", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "next"}}},
		},
	}
	latest := session.CompactionMetadata{FirstPostCheckpointMessageID: "post-user"}
	want := 3 + estimateStringFormula("system") + estimateMessageFormula(request.Messages[0]) + estimateMessageFormula(request.Messages[1]) + estimateMessageFormula(request.Messages[2])
	if got := estimateRequest(request, latest, true); got != want {
		t.Fatalf("estimateRequest() = %d, want %d", got, want)
	}
}

func TestEstimateRequestSaturatesAtMaxInt(t *testing.T) {
	request := provider.Request{Messages: []model.Message{
		{ID: "a", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "abc"}}, Usage: &model.Usage{InputTokens: math.MaxInt, CachedInputTokens: math.MaxInt}},
		{ID: "b", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "next"}}},
	}}
	if got := estimateRequest(request, session.CompactionMetadata{}, false); got != math.MaxInt {
		t.Fatalf("estimateRequest() = %d, want %d", got, math.MaxInt)
	}
}

func estimateStringFormula(value string) int {
	return (len([]byte(value)) + 2) / 3
}

func estimateMessageFormula(message model.Message) int {
	want := 6
	for _, block := range message.Blocks {
		switch block.Type {
		case model.BlockText:
			want += 2 + estimateStringFormula(block.Text)
		case model.BlockToolCall:
			want += 12 + estimateStringFormula(block.ToolCallID) + estimateStringFormula(block.ToolName) + estimateStringFormula(string(block.Arguments))
		case model.BlockToolResult:
			want += 8 + estimateStringFormula(block.Text) + estimateStringFormula(block.ToolCallID) + estimateStringFormula(block.ToolName)
		}
	}
	return want
}
