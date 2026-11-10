package bridge

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/baiyuqing/otto/chat-connector/internal/agent"
	"github.com/coder/acp-go-sdk"
)

// chat is the state of one "<platform>:<chat id>".
type chat struct {
	approvalViewMu   sync.Mutex
	sharedCards      map[string]*sharedCard
	approvalWatchSID string
	sharedBlocking   bool
	b                *Bridge
	key              string
	p                Platform
	id               string

	mu                 sync.Mutex
	queue              []Message
	dialogue           *approvalDialogue
	approvalGeneration uint64
	activity           chatActivity  // queue-worker or session-switch ownership
	turnDone           chan struct{} // closes after this turn's chat reply is sent
	sid                string
	taskGeneration     uint64
	controlTask        *controlTask
	perm               *permission
	reply              strings.Builder
	brk                bool // a tool call followed text; the next text starts a paragraph
	// proposed is set when the turn called remember or forget.
	proposed          bool
	memoryMu          sync.Mutex             // serializes card refreshes and memory reviews
	memoryCards       map[string]*memoryCard // request token -> original session/candidate
	manageMu          sync.Mutex
	change            *pendingChange // one connector-bound config confirmation
	approvalStatusSID string
	approvalStatuses  map[string]agent.SharedApproval
}

type pendingChange struct {
	id       string
	senderID string
	expires  time.Time
}

type controlItem struct {
	message Message
	steer   bool
	text    string
}

type permission struct {
	id      string
	decided chan decision // buffered 1
}

type decision int

const (
	allow decision = iota
	deny
	cancel
)

// send posts a connector notice, which is plain text.
func (c *chat) send(ctx context.Context, replyTo, text string) { c.post(ctx, "", replyTo, text, false) }

// sendReply posts the agent's Markdown reply for the message with requestID.
func (c *chat) sendReply(ctx context.Context, requestID, replyTo, text string) {
	c.post(ctx, requestID, replyTo, text, true)
}

// post sends text. A send with a requestID is a reply to that message and is
// counted in replies_total; its success is logged too. Failures of any send
// are logged.
func (c *chat) post(ctx context.Context, requestID, replyTo, text string, markdown bool) {
	err := c.p.Send(ctx, c.id, replyTo, text, markdown)
	if err != nil {
		if ctx.Err() == nil {
			slog.Warn("chat_reply_failed", "request_id", requestID, "platform", c.p.Name(), "chat_id", c.id, "bytes", len(text), "error", err)
			if requestID != "" {
				c.b.opts.Metrics.Reply(c.p.Name(), "error")
			}
		}
		return
	}
	if requestID != "" {
		slog.Info("chat_reply_sent", "request_id", requestID, "platform", c.p.Name(), "chat_id", c.id, "bytes", len(text))
		c.b.opts.Metrics.Reply(c.p.Name(), "ok")
	}
}

// notify sends text from a new goroutine so deliver does not block.
// ponytail: notices from deliver can overtake each other; add a per-chat
// outbox if ordering of notices matters.
func (c *chat) notify(replyTo, text string) {
	go c.send(c.b.workCtx, replyTo, text)
}

// queueFull logs a refused message and marks it for the messages_total outcome.
func (c *chat) queueFull(m Message, length int) {
	slog.Warn("chat_queue_full", "request_id", m.RequestID, "platform", c.p.Name(), "chat_id", c.id, "queue_length", length)
	c.b.opts.Metrics.Message(c.p.Name(), "queue_full")
	if m.queueFull != nil {
		*m.queueFull = true
	}
}

func (c *chat) enqueue(m Message) bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.enqueueLocked(m)
}

// enqueueLocked admits a message while c.mu is held.
func (c *chat) enqueueLocked(m Message) bool {
	if c.switchingLocked() {
		c.notify(m.MessageID, "A session switch is in progress; the message was not queued.")
		return false
	}
	if len(c.queue) >= c.b.opts.QueueLimit {
		c.queueFull(m, len(c.queue))
		c.notify(m.MessageID, fmt.Sprintf("Queue is full (%d messages); the message was not queued.", c.b.opts.QueueLimit))
		return false
	}
	c.queue = append(c.queue, m)
	slog.Info("chat_queued", "request_id", m.RequestID, "platform", c.p.Name(), "chat_id", c.id, "queue_length", len(c.queue))
	if c.workerOwnedLocked() {
		return true
	}
	c.b.mu.Lock()
	closing := c.b.closing
	if !closing {
		c.b.wg.Add(1)
	}
	c.b.mu.Unlock()
	if closing {
		c.queue = nil
		c.notify(m.MessageID, "Connector is shutting down; the message was not queued.")
		return false
	}
	c.beginWorkerLocked()
	go c.work()
	return true
}

func (c *chat) enqueueTaskControlLocked(item controlItem) bool {
	if !c.taskControlAvailableLocked() {
		return false
	}
	task := c.controlTask
	if len(c.queue)+len(task.controls) >= c.b.opts.QueueLimit {
		return false
	}
	task.controls = append(task.controls, item)
	if task.worker != nil {
		return true
	}

	c.b.mu.Lock()
	closing := c.b.closing
	if !closing {
		c.b.wg.Add(1)
	}
	c.b.mu.Unlock()
	if closing {
		task.controls = nil
		c.notify(item.message.MessageID, "Connector is shutting down; control input was not routed.")
		return false
	}
	worker := &controlWorker{task: task, done: make(chan struct{})}
	task.worker = worker
	go func() {
		defer c.b.wg.Done()
		c.taskControlDialogue(worker)
	}()
	return true
}

func (c *chat) taskControlDialogue(worker *controlWorker) {
	task := worker.task
	for {
		c.mu.Lock()
		if !c.controlWorkerCurrentLocked(worker) {
			if task.worker == worker {
				task.controls = nil
			}
			c.finishTaskControlWorkerLocked(worker)
			c.mu.Unlock()
			return
		}
		if len(task.controls) == 0 {
			c.finishTaskControlWorkerLocked(worker)
			c.mu.Unlock()
			return
		}
		item := task.controls[0]
		task.controls = task.controls[1:]
		c.mu.Unlock()

		ctx, stop := context.WithTimeout(task.ctx, 35*time.Second)
		var reply *agent.ControlReply
		var err error
		if item.steer {
			err = c.b.opts.Agent.Steer(ctx, task.sessionID, item.text)
		} else {
			reply, err = c.b.opts.Agent.ControlMessageWithChannel(ctx, task.sessionID, item.message.Text, item.message.Platform)
		}
		stop()

		response, markdown := "", false
		c.mu.Lock()
		if !c.controlWorkerCurrentLocked(worker) {
			c.finishTaskControlWorkerLocked(worker)
			c.mu.Unlock()
			return
		}
		if !item.steer && err != nil && (c.perm != nil || c.approvalDialogueAvailableLocked()) {
			accepted := c.enqueueApprovalMessageLocked(item.message)
			c.mu.Unlock()
			if !accepted {
				c.notify(item.message.MessageID, fmt.Sprintf("Queue is full (%d messages); the message was not queued.", c.b.opts.QueueLimit))
			}
			continue
		}
		if item.steer {
			switch {
			case err == nil:
				response = "Guidance accepted; it will be used at the next safe task boundary."
			case errors.Is(err, agent.ErrTaskControlUnsupported):
				response = "This agent does not support /steer; guidance was not sent. Update and restart Otto to enable active-task guidance."
			case errors.Is(err, agent.ErrSteerRejected):
				response = "The active task rejected this guidance."
			default:
				response = "Could not confirm guidance acceptance: " + err.Error()
			}
		} else {
			switch {
			case errors.Is(err, agent.ErrTaskControlUnsupported):
				if c.enqueueLocked(item.message) {
					response = "This agent does not support active-task guidance; your message was queued as a follow-up. Update and restart Otto for in-place guidance."
				}
			case err != nil:
				response = "Could not confirm how this message was routed. It was not queued by the connector; check task status before resending, or use /queue <prompt> for a separate follow-up."
			case reply == nil:
				if c.enqueueLocked(item.message) {
					response = "The active task had already finished; your message was queued as a follow-up."
				}
			case reply.Queued:
				if c.enqueueLocked(item.message) {
					response = reply.Text
					if response == "" {
						response = "Your message was queued as a follow-up."
					}
					markdown = true
				}
			default:
				if strings.TrimSpace(reply.Text) == "" {
					response = "Could not confirm how this message was routed. It was not queued by the connector; check task status before resending, or use /queue <prompt> for a separate follow-up."
				} else {
					response = reply.Text
					markdown = true
				}
			}
		}
		c.mu.Unlock()
		if response != "" {
			if markdown {
				c.sendReply(c.b.workCtx, item.message.RequestID, item.message.MessageID, response)
			} else {
				c.send(c.b.workCtx, item.message.MessageID, response)
			}
		}
	}
}

func (c *chat) work() {
	defer c.b.wg.Done()
	for {
		c.mu.Lock()
		if len(c.queue) == 0 || c.b.workCtx.Err() != nil {
			c.finishWorkerLocked()
			c.queue = nil
			c.mu.Unlock()
			return
		}
		m := c.queue[0]
		c.queue = c.queue[1:]
		c.beginMessageLocked()
		c.mu.Unlock()
		c.turn(m)
	}
}

// turn runs one message through the agent and sends the reply.
func (c *chat) turn(m Message) {
	turnDone := make(chan struct{})
	c.mu.Lock()
	c.turnDone = turnDone
	c.mu.Unlock()
	defer func() {
		c.mu.Lock()
		if c.turnDone == turnDone {
			c.turnDone = nil
		}
		c.mu.Unlock()
		close(turnDone)
	}()

	started := time.Now()
	ctx := c.b.workCtx
	typingDone := make(chan struct{})
	defer close(typingDone)
	go c.typing(ctx, typingDone)

	var (
		sid  string
		stop acp.StopReason
		err  error
		task *controlTask
	)
	for attempt := 0; attempt < 2; attempt++ {
		if sid, err = c.session(ctx, m); err != nil {
			break
		}
		unlock := c.b.lockSession(sid)
		c.mu.Lock()
		if c.stopRequestedLocked() {
			c.mu.Unlock()
			unlock()
			c.send(ctx, m.MessageID, "Stopped.")
			return
		}
		c.sid = sid
		c.cancelReboundApprovalDialogueLocked()
		task = c.beginTaskControlLocked(ctx, sid)
		c.reply.Reset()
		c.brk = false
		c.proposed = false
		c.mu.Unlock()
		c.b.mu.Lock()
		c.b.bySession[sid] = c
		c.b.mu.Unlock()

		c.startApprovalWatch(sid)
		slog.Info("turn_started", "request_id", m.RequestID, "platform", m.Platform, "chat_id", m.ChatID, "session_id", sid)
		stop, err = c.b.opts.Agent.PromptWithChannel(ctx, sid, m.Text, m.Platform, m.Platform+":"+m.ChatID, m.RequestID, chatInputID(m), chatPrompt(m))

		c.b.mu.Lock()
		delete(c.b.bySession, sid)
		c.b.mu.Unlock()
		c.finishTaskControl(task)
		unlock()
		if !errors.Is(err, agent.ErrNotOpen) {
			break
		}
	}
	c.mu.Lock()
	c.sid = ""
	c.cancelReboundApprovalDialogueLocked()
	text := strings.TrimSpace(c.reply.String())
	c.reply.Reset()
	proposed := c.proposed
	c.proposed = false
	c.mu.Unlock()
	if ctx.Err() != nil {
		return
	}
	elapsed := time.Since(started)
	if err != nil {
		slog.Warn("turn_failed", "request_id", m.RequestID, "platform", m.Platform, "chat_id", m.ChatID, "session_id", sid,
			"duration_ms", elapsed.Milliseconds(), "error", err)
		c.b.opts.Metrics.Turn(m.Platform, "error", elapsed)
	} else {
		slog.Info("turn_finished", "request_id", m.RequestID, "platform", m.Platform, "chat_id", m.ChatID, "session_id", sid,
			"stop_reason", string(stop), "duration_ms", elapsed.Milliseconds(), "reply_bytes", len(text))
		c.b.opts.Metrics.Turn(m.Platform, string(stop), elapsed)
	}
	switch {
	case err != nil:
		var ee *agent.ExitError
		if errors.As(err, &ee) {
			c.send(ctx, m.MessageID, ee.Error())
		} else {
			c.send(ctx, m.MessageID, "Error: "+err.Error())
		}
	case stop == acp.StopReasonCancelled:
		c.send(ctx, m.MessageID, "Stopped.")
	case stop != acp.StopReasonEndTurn:
		if text != "" {
			text += "\n\n"
		}
		c.sendReply(ctx, m.RequestID, m.MessageID, text+"Stop reason: "+string(stop))
	default:
		if proposed {
			if text != "" {
				text += "\n\n"
			}
			text += memoryHint
		}
		if text != "" {
			c.sendReply(ctx, m.RequestID, m.MessageID, text)
		}
	}
	if err == nil && stop == acp.StopReasonEndTurn {
		c.memoryMu.Lock()
		c.showMemory(ctx, sid, "", false)
		c.memoryMu.Unlock()
	}
}

// sendBackground sends a session notice (plain text) or wake reply (Markdown)
// after the current turn replies, only while the chat remains bound to sessionID.
func (c *chat) sendBackground(sessionID, text string, markdown bool) {
	c.mu.Lock()
	turnDone := c.turnDone
	c.mu.Unlock()
	go func() {
		if turnDone != nil {
			select {
			case <-turnDone:
			case <-c.b.workCtx.Done():
				return
			}
		}
		if access := c.b.opts.Access[c.p.Name()]; c.b.opts.Store.Session(c.key) == sessionID && slices.Contains(access.Chats, c.id) && len(access.Senders) > 0 {
			c.post(c.b.workCtx, "", "", text, markdown)
		}
	}()
}

// memoryHint follows a turn that proposed a memory change. Only a person can
// decide it, through the connector's commands, not by writing to the model.
const memoryHint = "Memory changes are proposals. Send /memory to review them."

// chatPrompt is the legacy fallback for agents that do not advertise
// _meta.otto.channelContext. Current Otto receives this platform as structured
// per-turn metadata instead. Agent commands are sent as typed.
func chatPrompt(m Message) string {
	if f := strings.Fields(m.Text); len(f) > 0 && slices.ContainsFunc(Commands, func(c Command) bool {
		return c.Agent && "/"+c.Name == f[0]
	}) {
		return m.Text
	}
	var channel string
	switch m.Platform {
	case "telegram":
		channel = "Telegram"
	default:
		return m.Text
	}
	return "[otto-connect channel context]\nCurrent channel: " + channel +
		". The user is communicating through chat and cannot see the local terminal. " +
		"Keep replies readable in chat; explain results, progress, and any necessary user actions. " +
		"For a pending permission request, the user can click Deny or send /deny; /stop cancels the turn and clears its queue. Ordinary messages during approval are handled by an approval-only dialogue. " +
		"This channel does not grant additional tools or permissions.\n[/otto-connect channel context]\n\n" + m.Text
}

// session returns the chat's session id, loading the stored one or creating
// a new one. A load that the agent refuses is replaced by a new session and a
// notice.
func (c *chat) session(ctx context.Context, m Message) (string, error) {
	replyTo := m.MessageID
	if id := c.b.opts.Store.Session(c.key); id != "" {
		err := c.b.load(ctx, id)
		if err == nil {
			slog.Info("session_loaded", "request_id", m.RequestID, "platform", m.Platform, "chat_id", m.ChatID, "session_id", id)
			return id, nil
		}
		var ee *agent.ExitError
		if errors.As(err, &ee) || ctx.Err() != nil {
			return "", err
		}
		slog.Warn("session_load_failed", "request_id", m.RequestID, "platform", m.Platform, "chat_id", m.ChatID, "session_id", id, "error", err)
		c.send(ctx, replyTo, "The previous session could not be loaded; started a new one.")
	}
	id, err := c.b.opts.Agent.New(ctx)
	if err != nil {
		return "", err
	}
	c.mu.Lock()
	if err := c.b.opts.Store.SetSession(c.key, id); err != nil {
		slog.Warn("saving session id failed", "chat", c.key, "error", err)
	}
	c.cancelReboundApprovalDialogueLocked()
	c.mu.Unlock()
	slog.Info("session_created", "request_id", m.RequestID, "platform", m.Platform, "chat_id", m.ChatID, "session_id", id)
	return id, nil
}

func (c *chat) typing(ctx context.Context, done <-chan struct{}) {
	t := time.NewTicker(c.b.opts.TypingInterval)
	defer t.Stop()
	for {
		_ = c.p.Typing(ctx, c.id)
		select {
		case <-t.C:
		case <-done:
			return
		case <-ctx.Done():
			return
		}
	}
}

// collect adds one session update to the running turn's reply.
func (c *chat) collect(u acp.SessionUpdate) {
	c.mu.Lock()
	defer c.mu.Unlock()
	switch {
	case u.AgentMessageChunk != nil:
		t := u.AgentMessageChunk.Content.Text
		if t == nil {
			return
		}
		if c.brk {
			s := strings.TrimRight(c.reply.String(), " \t\r\n")
			c.reply.Reset()
			c.reply.WriteString(s + "\n\n")
			c.brk = false
		}
		c.reply.WriteString(t.Text)
	case u.ToolCall != nil:
		c.brk = strings.TrimSpace(c.reply.String()) != ""
		if t := u.ToolCall.Title; t == "remember" || t == "forget" {
			c.proposed = true
		}
	}
}
