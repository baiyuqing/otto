package model

import (
	"bytes"
	"encoding/json"
	"reflect"
	"testing"
	"time"
)

func TestMessageJSONRoundTrip(t *testing.T) {
	original := Message{
		ID:        "msg-1",
		Role:      RoleAssistant,
		CreatedAt: time.Unix(10, 0).UTC(),
		Blocks: []Block{
			{Type: BlockText, Text: "checking"},
			{Type: BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)},
		},
		FinishReason: FinishToolCalls,
		Usage:        &Usage{InputTokens: 11, OutputTokens: 7},
	}
	encoded, err := json.Marshal(original)
	if err != nil {
		t.Fatal(err)
	}
	var decoded Message
	if err := json.Unmarshal(encoded, &decoded); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(original, decoded) {
		t.Fatalf("round trip mismatch\nwant: %#v\n got: %#v", original, decoded)
	}
}

func TestContextMessageJSONRoundTrip(t *testing.T) {
	original := Message{
		ID:              "context-1",
		Role:            RoleContext,
		Blocks:          []Block{{Type: BlockText, Text: "[Custom context: fixture]\ntext"}},
		CreatedAt:       time.Unix(20, 0).UTC(),
		ContextType:     "fixture",
		Display:         true,
		ContextMetadata: &ContextMetadata{TaskID: "t1"},
	}
	encoded, err := json.Marshal(original)
	if err != nil {
		t.Fatal(err)
	}
	var decoded Message
	if err := json.Unmarshal(encoded, &decoded); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(original, decoded) {
		t.Fatalf("round trip mismatch\nwant: %#v\n got: %#v", original, decoded)
	}
}

func TestToolDefinitionParametersPreserveLargeIntegers(t *testing.T) {
	raw := []byte(`{"name":"probe","description":"","parameters":{"type":"object","properties":{"id":{"minimum":9007199254740993}}}}`)
	var decoded ToolDefinition
	if err := json.Unmarshal(raw, &decoded); err != nil {
		t.Fatal(err)
	}
	properties, ok := decoded.Parameters["properties"].(map[string]any)
	if !ok {
		t.Fatalf("properties type = %T, want map[string]any", decoded.Parameters["properties"])
	}
	id, ok := properties["id"].(map[string]any)
	if !ok {
		t.Fatalf("id type = %T, want map[string]any", properties["id"])
	}
	number, ok := id["minimum"].(json.Number)
	if !ok {
		t.Fatalf("minimum type = %T, want json.Number", id["minimum"])
	}
	if got := number.String(); got != "9007199254740993" {
		t.Fatalf("minimum = %s, want 9007199254740993", got)
	}
	encoded, err := json.Marshal(decoded)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Contains(encoded, []byte(`"minimum":9007199254740993`)) {
		t.Fatalf("encoded parameters changed JSON number: %s", encoded)
	}
}

func TestMessageTextJoinsOnlyTextBlocks(t *testing.T) {
	message := Message{Blocks: []Block{
		{Type: BlockText, Text: "one"},
		{Type: BlockToolCall, ToolName: "read"},
		{Type: BlockText, Text: "two"},
	}}
	if got := message.Text(); got != "onetwo" {
		t.Fatalf("Text() = %q, want %q", got, "onetwo")
	}
}

func TestUsageValidate(t *testing.T) {
	tests := []struct {
		name    string
		usage   Usage
		wantErr bool
	}{
		{name: "valid", usage: Usage{InputTokens: 7, OutputTokens: 2, CachedInputTokens: 3}},
		{name: "zero", usage: Usage{}},
		{name: "negative-input", usage: Usage{InputTokens: -1, OutputTokens: 1, CachedInputTokens: 0}, wantErr: true},
		{name: "negative-output", usage: Usage{InputTokens: 1, OutputTokens: -1, CachedInputTokens: 0}, wantErr: true},
		{name: "negative-cached", usage: Usage{InputTokens: 1, OutputTokens: 1, CachedInputTokens: -1}, wantErr: true},
		{name: "cached-exceeds-input", usage: Usage{InputTokens: 1, OutputTokens: 1, CachedInputTokens: 2}, wantErr: true},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			method := reflect.ValueOf(tc.usage).MethodByName("Validate")
			if !method.IsValid() {
				t.Fatal("Usage.Validate method is missing")
			}
			results := method.Call(nil)
			if len(results) != 1 {
				t.Fatalf("Validate() returned %d values, want 1", len(results))
			}
			var err error
			if !results[0].IsNil() {
				err = results[0].Interface().(error)
			}
			if tc.wantErr && err == nil {
				t.Fatalf("Validate() = nil, want error")
			}
			if !tc.wantErr && err != nil {
				t.Fatalf("Validate() = %v, want nil", err)
			}
		})
	}
}

func TestMessageValidateAllowsTransientMessagesAndRejectsInvalidShapes(t *testing.T) {
	valid := Message{Role: RoleAssistant, Blocks: nil}
	if err := valid.Validate(); err != nil {
		t.Fatalf("empty assistant message Validate() = %v", err)
	}

	tests := []Message{
		{Role: Role("future"), Blocks: []Block{{Type: BlockText, Text: "x"}}},
		{Role: RoleUser, Blocks: []Block{{Type: BlockText, ToolName: "read"}}},
		{Role: RoleAssistant, FinishReason: FinishToolCalls, Blocks: []Block{{Type: BlockText, Text: "x"}}},
		{Role: RoleAssistant, Blocks: []Block{{Type: BlockToolCall, ToolCallID: "c1", ToolName: "read", Arguments: json.RawMessage(`[]`)}}},
		{Role: RoleContext, ContextType: "task_notification", Blocks: []Block{{Type: BlockText, Text: "x"}}, ContextMetadata: &ContextMetadata{TaskID: "bad"}},
	}
	for i, message := range tests {
		if err := message.Validate(); err == nil {
			t.Fatalf("case %d Validate() = nil, want error", i)
		}
	}
	largeNumber := Message{Role: RoleAssistant, FinishReason: FinishToolCalls, Blocks: []Block{{Type: BlockToolCall, ToolCallID: "c1", ToolName: "read", Arguments: json.RawMessage(`{"n":1e400}`)}}}
	if err := largeNumber.Validate(); err != nil {
		t.Fatalf("large JSON number was rejected: %v", err)
	}
}

func TestMessageValidateAllowsContextUsageAndTransientIdentity(t *testing.T) {
	message := Message{
		Role:            RoleContext,
		Blocks:          []Block{{Type: BlockText, Text: "notification"}},
		Usage:           &Usage{},
		ContextType:     "task_notification",
		ContextMetadata: &ContextMetadata{TaskID: "t12"},
	}
	if err := message.Validate(); err != nil {
		t.Fatalf("Validate() = %v", err)
	}
}

func TestCloneMessageDeepCopiesOwnedFields(t *testing.T) {
	original := Message{
		Blocks:          []Block{{Type: BlockToolCall, Arguments: json.RawMessage(`{"x":1}`)}},
		Usage:           &Usage{InputTokens: 1},
		ContextMetadata: &ContextMetadata{TaskID: "t1"},
	}
	cloned := CloneMessage(original)
	cloned.Blocks[0].Arguments[0] = '['
	cloned.Usage.InputTokens = 9
	cloned.ContextMetadata.TaskID = "t2"
	if string(original.Blocks[0].Arguments) != `{"x":1}` || original.Usage.InputTokens != 1 || original.ContextMetadata.TaskID != "t1" {
		t.Fatalf("CloneMessage aliased owned fields: %#v", original)
	}
}
