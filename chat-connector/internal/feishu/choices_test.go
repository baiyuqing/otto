package feishu

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/baiyuqing/otto/chat-connector/internal/bridge"
	"github.com/larksuite/channel-sdk-go/types"
)

func TestChoicesCardSwitchesModels(t *testing.T) {
	f := newFake()
	p := &Platform{ch: f}
	choices := []bridge.Choice{
		{Label: "chatgpt/gpt-5.6-terra", Command: "/model chatgpt/gpt-5.6-terra"},
		{Label: "chatgpt/gpt-5.5", Command: "/model chatgpt/gpt-5.5"},
	}
	if err := p.SendChoices(context.Background(), "oc_test", "", "Configured models:\n• chatgpt/gpt-5.6-terra", choices); err != nil {
		t.Fatal(err)
	}
	var card map[string]any
	if err := json.Unmarshal([]byte(f.sent[0].Card), &card); err != nil {
		t.Fatal(err)
	}
	elements := card["elements"].([]any)
	if elements[0].(map[string]any)["text"].(map[string]any)["content"] != "Configured models:\n• chatgpt/gpt-5.6-terra" {
		t.Fatalf("card = %v", elements[0])
	}
	actions := elements[1].(map[string]any)["actions"].([]any)
	if len(actions) != len(choices) {
		t.Fatalf("actions = %+v", actions)
	}
	for i, want := range choices {
		button := actions[i].(map[string]any)
		if button["text"].(map[string]any)["content"] != want.Label {
			t.Errorf("button %d label = %v, want %q", i, button["text"], want.Label)
		}
		m, ok := cardMessage(&types.CardActionEvent{
			ChatID:   "oc_test",
			Operator: types.CardActionOperator{OpenID: "ou_test"},
			Action:   types.CardActionPayload{Value: button["value"].(map[string]any)},
		})
		if !ok || m.Text != want.Command || m.SenderID != "ou_test" || m.ChatID != "oc_test" {
			t.Errorf("callback for %q = %+v, %v", want.Label, m, ok)
		}
	}
}

func TestChoiceCardActionsAcceptOnlyModelSwitches(t *testing.T) {
	for _, value := range []map[string]any{
		{"action": "command", "command": "rm -rf /"},
		{"action": "command", "command": "/model a b"},
		{"action": "command", "command": "/model terra"},
		{"action": "command", "command": "/stop"},
		{"action": "command"},
	} {
		event := &types.CardActionEvent{
			ChatID:   "oc_test",
			Operator: types.CardActionOperator{OpenID: "ou_test"},
			Action:   types.CardActionPayload{Value: value},
		}
		if _, ok := cardMessage(event); ok {
			t.Errorf("%v was accepted as a choice action", value)
		}
	}
	if _, ok := cardMessage(&types.CardActionEvent{
		Operator: types.CardActionOperator{OpenID: "ou_test"},
		Action:   types.CardActionPayload{Value: map[string]any{"action": "command", "command": "/model chatgpt/gpt-5.6-terra"}},
	}); ok {
		t.Error("a choice without a chat was accepted")
	}
}

func TestSendChoicesRefusesAnEmptyList(t *testing.T) {
	p := &Platform{ch: newFake()}
	if err := p.SendChoices(context.Background(), "oc_test", "", "text", nil); err == nil {
		t.Error("an empty choice list must be an error so the caller can fall back to text")
	}
}
