package telegram

import (
	"context"
	"encoding/json"
	"reflect"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/chat-connector/internal/agent"
)

func entsOf(t *testing.T, body map[string]any) []entity {
	t.Helper()
	raw, _ := json.Marshal(body["entities"])
	var ents []entity
	if err := json.Unmarshal(raw, &ents); err != nil {
		t.Fatal(err)
	}
	return ents
}

func TestSendMarkdownPostsTextAndEntities(t *testing.T) {
	api := botAPI(t)
	if err := newBot(api, nil).Send(context.Background(), "1", "", "hi **there**", true); err != nil {
		t.Fatal(err)
	}
	got := sends(api)
	if len(got) != 1 || got[0]["text"] != "hi there" {
		t.Fatalf("sends = %v", got)
	}
	if _, ok := got[0]["parse_mode"]; ok {
		t.Error("parse_mode must not be set")
	}
	if want := []entity{{Type: "bold", Offset: 3, Length: 5}}; !reflect.DeepEqual(entsOf(t, got[0]), want) {
		t.Errorf("entities = %v, want %v", got[0]["entities"], want)
	}
}

func TestSendMarkdownClipsEntitiesAcrossParts(t *testing.T) {
	api := botAPI(t)
	a, b := strings.Repeat("a", 3000), strings.Repeat("b", 3000)
	// Bold spans the line break that splitting drops; "z" is bold after it.
	in := "**" + a + "\n" + b + "**\n\nend **z**"
	if err := newBot(api, nil).Send(context.Background(), "1", "", in, true); err != nil {
		t.Fatal(err)
	}
	got := sends(api)
	if len(got) != 2 || got[0]["text"] != a || got[1]["text"] != b+"\n\nend z" {
		t.Fatalf("unexpected parts: %d", len(got))
	}
	if want := []entity{{Type: "bold", Offset: 0, Length: 3000}}; !reflect.DeepEqual(entsOf(t, got[0]), want) {
		t.Errorf("part 1 entities = %v", got[0]["entities"])
	}
	want := []entity{{Type: "bold", Offset: 0, Length: 3000}, {Type: "bold", Offset: 3006, Length: 1}}
	if !reflect.DeepEqual(entsOf(t, got[1]), want) {
		t.Errorf("part 2 entities = %v, want %v", got[1]["entities"], want)
	}
}

func TestSendMarkdownFallsBackToPlainOnBadRequest(t *testing.T) {
	api := newAPI(t, func(_ string, body map[string]any) resp {
		if _, ok := body["entities"]; ok {
			return errR(400, "Bad Request: can't parse entities", 0)
		}
		return okR(true)
	})
	if err := newBot(api, nil).Send(context.Background(), "1", "", "a [b](nope) **c**", true); err != nil {
		t.Fatal(err)
	}
	got := sends(api)
	if len(got) != 2 || got[0]["entities"] == nil {
		t.Fatalf("sends = %v", got)
	}
	if _, ok := got[1]["entities"]; ok || got[1]["text"] != got[0]["text"] || got[1]["text"] != "a b c" {
		t.Errorf("fallback = %v", got[1])
	}
}

func TestSendPlainKeepsTextAndOmitsEntities(t *testing.T) {
	api := botAPI(t)
	text := "* 0000000b  title **x**"
	if err := newBot(api, nil).Send(context.Background(), "1", "", text, false); err != nil {
		t.Fatal(err)
	}
	got := sends(api)
	if _, ok := got[0]["entities"]; ok || got[0]["text"] != text {
		t.Errorf("plain send = %v", got[0])
	}
}

func TestApprovalModeShortcutsAreSessionBound(t *testing.T) {
	session := strings.Repeat("a", 32)
	for _, mode := range []string{"on", "off"} {
		msg, ok := normalizeCallback(&callbackQuery{From: &user{ID: 7}, Message: &message{MessageID: 42}, Data: "auto:" + mode + ":" + session})
		if !ok || msg.ModeSession != session || msg.MessageID != "42" || msg.Text != "/auto "+mode || msg.ApprovalID != "" {
			t.Fatalf("mode callback = %+v, %v", msg, ok)
		}
	}
	if _, ok := normalizeCallback(&callbackQuery{From: &user{ID: 7}, Message: &message{}, Data: "auto:invalid:" + session}); ok {
		t.Fatal("invalid mode accepted")
	}
}

func TestApprovalModeMessageShowsModeAndOffersBothShortcuts(t *testing.T) {
	session := strings.Repeat("b", 32)
	api := newAPI(t, func(method string, body map[string]any) resp { return okR(map[string]any{"message_id": 42}) })
	bot := New(testToken, nil)
	bot.APIBase = api.srv.URL
	if err := bot.SendApprovalMode(context.Background(), "123", "", session, "auto"); err != nil {
		t.Fatal(err)
	}
	body := api.byMethod("sendMessage")[0].body
	if !strings.Contains(body["text"].(string), "AUTO") {
		t.Fatalf("mode text = %v", body)
	}
	buttons := body["reply_markup"].(map[string]any)["inline_keyboard"].([]any)[0].([]any)
	for i, mode := range []string{"on", "off"} {
		if got := buttons[i].(map[string]any)["callback_data"]; got != "auto:"+mode+":"+session {
			t.Fatalf("shortcut = %v", got)
		}
	}
}

func TestApprovalPolicyMessageShowsNeverAndInactiveReviewer(t *testing.T) {
	api := newAPI(t, func(method string, body map[string]any) resp { return okR(map[string]any{"message_id": 42}) })
	bot := New(testToken, nil)
	bot.APIBase = api.srv.URL
	policy := agent.ApprovalPolicy{Policy: "never", Reviewer: "auto_review", Mode: "auto", Automatic: true}
	if err := bot.SendApprovalPolicy(context.Background(), "123", "", strings.Repeat("b", 32), policy); err != nil {
		t.Fatal(err)
	}
	text := api.byMethod("sendMessage")[0].body["text"].(string)
	if !strings.Contains(text, "Approval policy: NEVER") || !strings.Contains(text, "auto_review (inactive)") {
		t.Fatalf("policy text = %q", text)
	}
}

func TestApprovalModeClicksShowSelectionAndRemoveButtons(t *testing.T) {
	for _, desc := range []string{"", "Bad Request: message is not modified", "Bad Request: message to edit not found"} {
		t.Run(desc, func(t *testing.T) {
			api := newAPI(t, func(method string, body map[string]any) resp {
				if desc != "" {
					return errR(400, desc, 0)
				}
				return okR(true)
			})
			bot := New(testToken, nil)
			bot.APIBase = api.srv.URL
			for _, mode := range []string{"auto", "yolo", "manual"} {
				err := bot.SendApprovalMode(context.Background(), "123", "42", strings.Repeat("b", 32), mode)
				wantError := strings.Contains(desc, "not found")
				if (err != nil) != wantError {
					t.Fatalf("edit error = %v, want error %v", err, wantError)
				}
			}
			if got := api.byMethod("sendMessage"); len(got) != 0 {
				t.Fatalf("click sent new cards: %v", got)
			}
			edits := api.byMethod("editMessageText")
			if len(edits) != 3 {
				t.Fatalf("edits = %v", edits)
			}
			for i, edit := range edits {
				mode := "AUTO"
				if i == 1 {
					mode = "YOLO"
				}
				if i == 2 {
					mode = "MANUAL"
				}
				if edit.body["message_id"] != float64(42) || edit.body["chat_id"] != float64(123) || !strings.Contains(edit.body["text"].(string), "Approval mode: "+mode) {
					t.Fatalf("edit = %v", edit.body)
				}
				buttons := edit.body["reply_markup"].(map[string]any)["inline_keyboard"].([]any)
				if len(buttons) != 0 {
					t.Fatalf("selected card still has buttons: %v", edit.body)
				}
			}
		})
	}
}
