package bridge

import (
	"context"
	"crypto/sha256"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/baiyuqing/otto/chat-connector/internal/agent"
)

func (c *chat) presentationOrigin() string {
	return "chat:" + c.p.Name() + ":" + c.id
}

func (c *chat) presents(r agent.SharedApproval) bool {
	return r.PresentationOrigin != "" && r.PresentationOrigin == c.presentationOrigin()
}

type sharedCard struct {
	token    string
	session  string
	request  agent.SharedApproval
	finish   func(context.Context, string) error
	deciding bool
}

// Stable across connector restarts, but invalidated by a different request
// revision, owning session or presentation origin. Fits platform callback limits.
func (c *chat) sharedApprovalToken(sid string, request agent.SharedApproval) string {
	key := fmt.Sprintf("%q:%q:%q:%d:%q", sid, c.presentationOrigin(), request.ID, request.Revision, request.Subject.Version)
	digest := sha256.Sum256([]byte(key))
	return fmt.Sprintf("%x", digest[:16])
}

// Restore only explicitly admitted chat bindings; no conversation is needed to
// make a durable background review visible after restarting the connector.
func (b *Bridge) restoreApprovalWatches() {
	for _, p := range b.opts.Platforms {
		access := b.opts.Access[p.Name()]
		if len(access.Senders) == 0 {
			continue
		}
		for _, id := range access.Chats {
			c := b.chat(p, id)
			sid := b.opts.Store.Session(c.key)
			if sid == "" {
				continue
			}
			b.wg.Add(1)
			go func() {
				defer b.wg.Done()
				ctx, cancel := context.WithTimeout(b.workCtx, 30*time.Second)
				defer cancel()
				supported, err := b.opts.Agent.SupportsSharedApprovals(ctx)
				if err != nil || !supported {
					return
				}
				if err := b.opts.Agent.Load(ctx, sid); err != nil {
					c.send(ctx, "", "Could not restore the approval queue; reconnect this session to retry.")
					return
				}
				c.startApprovalWatch(sid)
			}()
		}
	}
}

// The native journal owns state; cards are disposable views. The watcher remains
// active between turns so background requests are visible.
func (c *chat) startApprovalWatch(sid string) {
	c.mu.Lock()
	if c.approvalWatchSID == sid {
		c.mu.Unlock()
		return
	}
	c.approvalWatchSID = sid
	c.mu.Unlock()
	// Capture the cursor before returning to the caller that starts a turn.
	// This keeps fast AUTO decisions visible without replaying older turns.
	ctx, cancel := context.WithTimeout(c.b.workCtx, 10*time.Second)
	page, err := c.b.opts.Agent.SharedApprovals(ctx, sid)
	cancel()
	if err != nil {
		c.mu.Lock()
		if c.approvalWatchSID == sid {
			c.approvalWatchSID = ""
		}
		c.mu.Unlock()
		return
	}
	initialCursor := page.Cursor
	// Hydrate passive states once; list contains only requests waiting for a human.
	ctx, cancel = context.WithTimeout(c.b.workCtx, 10*time.Second)
	if changes, err := c.b.opts.Agent.ApprovalChanges(ctx, sid, 0); err == nil {
		for _, change := range changes {
			c.recordApprovalStatus(sid, change.Request)
		}
	}
	cancel()
	for _, request := range page.Requests {
		c.recordApprovalStatus(sid, request)
	}
	c.b.mu.Lock()
	if c.b.closing {
		c.b.mu.Unlock()
		return
	}
	c.b.wg.Add(1)
	c.b.mu.Unlock()
	go func() {
		defer c.b.wg.Done()
		defer func() {
			c.mu.Lock()
			if c.approvalWatchSID == sid {
				c.approvalWatchSID = ""
			}
			c.mu.Unlock()
		}()
		cursor := initialCursor
		timer := time.NewTicker(500 * time.Millisecond)
		defer timer.Stop()
		for {
			c.mu.Lock()
			current := c.approvalWatchSID == sid
			c.mu.Unlock()
			if !current || c.b.opts.Store.Session(c.key) != sid {
				return
			}
			ctx, cancel := context.WithTimeout(c.b.workCtx, 10*time.Second)
			page, err := c.b.opts.Agent.SharedApprovals(ctx, sid)
			if errors.Is(err, agent.ErrSharedApprovalUnsupported) {
				cancel()
				return
			}
			if err == nil {
				c.showApprovals(ctx, sid, page.Requests)
				changes, changeErr := c.b.opts.Agent.ApprovalChanges(ctx, sid, cursor)
				if changeErr == nil {
					for _, change := range changes {
						r := change.Request
						c.recordApprovalStatus(sid, r)
						if c.presents(r) && r.Review != nil {
							c.reviewNotice(ctx, r)
						}
						if c.presents(r) && r.Review == nil && r.Decision != nil && (r.Decision.Transport == "auto-mode" || r.Decision.Transport == "yolo-mode") {
							// A YOLO decision approved a request the user would
							// otherwise have seen, so it is always reported.
							policy := "AUTO"
							if r.Decision.Transport == "yolo-mode" {
								policy = "YOLO"
							}
							if r.Application == "failed" || r.Application == "unknown" {
								c.send(ctx, "", fmt.Sprintf("Approval %s was automatically approved; application %s: %s", r.ID, r.Application, r.ApplicationError))
							} else if r.Application == "not_started" || r.Application == "not_required" {
								c.send(ctx, "", fmt.Sprintf("Approval requested: %s\n%s\nReason: %s\nRequest: %s\nAutomatically approved (%s).", r.Title, r.Subject.Details, r.Reason, r.ID, policy))
							}
						}
						cursor = change.Sequence
					}
				}
			}
			cancel()
			select {
			case <-c.b.workCtx.Done():
				return
			case <-timer.C:
			}
		}
	}()
}

func (c *chat) recordApprovalStatus(sid string, r agent.SharedApproval) {
	if current := c.b.opts.Store.Session(c.key); current != "" && current != sid {
		return
	}
	if !c.presents(r) {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.approvalStatusSID != sid {
		c.approvalStatusSID = sid
		c.approvalStatuses = map[string]agent.SharedApproval{}
	}
	if r.HumanPending() || (r.State == "pending" && r.Review != nil && (r.Review.Status == "queued" || r.Review.Status == "reviewing")) || r.Application == "failed" || r.Application == "unknown" {
		c.approvalStatuses[r.ID] = r
	} else {
		delete(c.approvalStatuses, r.ID)
	}
}

func (c *chat) approvalCounts(sid string) string {
	c.mu.Lock()
	defer c.mu.Unlock()
	human, reviewing, failed := 0, 0, 0
	if c.approvalStatusSID != sid {
		return ""
	}
	for _, r := range c.approvalStatuses {
		if r.HumanPending() {
			human++
		}
		if r.State == "pending" && r.Review != nil && (r.Review.Status == "queued" || r.Review.Status == "reviewing") {
			reviewing++
		}
		if r.Application == "failed" || r.Application == "unknown" {
			failed++
		}
	}
	return fmt.Sprintf("\nHuman pending: %d · Reviewing: %d · Application failures: %d", human, reviewing, failed)
}

func (c *chat) showApprovals(ctx context.Context, sid string, requests []agent.SharedApproval) {
	c.approvalViewMu.Lock()
	defer c.approvalViewMu.Unlock()
	platform, ok := c.p.(ApprovalPlatform)
	if !ok {
		return
	}
	routed := requests[:0]
	for _, r := range requests {
		if c.presents(r) && r.HumanPending() {
			routed = append(routed, r)
		}
	}
	requests = routed
	blocking := false
	for _, r := range requests {
		blocking = blocking || r.Blocking
	}
	c.mu.Lock()
	c.sharedBlocking = blocking
	c.mu.Unlock()
	pending := map[string]bool{}
	for _, r := range requests {
		pending[r.ID] = true
		token := c.sharedApprovalToken(sid, r)
		c.mu.Lock()
		card := c.sharedCards[r.ID]
		ready := card != nil && card.finish != nil && card.token == token
		stale := card != nil && card.token != token && !card.deciding
		deciding := card != nil && card.deciding
		if stale {
			delete(c.sharedCards, r.ID)
		}
		c.mu.Unlock()
		if stale {
			c.finishSharedCard(ctx, card, "Closed; request changed.")
			card = nil
		}
		if ready || deciding {
			continue
		}
		text := fmt.Sprintf("%s\n\n%s\n\nReason: %s\nRequest: %s", r.Title, r.Subject.Details, r.Reason, r.ID)
		if r.Persistent {
			text += "\nThis establishes persistent authorization."
		}
		if r.Kind == "confirmation" {
			text += "\nProposal confirmation; this does not grant tool permissions."
		}
		if card == nil {
			text += fmt.Sprintf("\nDecide: /allow %s [feedback] or /deny %s [feedback]", r.ID, r.ID)
			// Complete details precede a possibly truncated card. Register first
			// so a fast button callback cannot race card publication.
			if err := c.p.Send(ctx, c.id, "", text, false); err != nil {
				continue
			}
			card = &sharedCard{token: token, session: sid, request: r}
			c.mu.Lock()
			if c.sharedCards == nil {
				c.sharedCards = map[string]*sharedCard{}
			}
			c.sharedCards[r.ID] = card
			c.mu.Unlock()
		}
		finish, err := platform.SendApproval(ctx, c.id, card.token, text)
		if err != nil {
			continue
		}
		c.mu.Lock()
		card.finish = finish
		c.mu.Unlock()
	}
	c.mu.Lock()
	var closed []*sharedCard
	for id, card := range c.sharedCards {
		if !card.deciding && (card.session != sid || !pending[id]) {
			closed = append(closed, card)
			delete(c.sharedCards, id)
		}
	}
	c.mu.Unlock()
	for _, card := range closed {
		c.finishSharedCard(ctx, card, "Closed; answered elsewhere or cancelled.")
	}
}

func (c *chat) decideSharedCard(m Message) bool {
	c.mu.Lock()
	var card *sharedCard
	for _, candidate := range c.sharedCards {
		if candidate.token == m.ApprovalID {
			card = candidate
			break
		}
	}
	c.mu.Unlock()
	if card == nil {
		return false
	}
	c.decideShared(card, m.Text == "/allow", "")
	return true
}

// Automatic review stays in audit/status views. Only actual application failure
// needs a chat notification; the human pending queue separately offers buttons.
func (c *chat) reviewNotice(ctx context.Context, r agent.SharedApproval) {
	if r.Application == "failed" || r.Application == "unknown" {
		c.send(ctx, "", fmt.Sprintf("Approval %s: application %s: %s", r.ID, r.Application, r.ApplicationError))
	}
}

func (c *chat) cmdSharedDecision(m Message, allow bool, args string) bool {
	c.mu.Lock()
	if len(c.sharedCards) == 0 {
		c.mu.Unlock()
		return false
	}
	fields := strings.Fields(args)
	var card *sharedCard
	feedback := ""
	if len(fields) > 0 {
		card = c.sharedCards[fields[0]]
		if card == nil {
			for _, candidate := range c.sharedCards {
				if candidate.token == fields[0] {
					card = candidate
					break
				}
			}
		}
		feedback = strings.TrimSpace(strings.TrimPrefix(args, fields[0]))
	} else {
		for _, pending := range c.sharedCards {
			if card != nil {
				card = nil
				break
			}
			card = pending
		}
	}
	c.mu.Unlock()
	if card == nil {
		c.notify(m.MessageID, "Choose an approval: /allow <request-id> [feedback] or /deny <request-id> [feedback].")
		return true
	}
	c.decideShared(card, allow, feedback)
	return true
}

func (c *chat) finishSharedCard(ctx context.Context, card *sharedCard, status string) {
	c.mu.Lock()
	finish := card.finish
	c.mu.Unlock()
	if finish != nil {
		_ = finish(ctx, status)
	} else {
		c.send(ctx, "", card.request.ID+": "+status)
	}
}

func (c *chat) decideShared(card *sharedCard, allow bool, feedback string) {
	c.mu.Lock()
	if c.sharedCards[card.request.ID] != card || card.deciding {
		c.mu.Unlock()
		return
	}
	card.deciding = true
	c.mu.Unlock()
	go func() {
		c.approvalViewMu.Lock()
		c.approvalViewMu.Unlock()
		defer func() { c.mu.Lock(); card.deciding = false; c.mu.Unlock() }()
		ctx, cancel := context.WithTimeout(c.b.workCtx, 10*time.Second)
		defer cancel()
		if c.b.opts.Store.Session(c.key) != card.session {
			c.send(ctx, "", "This approval belongs to another session.")
			return
		}
		option, status := "reject", "Denied."
		if allow {
			option, status = "approve", "Approved."
		}
		if err := c.b.opts.Agent.DecideSharedApproval(ctx, card.session, card.request, option, feedback); err != nil {
			c.send(ctx, "", err.Error())
			return
		}
		c.mu.Lock()
		delete(c.sharedCards, card.request.ID)
		c.mu.Unlock()
		c.finishSharedCard(ctx, card, status)
	}()
}
