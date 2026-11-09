// Package bridge connects chat platforms to one ACP agent: admission,
// per-chat queues, connector commands, replies and permission requests.
package bridge

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/baiyuqing/otto/chat-connector/internal/agent"
	"github.com/baiyuqing/otto/chat-connector/internal/manage"
	"github.com/baiyuqing/otto/chat-connector/internal/metrics"
	"github.com/baiyuqing/otto/chat-connector/internal/state"
	"github.com/coder/acp-go-sdk"
)

// Access is the allowlist of one platform. A message is admitted only when
// its chat is in Chats and its sender is in Senders; an empty list admits
// nothing.
type Access struct {
	Chats   []string
	Senders []string
}

// Options configure a Bridge. Zero tunables select the defaults.
type Options struct {
	Agent *agent.Agent
	// Manage is non-nil only when the existing agent command uses --attach.
	// It calls otto serve over that command's Unix socket.
	Manage    *manage.Client
	Platforms []Platform
	Access    map[string]Access // by Platform.Name()
	Store     *state.Store
	Metrics   *metrics.Metrics // nil disables counting
	// Models reads the local Otto configuration for the "/model" picker.
	// Nil leaves bare "/model" to Otto.
	Models ModelSource

	QueueLimit        int           // default 10
	PermissionTimeout time.Duration // default 10 min
	StaleAfter        time.Duration // default 30 min
	TypingInterval    time.Duration // default 4 s
	Now               func() time.Time
}

// Bridge routes messages between platforms and the agent.
type Bridge struct {
	opts      Options
	platforms map[string]Platform

	workCtx  context.Context // ends at shutdown; chat workers run under it
	stopWork context.CancelFunc
	wg       sync.WaitGroup // chat workers

	mu        sync.Mutex // guards chats, bySession, closing
	chats     map[string]*chat
	bySession map[string]*chat // chats with a running prompt, by session id
	closing   bool
	sessLocks map[string]*sessLock // one per session id with a holder or waiter; guarded by mu
	loadMu    sync.Mutex           // serializes session/load calls
}

// New returns a Bridge and registers it as the agent's handler.
func New(opts Options) *Bridge {
	if opts.QueueLimit == 0 {
		opts.QueueLimit = 10
	}
	if opts.PermissionTimeout == 0 {
		opts.PermissionTimeout = 10 * time.Minute
	}
	if opts.StaleAfter == 0 {
		opts.StaleAfter = 30 * time.Minute
	}
	if opts.TypingInterval == 0 {
		opts.TypingInterval = 4 * time.Second
	}
	if opts.Now == nil {
		opts.Now = time.Now
	}
	b := &Bridge{
		opts:      opts,
		platforms: map[string]Platform{},
		chats:     map[string]*chat{},
		bySession: map[string]*chat{},
		sessLocks: map[string]*sessLock{},
	}
	b.workCtx, b.stopWork = context.WithCancel(context.Background())
	for _, p := range opts.Platforms {
		b.platforms[p.Name()] = p
	}
	opts.Agent.SetHandler(b)
	names := make([]string, 0, len(opts.Platforms))
	for _, p := range opts.Platforms {
		names = append(names, p.Name())
	}
	opts.Agent.SetPlatforms(names)
	chats := []string{}
	for _, p := range opts.Platforms {
		if p.Name() != "telegram" {
			continue
		}
		access := opts.Access[p.Name()]
		if len(access.Senders) == 0 {
			continue
		}
		for _, id := range access.Chats {
			chats = append(chats, p.Name()+":"+id)
		}
	}
	opts.Agent.SetDeliveryChats(chats)
	return b
}

// Run starts every platform and blocks until ctx ends or a platform's Run
// fails. It then cancels running prompts, stops the platforms, and closes the
// agent. It returns the first platform error.
func (b *Bridge) Run(ctx context.Context) error {
	for _, p := range b.opts.Platforms {
		acc := b.opts.Access[p.Name()]
		if len(acc.Chats) == 0 || len(acc.Senders) == 0 {
			slog.Warn("platform has an empty chats or senders list and will ignore all messages",
				"platform", p.Name(), "chats", len(acc.Chats), "senders", len(acc.Senders))
		}
	}
	platCtx, stopPlatforms := context.WithCancel(ctx)
	defer stopPlatforms()
	// The connection stays open so the agent can deliver scheduled messages
	// even when no chat is bound.
	b.wg.Add(1)
	go func() {
		defer b.wg.Done()
		b.opts.Agent.Keep(b.workCtx, b.loadBoundSessions)
	}()
	errc := make(chan error, len(b.opts.Platforms))
	for _, p := range b.opts.Platforms {
		go func() {
			err := p.Run(platCtx, b.deliver)
			if err != nil {
				err = fmt.Errorf("%s: %w", p.Name(), err)
			}
			errc <- err
		}()
	}
	b.restoreApprovalWatches()
	var first error
	done := 0
	select {
	case <-ctx.Done():
	case first = <-errc:
		done++
	}

	b.shutdown()
	stopPlatforms()
	for ; done < len(b.opts.Platforms); done++ {
		if err := <-errc; err != nil && first == nil {
			first = err
		}
	}
	b.wg.Wait()
	b.mu.Lock()
	var chats []*chat
	for _, c := range b.chats {
		chats = append(chats, c)
	}
	b.mu.Unlock()
	for _, c := range chats {
		c.memoryMu.Lock()
		for token := range c.memoryCards {
			c.closeMemory(context.Background(), token, "Closed; connector stopped. Send /memory after restart.")
		}
		c.memoryMu.Unlock()
	}
	if err := b.opts.Agent.Close(); err != nil {
		slog.Warn("closing agent", "error", err)
	}
	return first
}

// load opens session id in the agent process. Agent.Load returns nil for a
// session that is already open but does not wait for a load of the same id in
// flight, so loads are serialized here: the startup loader and a chat worker
// must not send session/load twice for one session. A slow load delays loads
// of other sessions; a per-session lock removes that if it matters.
func (b *Bridge) load(ctx context.Context, id string) error {
	b.loadMu.Lock()
	defer b.loadMu.Unlock()
	return b.opts.Agent.Load(ctx, id)
}

// loadBoundSessions loads each distinct session bound to a chat of a
// configured platform, one at a time. This restores the session's persisted
// timers in a restarted agent process and makes the agent follow the session,
// so wake replies reach the chat. A failed load is logged and does not remove
// the binding.
func (b *Bridge) loadBoundSessions() {
	sessions := b.opts.Store.Sessions()
	keys := slices.Sorted(maps.Keys(sessions))
	loaded := map[string]bool{}
	for _, key := range keys {
		name, id, ok := strings.Cut(key, ":")
		p := b.platforms[name]
		if !ok || p == nil || !slices.Contains(b.opts.Access[name].Chats, id) || len(b.opts.Access[name].Senders) == 0 {
			continue
		}
		b.chat(p, id)
		sid := sessions[key]
		if loaded[sid] {
			continue
		}
		loaded[sid] = true
		if err := b.load(b.workCtx, sid); err != nil && b.workCtx.Err() == nil {
			slog.Warn("loading bound session at startup failed", "chat", key, "session", sid, "error", err)
		}
	}
}

// shutdown stops the chat workers, which then return without replying, and
// sends session/cancel for every running prompt. workCtx ends first: a prompt
// that returns cancelled, or a permission request the agent withdraws, while
// workCtx is still live is reported to the chat as "Stopped." or as answered
// elsewhere. The agent connection stays open until Run closes it.
func (b *Bridge) shutdown() {
	b.mu.Lock()
	b.closing = true
	var sids []string
	for sid := range b.bySession {
		sids = append(sids, sid)
	}
	b.mu.Unlock()
	b.stopWork()
	for _, sid := range sids {
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		_ = b.opts.Agent.Cancel(ctx, sid)
		cancel()
	}
}

// Extension implements agent.ExtensionHandler. _otto/schedule/deliver sends
// text to a chat that is in its platform's chats allowlist. The connector
// keeps no state for it; the agent retries failed deliveries.
func (b *Bridge) Extension(ctx context.Context, method string, params json.RawMessage) (any, error) {
	if method == "_otto/local_commands/update" {
		return b.localCommands(ctx, params)
	}
	if method != "_otto/schedule/deliver" {
		return nil, acp.NewMethodNotFound(method)
	}
	var req struct {
		Chat string `json:"chat"`
		Text string `json:"text"`
	}
	dec := json.NewDecoder(bytes.NewReader(params))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&req); err != nil || req.Chat == "" || req.Text == "" {
		return nil, errors.New("chat and text are required")
	}
	name, id, _ := strings.Cut(req.Chat, ":")
	p := b.platforms[name]
	if p == nil || id == "" || !slices.Contains(b.opts.Access[name].Chats, id) {
		return nil, errors.New("chat is not an allowed chat of a configured platform")
	}
	if err := p.Send(ctx, id, "", req.Text, true); err != nil {
		return nil, err
	}
	return map[string]any{}, nil
}

func (b *Bridge) admitted(m Message) bool {
	acc := b.opts.Access[m.Platform]
	return slices.Contains(acc.Chats, m.ChatID) && slices.Contains(acc.Senders, m.SenderID)
}

// Command is one command as offered in a platform's command menu. Agent
// commands are not handled by the bridge: it forwards their text to the agent
// as a prompt and the agent runs them.
type Command struct {
	Name, Description string
	Agent             bool
}

// Commands lists the commands in menu order, grouped like Otto's TUI
// commands: setup, during a task, approvals, memory, sessions.
// TestMenuCommandOrder checks the order. Keep it in sync with the dispatch in
// deliver and cmdManage, and with the commands `otto acp` advertises;
// TestMenuCommandsAreHandled checks it.
var Commands = []Command{
	{"config", "Show or change configured models", false},
	{"models", "List the model IDs of a provider", false},
	{"model", "Choose the conversation's model: /model, or /model <provider>/<model_id>", true},
	{"context", "Show what the next request sends, with token counts", true},
	{"compact", "Compact the session context: /compact [focus]", true},
	{"steer", "Guide the active task: /steer <guidance>", false},
	{"queue", "Run a separate follow-up: /queue <prompt>", false},
	{"stop", "Cancel the running turn and clear the queue", false},
	{"permissions", "Approval mode and policy: /permissions [manual|auto]", false},
	{"allow", "Allow the pending permission request once", false},
	{"deny", "Deny the pending permission request", false},
	{"memory", "List or review pending memory candidates", false},
	{"new", "Start a new session on the next message", false},
	{"sessions", "List the newest sessions", false},
	{"use", "Switch to a session: /use <id>", false},
}

// deliver handles one inbound message and does not block: sends and agent
// notifications run on their own goroutines.
func (b *Bridge) deliver(m Message) {
	if !b.admitted(m) {
		slog.Info("chat_rejected", "platform", m.Platform, "chat_id", m.ChatID, "sender_id", m.SenderID,
			"reason", "chat or sender not allowed")
		b.opts.Metrics.Message(m.Platform, "rejected")
		return
	}
	p := b.platforms[m.Platform]
	if p == nil {
		return
	}
	m.RequestID = newRequestID()
	m.queueFull = new(bool)
	slog.Info("chat_received", "request_id", m.RequestID, "platform", m.Platform, "chat_id", m.ChatID,
		"sender_id", m.SenderID, "message_id", m.MessageID)
	defer func() {
		if !*m.queueFull {
			b.opts.Metrics.Message(m.Platform, "accepted")
		}
	}()
	c := b.chat(p, m.ChatID)
	if !m.Time.IsZero() && b.opts.Now().Sub(m.Time) > b.opts.StaleAfter {
		c.notify(m.MessageID, fmt.Sprintf("Message from %s was not run: it is older than %s.",
			m.Time.Format("2006-01-02 15:04:05 MST"), b.opts.StaleAfter))
		return
	}
	if m.ApprovalID != "" {
		if m.Text == "/allow" || m.Text == "/deny" {
			c.cmdApprovalButton(m)
		}
		return
	}
	text := strings.TrimSpace(m.Text)
	if arg, ok := commandArg(text, "auto"); ok {
		c.cmdAuto(m, arg)
		return
	}
	if arg, ok := commandArg(text, "permissions"); ok {
		c.cmdPermissions(m, arg)
		return
	}
	if c.cmdManage(m, text) {
		return
	}
	if arg, ok := commandArg(text, "model"); ok && c.cmdModel(m, arg) {
		return
	}
	for _, command := range []string{"/allow", "/deny"} {
		if text == command || strings.HasPrefix(text, command+" ") {
			if c.cmdSharedDecision(m, command == "/allow", strings.TrimSpace(strings.TrimPrefix(text, command))) {
				return
			}
		}
	}
	switch text {
	case "/new":
		c.cmdNew(m)
		return
	case "/stop":
		c.cmdStop(m)
		return
	case "/allow", "/deny":
		c.cmdDecide(m, text == "/allow")
		return
	case "/sessions":
		c.cmdSessions(m)
		return
	}
	if arg, ok := commandArg(text, "steer"); ok {
		c.cmdSteer(m, arg)
		return
	}
	if arg, ok := commandArg(text, "queue"); ok {
		c.cmdQueue(m, arg)
		return
	}
	if arg, ok := strings.CutPrefix(text, "/use"); ok && (arg == "" || arg[0] == ' ' || arg[0] == '\t') {
		c.cmdUse(m, strings.TrimSpace(arg))
		return
	}
	if arg, ok := strings.CutPrefix(text, "/memory"); ok && (arg == "" || arg[0] == ' ' || arg[0] == '\t') {
		c.cmdMemory(m, strings.Fields(arg))
		return
	}
	if m.Attachment {
		c.notify(m.MessageID, "Attachments are not supported and were ignored.")
	}
	if text == "" {
		return
	}
	m.Text = text
	c.mu.Lock()
	pending, sid := c.perm != nil || c.approvalRunning || c.sharedBlocking, c.sid
	if pending {
		if len(m.Text) > 32_000 {
			c.mu.Unlock()
			c.notify(m.MessageID, "Message is too long; limit is 32000 bytes.")
			return
		}
		accepted := c.enqueueApprovalMessageLocked(sid, m)
		c.mu.Unlock()
		if !accepted {
			c.queueFull(m, c.b.opts.QueueLimit)
			c.notify(m.MessageID, fmt.Sprintf("Queue is full (%d messages); the message was not queued.", c.b.opts.QueueLimit))
		}
		return
	}
	activeControl := c.taskControlAvailableLocked()
	if activeControl {
		if len(m.Text) > 32_000 {
			c.mu.Unlock()
			c.notify(m.MessageID, "Message is too long; limit is 32000 bytes.")
			return
		}
		accepted := c.enqueueTaskControlLocked(controlItem{message: m})
		c.mu.Unlock()
		if !accepted {
			c.queueFull(m, c.b.opts.QueueLimit)
			c.notify(m.MessageID, fmt.Sprintf("Queue is full (%d messages); the message was not routed to the active task.", c.b.opts.QueueLimit))
		}
		return
	}
	c.mu.Unlock()
	c.enqueue(m)
}

func commandArg(text, name string) (string, bool) {
	arg, ok := strings.CutPrefix(text, "/"+name)
	if !ok || (arg != "" && arg[0] != ' ' && arg[0] != '\t') {
		return "", false
	}
	return strings.TrimSpace(arg), true
}

func (c *chat) enqueueApprovalMessageLocked(sid string, m Message) bool {
	if len(c.queue)+len(c.approvalMessages) >= c.b.opts.QueueLimit {
		return false
	}
	c.approvalMessages = append(c.approvalMessages, m)
	if !c.approvalRunning {
		c.approvalRunning = true
		c.approvalGeneration++
		approvalCtx, cancel := context.WithCancel(c.b.workCtx)
		c.approvalCancel = cancel
		go c.approvalDialogue(sid, c.approvalGeneration, approvalCtx)
	}
	return true
}

// approvalDialogue sends messages received while a permission is pending in
// delivery order. The agent decides whether each message belongs in the turn
// queue; this path never grants permission.
func (c *chat) approvalDialogue(sid string, generation uint64, approvalCtx context.Context) {
	for {
		c.mu.Lock()
		if generation != c.approvalGeneration {
			c.mu.Unlock()
			return
		}
		if len(c.approvalMessages) == 0 {
			c.approvalRunning = false
			c.approvalCancel()
			c.approvalCancel = nil
			c.mu.Unlock()
			return
		}
		m := c.approvalMessages[0]
		c.approvalMessages = c.approvalMessages[1:]
		c.mu.Unlock()

		ctx, stop := context.WithTimeout(approvalCtx, 35*time.Second)
		reply, err := c.b.opts.Agent.ApprovalMessageWithChannel(ctx, sid, m.Text, m.Platform, chatPrompt(m))
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
		if generation != c.approvalGeneration {
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

func (b *Bridge) chat(p Platform, id string) *chat {
	key := p.Name() + ":" + id
	b.mu.Lock()
	defer b.mu.Unlock()
	c := b.chats[key]
	if c == nil {
		c = &chat{b: b, key: key, p: p, id: id}
		b.chats[key] = c
	}
	return c
}

// sessLock is a session mutex plus the number of callers holding or waiting
// for it; the map entry is deleted when the count reaches zero.
type sessLock struct {
	mu   sync.Mutex
	refs int
}

// lockSession serializes prompts on session id and returns the unlock
// function. Two chats bound to one session would otherwise overwrite each
// other in bySession and receive each other's updates. Go mutexes are not
// FIFO; the order of chats that wait together is not guaranteed.
func (b *Bridge) lockSession(id string) (unlock func()) {
	b.mu.Lock()
	l := b.sessLocks[id]
	if l == nil {
		l = &sessLock{}
		b.sessLocks[id] = l
	}
	l.refs++
	b.mu.Unlock()
	l.mu.Lock()
	return func() {
		l.mu.Unlock()
		b.mu.Lock()
		l.refs--
		if l.refs == 0 {
			delete(b.sessLocks, id)
		}
		b.mu.Unlock()
	}
}

// Update implements agent.Handler. It only takes a mutex.
func (b *Bridge) Update(sid string, u acp.SessionUpdate) {
	if text, origin, ok := agent.SessionNotice(u); ok {
		if origin == "" {
			return
		}
		b.mu.Lock()
		c := b.chats[strings.TrimPrefix(origin, "chat:")]
		b.mu.Unlock()
		if c != nil && c.presentationOrigin() == origin && b.opts.Store.Session(c.key) == sid {
			c.sendBackground(sid, text, false)
		}
		return
	}
	if text, ok := agent.WakeReply(u); ok {
		b.mu.Lock()
		chats := make([]*chat, 0, len(b.chats))
		for _, c := range b.chats {
			chats = append(chats, c)
		}
		b.mu.Unlock()
		for _, c := range chats {
			if b.opts.Store.Session(c.key) == sid {
				c.sendBackground(sid, text, true)
			}
		}
		return
	}
	b.mu.Lock()
	c := b.bySession[sid]
	b.mu.Unlock()
	if c != nil {
		c.collect(u)
	}
}

// Permission implements agent.Handler.
func (b *Bridge) Permission(ctx context.Context, sid string, req acp.RequestPermissionRequest) acp.RequestPermissionResponse {
	cancelled := acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeCancelled()}
	b.mu.Lock()
	c := b.bySession[sid]
	b.mu.Unlock()
	if c == nil {
		return cancelled
	}
	return c.askPermission(ctx, req)
}
