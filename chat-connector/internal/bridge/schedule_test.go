package bridge

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"
)

func deliver(t *testing.T, h *harness, params string) (any, error) {
	t.Helper()
	return h.b.Extension(context.Background(), "_otto/schedule/deliver", json.RawMessage(params))
}

func TestScheduleDeliverSendsMarkdownToAllowedChat(t *testing.T) {
	h := newHarness(t, setup{platform: "telegram"})
	got, err := deliver(t, h, `{"chat":"telegram:c1","text":"**hi**"}`)
	if err != nil {
		t.Fatal(err)
	}
	if m, ok := got.(map[string]any); !ok || len(m) != 0 {
		t.Fatalf("result = %#v, want empty object", got)
	}
	h.plat.mu.Lock()
	defer h.plat.mu.Unlock()
	if len(h.plat.sent) != 1 || h.plat.sent[0] != (sent{"c1", "", "**hi**", true}) {
		t.Fatalf("sent = %#v", h.plat.sent)
	}
}

func TestScheduleDeliverRefusals(t *testing.T) {
	h := newHarness(t, setup{platform: "telegram", chats: []string{"c1"}, senders: []string{"u1"}})
	for _, params := range []string{
		`{"chat":"telegram:c2","text":"x"}`, // chat not in allowlist
		`{"chat":"feishu:c1","text":"x"}`,   // platform not configured
		`{"chat":"telegram:","text":"x"}`,   // empty chat id
		`{"chat":"c1","text":"x"}`,          // no platform
		`{"chat":"telegram:c1","text":""}`,  // empty text
		`{"chat":"","text":"x"}`,            // empty chat
		`{"text":"x"}`,                      // missing chat
		`{"chat":"telegram:c1","text":"x","extra":1}`,
		`not json`,
	} {
		if _, err := deliver(t, h, params); err == nil {
			t.Errorf("%s: want an error", params)
		}
	}
	if got := h.plat.texts(); len(got) != 0 {
		t.Fatalf("sent = %q, want nothing", got)
	}
}

func TestScheduleDeliverReturnsSendError(t *testing.T) {
	h := newHarness(t, setup{platform: "telegram"})
	h.plat.mu.Lock()
	h.plat.sendErr = errors.New("send boom")
	h.plat.mu.Unlock()
	if _, err := deliver(t, h, `{"chat":"telegram:c1","text":"x"}`); err == nil || !strings.Contains(err.Error(), "send boom") {
		t.Fatalf("err = %v, want the send error", err)
	}
}

func TestUnknownExtensionMethod(t *testing.T) {
	h := newHarness(t, setup{})
	if _, err := h.b.Extension(context.Background(), "_otto/other", nil); err == nil {
		t.Fatal("want method-not-found")
	}
}

func TestInitializeAndPromptCarryScheduleMetadata(t *testing.T) {
	h := newHarness(t, setup{platform: "telegram"})
	h.say("hello")
	h.waitSent(1)
	if got := h.calls("schedule_delivery:"); len(got) != 1 || got[0] != `schedule_delivery:["telegram"]` {
		t.Fatalf("initialize meta calls = %q", got)
	}
	if got := h.calls("chat:"); len(got) != 1 || got[0] != "chat:telegram:c1" {
		t.Fatalf("prompt chat calls = %q", got)
	}
}

func TestAgentDeliversThroughConnection(t *testing.T) {
	h := newHarness(t, setup{platform: "telegram"})
	h.say(`ext:_otto/schedule/deliver {"chat":"telegram:c1","text":"due"}`)
	h.waitCall("ext_result:{}")
	h.waitFor(func() bool { return len(h.plat.texts()) > 0 && h.plat.texts()[0] == "due" }, "delivered text")
	h.waitCall("end:ext:_otto/schedule/deliver")
	h.say(`ext:_otto/schedule/deliver {"chat":"telegram:zz","text":"no"}`)
	if c := h.waitCall("ext_result:error:"); !strings.Contains(c, "allowed") {
		t.Fatalf("call = %q", c)
	}
}

func TestConnectsAtStartupAndReconnectsAfterExit(t *testing.T) {
	store := newStore(t)
	h := newHarness(t, setup{platform: "telegram", store: store})
	h.waitCall("init:") // connected without any chat message
	h.say("hello")
	h.waitSent(1)
	h.agent.Kill()
	h.waitFor(func() bool { return len(h.calls("init:")) == 2 }, "reconnect")
	h.waitFor(func() bool { return len(h.calls("load:")) >= 1 }, "bound session reload")
}
