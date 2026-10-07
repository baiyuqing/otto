package bridge

import (
	"context"
	"strings"
	"testing"
	"time"
)

func TestAutoCommandUsesSessionAPIAndSurvivesReload(t *testing.T) {
	h := newHarness(t, setup{})
	h.say("hello")
	h.waitSent(1)
	h.say("/auto off")
	h.waitFor(func() bool { return strings.Contains(strings.Join(h.plat.texts(), "\n"), "Approval mode: MANUAL") }, "manual mode")
	sid := h.b.opts.Store.Session("fake:c1")
	h.say("/auto")
	h.waitSent(3)
	if sid == "" || !strings.Contains(h.plat.texts()[2], "MANUAL") {
		t.Fatalf("mode query = %q", h.plat.texts())
	}
	h.say("/auto on")
	h.waitFor(func() bool { return strings.Contains(strings.Join(h.plat.texts(), "\n"), "Approval mode: AUTO") }, "auto mode")
	h.say("/auto nope")
	h.waitFor(func() bool { return strings.Contains(strings.Join(h.plat.texts(), "\n"), "Usage: /auto") }, "invalid mode")
	h.b.deliver(Message{Platform: "fake", ChatID: "c1", SenderID: "u1", ModeSession: "old-session", Text: "/auto off"})
	h.waitFor(func() bool { return strings.Contains(strings.Join(h.plat.texts(), "\n"), "another session") }, "stale shortcut")
}

type modeCall struct {
	chat, message, session string
	mode                   string
}

type modePlatform struct {
	Platform
	calls chan modeCall
}

func (p modePlatform) SendApprovalMode(_ context.Context, chat, message, session, mode string) error {
	p.calls <- modeCall{chat, message, session, mode}
	return nil
}

func TestAutoShortcutUpdatesClickedCardAndSessionMode(t *testing.T) {
	calls := make(chan modeCall, 1)
	h := newHarness(t, setup{mod: func(o *Options) {
		o.Platforms = []Platform{modePlatform{o.Platforms[0], calls}}
	}})
	var sid string
	for i, command := range []string{"/auto", "/auto on", "/auto on", "/auto off"} {
		m := h.msg("c1", "u1", command)
		wantID := ""
		if i > 0 {
			m.ModeSession, m.MessageID = sid, "42"
			wantID = "42"
		}
		h.b.deliver(m)
		select {
		case call := <-calls:
			want := "auto"
			if i == 3 {
				want = "manual"
			}
			if call.chat != "c1" || call.message != wantID || call.mode != want {
				t.Fatalf("mode presentation = %+v", call)
			}
			if i == 0 {
				sid = h.store.Session("fake:c1")
			}
			if sid == "" || call.session != sid {
				t.Fatalf("session = %q, call = %+v", sid, call)
			}
			mode, err := h.agent.ApprovalMode(context.Background(), sid, nil)
			if err != nil || mode != call.mode {
				t.Fatalf("stored mode = %v, error = %v", mode, err)
			}
		case <-time.After(5 * time.Second):
			t.Fatal("mode presentation timed out")
		}
	}
	if got := h.plat.texts(); len(got) != 0 {
		t.Fatalf("unexpected extra messages: %v", got)
	}
}
