package bridge

import (
	"strings"
	"testing"
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
