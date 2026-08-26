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
