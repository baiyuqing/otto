package bridge

import (
	"context"
	"errors"
	"testing"

	"github.com/baiyuqing/otto/chat-connector/internal/agent"
)

func TestSharedCardRetryKeepsDetailsAndDoesNotCloseAnInFlightDecision(t *testing.T) {
	h := newHarness(t, setup{})
	c := h.b.chat(h.plat, "c1")
	r := agent.SharedApproval{ID: "request-a", Revision: 1, Title: "Review plan", State: "pending", Blocking: true}
	r.Subject.Details = "Complete proposal"
	h.plat.mu.Lock()
	h.plat.cardErr = errors.New("temporary delivery failure")
	h.plat.mu.Unlock()
	ctx := context.Background()
	c.showApprovals(ctx, "session-a", []agent.SharedApproval{r})
	c.showApprovals(ctx, "session-a", []agent.SharedApproval{r})
	if len(h.plat.texts()) != 1 {
		t.Fatal("retry repeated the complete details message")
	}
	h.plat.mu.Lock()
	h.plat.cardErr = nil
	h.plat.mu.Unlock()
	c.showApprovals(ctx, "session-a", []agent.SharedApproval{r})
	c.showApprovals(ctx, "session-a", []agent.SharedApproval{r})
	if len(h.plat.texts()) != 2 {
		t.Fatal("card delivery was not retried once and deduplicated")
	}
	c.mu.Lock()
	c.sharedCards[r.ID].deciding = true
	c.mu.Unlock()
	c.showApprovals(ctx, "session-a", nil)
	h.plat.mu.Lock()
	closed := len(h.plat.statuses)
	h.plat.mu.Unlock()
	if closed != 0 {
		t.Fatal("a committed decision was closed before application completed")
	}
	c.mu.Lock()
	c.sharedCards[r.ID].deciding = false
	c.mu.Unlock()
	c.showApprovals(ctx, "session-a", nil)
	h.plat.mu.Lock()
	defer h.plat.mu.Unlock()
	if len(h.plat.statuses) != 1 {
		t.Fatal("an externally decided card did not close")
	}
}
