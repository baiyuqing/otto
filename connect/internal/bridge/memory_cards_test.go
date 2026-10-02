package bridge

import (
	"errors"
	"strings"
	"testing"
)

func memoryClick(h *harness, chat, sender, token, text string) {
	m := h.msg(chat, sender, text)
	m.ApprovalID = token
	h.plat.deliver(m)
}

func waitCards(h *harness, n int) {
	h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.approvals) >= n }, "memory cards")
	// SendApproval records the ID before the bridge registers its callback;
	// callbacks acquire memoryMu, so a click is safe as soon as it was sent.
}

func TestMemoryCardsAutomaticAndReview(t *testing.T) {
	for _, decision := range []string{"accept", "reject"} {
		t.Run(decision, func(t *testing.T) {
			h := newHarness(t, setup{})
			seedMemory(t, h.dir, candA+"\tpreference\tprefers tabs")
			h.say("remember")
			waitCards(h, 1)
			texts := h.waitSent(2)
			if !strings.Contains(texts[1], "prefers tabs") || !strings.Contains(texts[1], "Memory review: create") {
				t.Fatal(texts)
			}
			id := h.plat.approvalID()
			memoryClick(h, "c1", "intruder", id, "/allow")
			memoryClick(h, "c2", "u1", id, "/allow")
			settle()
			if len(h.calls("review:")) != 0 {
				t.Fatal("unauthorized click reviewed memory")
			}
			text := "/deny"
			status := "Denied."
			if decision == "accept" {
				text = "/allow"
				status = "Approved."
			}
			memoryClick(h, "c1", "u1", id, text)
			h.waitCall("review:")
			if got := h.calls("review:"); len(got) != 1 || got[0] != "review:"+candA+":"+decision {
				t.Fatal(got)
			}
			h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.statuses) > 0 }, "resolved card")
			h.plat.mu.Lock()
			got := h.plat.statuses[0]
			h.plat.mu.Unlock()
			if got != status {
				t.Fatal(got)
			}
			memoryClick(h, "c1", "u1", id, text)
			settle()
			if len(h.calls("review:")) != 1 {
				t.Fatal("duplicate click reviewed twice")
			}
		})
	}
}

func TestMemoryCardsRefreshAndSessionBinding(t *testing.T) {
	h := newHarness(t, setup{})
	seedMemory(t, h.dir, candA+"\tfact\tone")
	h.say("hi")
	waitCards(h, 1)
	old := h.plat.approvalID()
	h.say("/memory")
	waitCards(h, 2)
	fresh := h.plat.approvalID()
	if old == fresh {
		t.Fatal("refresh reused token")
	}
	memoryClick(h, "c1", "u1", old, "/allow")
	settle()
	if len(h.calls("review:")) != 0 {
		t.Fatal("old token reviewed memory")
	}
	h.say("/new")
	h.waitFor(func() bool { return h.store.Session("fake:c1") == "" }, "new session")
	memoryClick(h, "c1", "u1", fresh, "/allow")
	settle()
	if len(h.calls("review:")) != 0 {
		t.Fatal("old session card reviewed memory")
	}
	h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.statuses) == 2 }, "old session closed")
}

func TestMemoryCardsTextReviewAndFallback(t *testing.T) {
	h := newHarness(t, setup{})
	seedMemory(t, h.dir, candA+"\tfact\tone")
	h.say("hi")
	waitCards(h, 1)
	h.say("/memory accept " + candA)
	h.waitCall("review:")
	h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.statuses) == 1 }, "text review closes buttons")
	h.plat.mu.Lock()
	h.plat.cardErr = errors.New("card unavailable")
	h.plat.mu.Unlock()
	seedMemory(t, h.dir, candC+"\tfact\ttwo")
	h.say("/memory")
	h.waitFor(func() bool {
		for _, text := range h.plat.texts() {
			if strings.Contains(text, "1 pending memory candidates") {
				return true
			}
		}
		return false
	}, "text fallback")
	h.say("/memory reject " + candC)
	h.waitFor(func() bool { return len(h.calls("review:")) == 2 }, "fallback review")
}

func TestMemoryCardConflictClosesAndShutdownReleasesCards(t *testing.T) {
	h := newHarness(t, setup{})
	seedMemory(t, h.dir, "dddd1111000000000000000000000000\tfact\tone")
	h.say("hi")
	waitCards(h, 1)
	memoryClick(h, "c1", "u1", h.plat.approvalID(), "/allow")
	h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.statuses) == 1 }, "conflict closes card")
	seedMemory(t, h.dir, candC+"\tfact\ttwo")
	h.say("/memory")
	waitCards(h, 2)
	if err := h.shutdown(); err != nil {
		t.Fatal(err)
	}
	h.plat.mu.Lock()
	defer h.plat.mu.Unlock()
	if len(h.plat.statuses) != 2 || !strings.Contains(h.plat.statuses[1], "connector stopped") {
		t.Fatal(h.plat.statuses)
	}
}
