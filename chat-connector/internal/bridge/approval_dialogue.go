package bridge

import (
	"context"
	"errors"
	"time"

	"github.com/baiyuqing/otto/chat-connector/internal/agent"
)

// A non-nil dialogue owns one session's FIFO, cancellation and completion.
// Identity/context fields are immutable; transitions require chat.mu.
type approvalDialogue struct {
	generation uint64
	sessionID  string
	ctx        context.Context
	cancel     context.CancelFunc
	messages   []Message
	done       chan struct{}
}

func (c *chat) approvalSessionIDLocked() string {
	if c.sid != "" {
		return c.sid
	}
	return c.b.opts.Store.Session(c.key)
}

func (c *chat) approvalDialogueAvailableLocked() bool {
	return c.dialogue != nil && c.approvalDialogueCurrentLocked(c.dialogue)
}

func (c *chat) approvalDialogueCurrentLocked(dialogue *approvalDialogue) bool {
	return c.dialogue == dialogue && c.approvalGeneration == dialogue.generation &&
		dialogue.ctx.Err() == nil && dialogue.sessionID == c.approvalSessionIDLocked()
}

func (c *chat) cancelReboundApprovalDialogueLocked() {
	if c.dialogue != nil && c.dialogue.sessionID != c.approvalSessionIDLocked() {
		c.stopApprovalDialogueLocked()
	}
}

func (c *chat) beginApprovalDialogueLocked(parent context.Context, sessionID string) *approvalDialogue {
	c.stopApprovalDialogueLocked()
	ctx, cancel := context.WithCancel(parent)
	dialogue := &approvalDialogue{
		generation: c.approvalGeneration,
		sessionID:  sessionID,
		ctx:        ctx,
		cancel:     cancel,
		done:       make(chan struct{}),
	}
	c.dialogue = dialogue
	return dialogue
}

// Cancellation revokes routing; only the captured worker completes done.
func (c *chat) stopApprovalDialogueLocked() {
	c.approvalGeneration++
	if dialogue := c.dialogue; dialogue != nil {
		dialogue.messages = nil
		dialogue.cancel()
		c.dialogue = nil
	}
}

func (c *chat) finishApprovalDialogueLocked(dialogue *approvalDialogue) {
	dialogue.messages = nil
	dialogue.cancel()
	if c.dialogue == dialogue {
		c.dialogue = nil
	}
	select {
	case <-dialogue.done:
	default:
		close(dialogue.done)
	}
}

func (c *chat) enqueueApprovalMessageLocked(m Message) bool {
	dialogue := c.dialogue
	queued := 0
	if c.approvalDialogueAvailableLocked() {
		queued = len(dialogue.messages)
	} else {
		dialogue = nil
	}
	if len(c.queue)+queued >= c.b.opts.QueueLimit {
		return false
	}
	c.b.mu.Lock()
	closing := c.b.closing
	if !closing && dialogue == nil {
		c.b.wg.Add(1)
	}
	c.b.mu.Unlock()
	if closing {
		return false
	}
	if dialogue != nil {
		dialogue.messages = append(dialogue.messages, m)
		return true
	}
	dialogue = c.beginApprovalDialogueLocked(c.b.workCtx, c.approvalSessionIDLocked())
	dialogue.messages = append(dialogue.messages, m)
	go func() {
		defer c.b.wg.Done()
		c.runApprovalDialogue(dialogue)
	}()
	return true
}

// The agent decides whether input belongs in the turn queue; this path never
// grants permission. A worker retains its record through RPC and reply delivery.
func (c *chat) runApprovalDialogue(dialogue *approvalDialogue) {
	for {
		c.mu.Lock()
		if !c.approvalDialogueCurrentLocked(dialogue) || len(dialogue.messages) == 0 {
			// Detach atomically with the empty check, so a new enqueue owns a
			// fresh worker rather than appending to a worker that is exiting.
			c.finishApprovalDialogueLocked(dialogue)
			c.mu.Unlock()
			return
		}
		m := dialogue.messages[0]
		dialogue.messages = dialogue.messages[1:]
		c.mu.Unlock()

		ctx, stop := context.WithTimeout(dialogue.ctx, 35*time.Second)
		reply, err := c.b.opts.Agent.ApprovalMessageWithChannel(ctx, dialogue.sessionID, m.Text, m.Platform, chatPrompt(m))
		queue, response := false, ""
		if errors.Is(err, agent.ErrApprovalUnsupported) {
			queue = true
			response = "This agent queues messages while waiting for approval; use /deny or /stop to cancel."
		} else if err != nil {
			response = "Could not handle the approval message; use /deny to reject or /stop to cancel. " + err.Error()
		} else {
			queue = reply == nil || reply.Queued
			if reply != nil && reply.Text != "" {
				response = reply.Text
			}
		}
		c.mu.Lock()
		if !c.approvalDialogueCurrentLocked(dialogue) {
			c.finishApprovalDialogueLocked(dialogue)
			c.mu.Unlock()
			stop()
			return
		}
		if queue {
			c.enqueueLocked(m)
		}
		c.mu.Unlock()
		if response != "" {
			c.send(ctx, m.MessageID, response)
		}
		stop()
	}
}
