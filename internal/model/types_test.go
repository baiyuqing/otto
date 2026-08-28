package model

import (
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
		ID:          "context-1",
		Role:        RoleContext,
		Blocks:      []Block{{Type: BlockText, Text: "[Custom context: fixture]\ntext"}},
		CreatedAt:   time.Unix(20, 0).UTC(),
		ContextType: "fixture",
		Display:     true,
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
