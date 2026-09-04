package subagent

import (
	"reflect"
	"testing"

	"github.com/baiyuqing/otto/internal/model"
)

// TestInheritSnapshot covers InheritSnapshot's prefix rule: everything
// before the last assistant message (the one carrying the pending agent
// tool call(s)) is kept; that assistant message and anything appended after
// it (sibling tool results from earlier calls in the same assistant turn)
// are cut.
func TestInheritSnapshot(t *testing.T) {
	user := func(id, text string) model.Message {
		return model.Message{ID: id, Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: text}}}
	}
	assistantText := func(id, text string) model.Message {
		return model.Message{ID: id, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: text}}}
	}
	assistantToolCall := func(id, toolName string) model.Message {
		return model.Message{ID: id, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolName: toolName, ToolCallID: "call-1"}}}
	}
	toolResult := func(id string) model.Message {
		return model.Message{ID: id, Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, ToolCallID: "call-0"}}}
	}

	cases := []struct {
		name     string
		messages []model.Message
		want     []model.Message
	}{
		{
			name:     "nil messages",
			messages: nil,
			want:     nil,
		},
		{
			name:     "no assistant message",
			messages: []model.Message{user("u1", "hello")},
			want:     nil,
		},
		{
			name:     "trailing assistant only",
			messages: []model.Message{assistantToolCall("a1", "agent")},
			want:     []model.Message{},
		},
		{
			name: "assistant followed by sibling tool results",
			messages: []model.Message{
				user("u1", "hello"),
				assistantToolCall("a1", "agent"),
				toolResult("tr1"),
			},
			want: []model.Message{user("u1", "hello")},
		},
		{
			name: "earlier complete exchanges kept",
			messages: []model.Message{
				user("u1", "first"),
				assistantText("a1", "reply one"),
				user("u2", "second"),
				assistantToolCall("a2", "agent"),
			},
			want: []model.Message{
				user("u1", "first"),
				assistantText("a1", "reply one"),
				user("u2", "second"),
			},
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := InheritSnapshot(tc.messages)
			if tc.want == nil {
				if got != nil {
					t.Fatalf("got %+v, want nil", got)
				}
				return
			}
			if !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("got %+v, want %+v", got, tc.want)
			}
		})
	}
}

// InheritSnapshot must return a copy: mutating the input slice afterward
// must not change the returned snapshot.
func TestInheritSnapshotReturnsACopy(t *testing.T) {
	messages := []model.Message{
		{ID: "u1", Role: model.RoleUser},
		{ID: "a1", Role: model.RoleAssistant},
	}
	got := InheritSnapshot(messages)
	messages[0].ID = "mutated"
	if got[0].ID != "u1" {
		t.Fatalf("snapshot shares backing array with input: got[0].ID = %q", got[0].ID)
	}
}
