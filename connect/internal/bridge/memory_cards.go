package bridge

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"log/slog"
	"time"

	"github.com/baiyuqing/otto/connect/internal/agent"
)

type memoryCard struct {
	session, candidate string
	finish             func(context.Context, string) error
}

// Callers hold memoryMu. Cards are a disposable view of the first pending
// page, not another source of review state. The ACP service owns decisions.
func (c *chat) showMemory(ctx context.Context, sid, replyTo string, explicit bool) {
	if ctx.Err() != nil {
		return
	}
	p, buttons := c.p.(ApprovalPlatform)
	if !buttons && !explicit {
		return
	}
	ctx, stop := context.WithTimeout(ctx, 2*time.Minute)
	defer stop()
	page, err := c.b.opts.Agent.MemoryPending(ctx, sid, "")
	if err != nil {
		if explicit {
			c.send(ctx, replyTo, memoryError(err))
		}
		return
	}
	if !buttons {
		if explicit {
			c.send(ctx, replyTo, renderPending(page))
		}
		return
	}
	// Close cards no longer on this page, including cards from an old session.
	for token, card := range c.memoryCards {
		pending := false
		if card.session == sid {
			for _, cand := range page.Candidates {
				if cand.ID == card.candidate {
					pending = true
					break
				}
			}
		}
		if !pending || explicit {
			c.closeMemory(ctx, token, "Closed; send /memory to refresh.")
		}
	}
	if len(page.Candidates) == 0 {
		if explicit {
			c.send(ctx, replyTo, "No pending memory candidates.")
		}
		return
	}
	for _, cand := range page.Candidates {
		exists := false
		for _, card := range c.memoryCards {
			if card.session == sid && card.candidate == cand.ID {
				exists = true
				break
			}
		}
		if exists {
			continue
		}
		var nonce [16]byte
		if _, err := rand.Read(nonce[:]); err != nil {
			c.send(ctx, replyTo, renderPending(page))
			return
		}
		token := hex.EncodeToString(nonce[:])
		text := fmt.Sprintf("Memory review: %s %s/%s\n\n%s\n\nReason: %s\nOrigin: %s\n\n/memory accept %s or /memory reject %s", cand.Action, cand.Kind, cand.Key, cand.Text, cand.Reason, cand.Origin, cand.ID, cand.ID)
		finish, err := p.SendApproval(ctx, c.id, token, text)
		if err != nil {
			slog.Warn("memory card send failed", "chat", c.key, "error", err)
			c.send(ctx, replyTo, renderPending(page))
			return
		}
		if c.memoryCards == nil {
			c.memoryCards = make(map[string]*memoryCard)
		}
		c.memoryCards[token] = &memoryCard{session: sid, candidate: cand.ID, finish: finish}
	}
	if page.NextCursor != "" && explicit {
		c.send(ctx, replyTo, "More are pending; decide some, then send /memory again.")
	}
}

// Only normalized, admitted button events reach here. Taking the memory lock
// in the goroutine keeps platform callbacks non-blocking.
func (c *chat) cmdApprovalButton(m Message) {
	c.mu.Lock()
	bash := c.perm != nil && c.perm.id == m.ApprovalID
	c.mu.Unlock()
	if bash {
		c.cmdDecide(m, m.Text == "/allow")
		return
	}
	// All token lookups use memoryMu; do not block the platform on network I/O.
	// The callback dispatch itself happens asynchronously, with Bash fallback.
	go func() {
		c.memoryMu.Lock()
		defer c.memoryMu.Unlock()
		card := c.memoryCards[m.ApprovalID]
		if card == nil {
			c.cmdDecide(m, m.Text == "/allow")
			return
		}
		ctx, stop := context.WithTimeout(c.b.workCtx, 2*time.Minute)
		defer stop()
		if c.b.opts.Store.Session(c.key) != card.session {
			c.closeMemory(ctx, m.ApprovalID, "Closed; session changed.")
			c.send(ctx, "", "This card belongs to a previous session; send /memory to refresh.")
			return
		}
		if err := c.b.opts.Agent.Load(ctx, card.session); err != nil {
			c.send(ctx, "", memoryError(err))
			return
		}
		decision := "reject"
		if m.Text == "/allow" {
			decision = "accept"
		}
		_, err := c.b.opts.Agent.MemoryReview(ctx, card.session, card.candidate, decision)
		if err != nil {
			if errors.Is(err, agent.ErrMemoryConflict) || errors.Is(err, agent.ErrCandidateNotFound) {
				c.closeMemory(ctx, m.ApprovalID, memoryStatus(decision, err))
			}
			c.send(ctx, "", memoryError(err))
			return
		}
		c.closeMemory(ctx, m.ApprovalID, memoryStatus(decision, nil))
		c.send(ctx, "", memoryStatus(decision, nil))
	}()
}

func memoryStatus(decision string, err error) string {
	if err != nil {
		return "Closed; already decided or changed."
	}
	if decision == "accept" {
		return "Approved."
	}
	return "Denied."
}

func (c *chat) closeMemory(ctx context.Context, token, status string) {
	card := c.memoryCards[token]
	delete(c.memoryCards, token)
	cleanup, stop := context.WithTimeout(ctx, 10*time.Second)
	defer stop()
	if err := card.finish(cleanup, status); err != nil {
		slog.Warn("memory card update failed", "chat", c.key, "error", err)
	}
}

func (c *chat) finishMemory(ctx context.Context, sid, id, status string) {
	for token, card := range c.memoryCards {
		if card.session == sid && card.candidate == id {
			c.closeMemory(ctx, token, status)
		}
	}
}
