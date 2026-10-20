package bridge

import (
	"fmt"
	"strings"
	"testing"
)

func TestApprovalButtonsBoundToRequest(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("perm:first")
	h.waitSent(1)
	first := h.plat.approvalID()
	click := func(chat, sender, id, text string) {
		m := h.msg(chat, sender, text)
		m.ApprovalID = id
		h.plat.deliver(m)
	}
	click("c1", "intruder", first, "/allow")
	click("c2", "u1", first, "/allow")
	click("c1", "u1", "wrong", "/allow")
	settle()
	if len(h.calls("perm:")) != 0 {
		t.Fatal("invalid callback answered request")
	}
	click("c1", "u1", first, "/allow")
	h.waitCall("perm:")
	h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.statuses) == 1 }, "card completion")
	h.say("perm:second")
	h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.approvals) == 2 }, "second card")
	second := h.plat.approvalID()
	if first == second {
		t.Fatal("request ID reused")
	}
	click("c1", "u1", first, "/allow")
	settle()
	if len(h.calls("perm:")) != 1 {
		t.Fatal("old card answered new request")
	}
	click("c1", "u1", second, "/deny")
	h.waitFor(func() bool { return len(h.calls("perm:")) == 2 }, "deny")
	if got := h.calls("perm:"); got[0] != "perm:allow_once" || got[1] != "perm:reject_once" {
		t.Fatal(got)
	}
	click("c1", "u1", second, "/allow")
	settle()
	if len(h.calls("perm:")) != 2 {
		t.Fatal("duplicate callback reached agent")
	}
}

func TestApprovalCardFailureFallsBackToCommands(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.plat.mu.Lock()
	h.plat.cardErr = fmt.Errorf("card unavailable")
	h.plat.mu.Unlock()
	h.say("perm:ls")
	got := h.waitSent(1)
	if !strings.Contains(got[0], "Reply /allow or /deny") {
		t.Fatal(got)
	}
	h.say("/allow")
	h.waitCall("perm:")
	if h.calls("perm:")[0] != "perm:allow_once" {
		t.Fatal(h.calls("perm:"))
	}
}

func TestPersistentReadPermissionUsesExistingCards(t *testing.T) {
	t.Parallel()
	for _, tc := range []struct{ name, reply, want string }{
		{"allow", "/allow", "read_grant"},
		{"deny", "/deny", "reject_once"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			h := newHarness(t, setup{})
			h.say("permread:Permanently allow reading /fixture; saved to read_paths")
			got := h.waitSent(1)
			if !strings.Contains(got[0], "Permanently allow reading /fixture") {
				t.Fatalf("card = %q", got[0])
			}
			id := h.plat.approvalID()
			msg := h.msg("c1", "u1", tc.reply)
			msg.ApprovalID = id
			h.plat.deliver(msg)
			h.waitCall("perm:")
			if got := h.calls("perm:"); len(got) != 1 || got[0] != "perm:"+tc.want {
				t.Fatalf("agent outcomes = %q", got)
			}
			h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.statuses) == 1 }, "read card resolved")
		})
	}
}
