package telegram

import (
	"context"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/chat-connector/internal/bridge"
)

// callbackFor builds the callback a tapped button would send.
func callbackFor(data string) *callbackQuery {
	msg := &message{MessageID: 9}
	msg.Chat.ID = 123
	return &callbackQuery{ID: "q1", From: &user{ID: 7}, Message: msg, Data: data}
}

func TestChoiceButtonsDeliverTheirCommand(t *testing.T) {
	api := newAPI(t, func(string, map[string]any) resp { return okR(map[string]any{"message_id": 7}) })
	b := New(testToken, nil)
	b.APIBase = api.srv.URL
	choices := []bridge.Choice{
		{Label: "deepseek", Command: "/model deepseek"},
		{Label: "terra", Command: "/model terra"},
		{Label: "chatgpt-5.5", Command: "/model chatgpt-5.5"},
	}
	if err := b.SendChoices(context.Background(), "123", "9", "Profiles:\n• terra", choices); err != nil {
		t.Fatal(err)
	}
	call := api.byMethod("sendMessage")[0].body
	if call["chat_id"] != float64(123) || call["text"] != "Profiles:\n• terra" {
		t.Fatalf("sendMessage = %+v", call)
	}
	reply := call["reply_parameters"].(map[string]any)
	if reply["message_id"] != float64(9) || reply["allow_sending_without_reply"] != true {
		t.Fatalf("reply_parameters = %+v", reply)
	}
	rows := call["reply_markup"].(map[string]any)["inline_keyboard"].([]any)
	if len(rows) != 2 || len(rows[0].([]any)) != 2 || len(rows[1].([]any)) != 1 {
		t.Fatalf("rows = %+v, want two per row", rows)
	}
	for i, want := range choices {
		button := rows[i/2].([]any)[i%2].(map[string]any)
		if button["text"] != want.Label {
			t.Errorf("button %d label = %v, want %q", i, button["text"], want.Label)
		}
		m, ok := normalizeCallback(callbackFor(button["callback_data"].(string)))
		if !ok || m.Text != want.Command || m.ChatID != "123" || m.SenderID != "7" || m.MessageID != "9" {
			t.Errorf("callback for %q = %+v, %v", want.Label, m, ok)
		}
	}
}

func TestChoiceCallbacksAcceptOnlyProfileSwitches(t *testing.T) {
	for _, data := range []string{
		"cmd:rm -rf /",
		"cmd:/model a b",
		"cmd:/model a/b",
		"cmd:/model ",
		"cmd:/stop",
		"cmd:/model " + strings.Repeat("x", 65),
	} {
		if _, ok := normalizeCallback(callbackFor(data)); ok {
			t.Errorf("%q was accepted as a choice callback", data)
		}
	}
	if _, ok := normalizeCallback(&callbackQuery{Data: "cmd:/model terra"}); ok {
		t.Error("a callback without a sender and message was accepted")
	}
}

func TestSendChoicesRefusesWhatTelegramCannotCarry(t *testing.T) {
	api := newAPI(t, func(string, map[string]any) resp { return okR(map[string]any{"message_id": 7}) })
	b := New(testToken, nil)
	b.APIBase = api.srv.URL
	ctx := context.Background()
	if err := b.SendChoices(ctx, "123", "", "text", nil); err == nil {
		t.Error("an empty choice list must be an error so the caller can fall back to text")
	}
	long := bridge.Choice{Label: "x", Command: "/model " + strings.Repeat("x", 64)}
	if err := b.SendChoices(ctx, "123", "", "text", []bridge.Choice{long}); err == nil {
		t.Error("a choice whose callback exceeds 64 bytes must be an error")
	}
	tooLong := strings.Repeat("x", maxUnits+1)
	if err := b.SendChoices(ctx, "123", "", tooLong, []bridge.Choice{{Label: "a", Command: "/model a"}}); err == nil {
		t.Error("text longer than one message must be an error")
	}
	if n := len(api.byMethod("sendMessage")); n != 0 {
		t.Errorf("sendMessage called %d times for refused choices", n)
	}
}

func TestSendChoicesReportsSendFailures(t *testing.T) {
	api := newAPI(t, func(string, map[string]any) resp { return errR(400, "bad request", 0) })
	b := New(testToken, nil)
	b.APIBase = api.srv.URL
	if err := b.SendChoices(context.Background(), "123", "", "text", []bridge.Choice{{Label: "a", Command: "/model a"}}); err == nil {
		t.Fatal("a rejected sendMessage must be an error")
	}
}
