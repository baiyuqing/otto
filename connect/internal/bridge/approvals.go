package bridge

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/baiyuqing/otto/connect/internal/agent"
)

type sharedCard struct {
	session  string
	request  agent.SharedApproval
	finish   func(context.Context, string) error
	deciding bool
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
// active between turns so reflection and other background requests are visible.
func (c *chat) startApprovalWatch(sid string) {
	c.mu.Lock()
	if c.approvalWatchSID == sid {
		c.mu.Unlock()
		return
	}
	c.approvalWatchSID = sid
	c.mu.Unlock()
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

func (c *chat) showApprovals(ctx context.Context, sid string, requests []agent.SharedApproval) {
	c.approvalViewMu.Lock()
	defer c.approvalViewMu.Unlock()
	platform, ok := c.p.(ApprovalPlatform)
	if !ok {
		return
	}
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
		c.mu.Lock()
		card := c.sharedCards[r.ID]
		ready := card != nil && card.finish != nil
		c.mu.Unlock()
		if ready {
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
			card = &sharedCard{session: sid, request: r}
			c.mu.Lock()
			if c.sharedCards == nil {
				c.sharedCards = map[string]*sharedCard{}
			}
			c.sharedCards[r.ID] = card
			c.mu.Unlock()
		}
		finish, err := platform.SendApproval(ctx, c.id, r.ID, text)
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
	card := c.sharedCards[m.ApprovalID]
	c.mu.Unlock()
	if card == nil {
		return false
	}
	c.decideShared(card, m.Text == "/allow", "")
	return true
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
