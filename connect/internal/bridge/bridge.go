// Package bridge connects chat platforms to one ACP agent: admission,
// per-chat queues, connector commands, replies and permission requests.
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

	"github.com/baiyuqing/otto/connect/internal/agent"
	"github.com/baiyuqing/otto/connect/internal/state"
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
	Agent     *agent.Agent
	Platforms []Platform
	Access    map[string]Access // by Platform.Name()
	Store     *state.Store

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
	}
	b.workCtx, b.stopWork = context.WithCancel(context.Background())
	for _, p := range opts.Platforms {
		b.platforms[p.Name()] = p
	}
	opts.Agent.SetHandler(b)
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
	if err := b.opts.Agent.Close(); err != nil {
		slog.Warn("closing agent", "error", err)
	}
	return first
}

// shutdown sends session/cancel for every running prompt and stops the chat
// workers, which then return without replying.
func (b *Bridge) shutdown() {
	b.mu.Lock()
	b.closing = true
	var sids []string
	for sid := range b.bySession {
		sids = append(sids, sid)
	}
	b.mu.Unlock()
	for _, sid := range sids {
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		_ = b.opts.Agent.Cancel(ctx, sid)
		cancel()
	}
	b.stopWork()
}

func (b *Bridge) admitted(m Message) bool {
	acc := b.opts.Access[m.Platform]
	return slices.Contains(acc.Chats, m.ChatID) && slices.Contains(acc.Senders, m.SenderID)
}

// deliver handles one inbound message and does not block: sends and agent
// notifications run on their own goroutines.
func (b *Bridge) deliver(m Message) {
	if !b.admitted(m) {
		slog.Info("message rejected: chat or sender not allowed",
			"platform", m.Platform, "chat", m.ChatID, "sender", m.SenderID)
		return
	}
	p := b.platforms[m.Platform]
	if p == nil {
		return
	}
	c := b.chat(p, m.ChatID)
	if !m.Time.IsZero() && b.opts.Now().Sub(m.Time) > b.opts.StaleAfter {
		c.notify(m.MessageID, fmt.Sprintf("Message from %s was not run: it is older than %s.",
			m.Time.Format("2006-01-02 15:04:05 MST"), b.opts.StaleAfter))
		return
	}
	text := strings.TrimSpace(m.Text)
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
	}
	if m.Attachment {
		c.notify(m.MessageID, "Attachments are not supported and were ignored.")
	}
	if text == "" {
		return
	}
	m.Text = text
	c.enqueue(m)
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

// Update implements agent.Handler. It only takes a mutex.
func (b *Bridge) Update(sid string, u acp.SessionUpdate) {
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

// chat is the state of one "<platform>:<chat id>".
type chat struct {
	b   *Bridge
	key string
	p   Platform
	id  string

	mu      sync.Mutex
	queue   []Message
	running bool // a worker goroutine owns the queue
	sid     string
	stopped bool // /stop arrived during the current turn
	perm    *permission
	reply   strings.Builder
	brk     bool // a tool call followed text; the next text starts a paragraph
}

type permission struct {
	decided chan decision // buffered 1
}

type decision int

const (
	allow decision = iota
	deny
	cancel
)

func (c *chat) send(ctx context.Context, replyTo, text string) {
	if err := c.p.Send(ctx, c.id, replyTo, text); err != nil && ctx.Err() == nil {
		slog.Warn("send failed", "platform", c.p.Name(), "chat", c.id, "error", err)
	}
}

// notify sends text from a new goroutine so deliver does not block.
// ponytail: notices from deliver can overtake each other; add a per-chat
// outbox if ordering of notices matters.
func (c *chat) notify(replyTo, text string) {
	go c.send(c.b.workCtx, replyTo, text)
}

func (c *chat) enqueue(m Message) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if len(c.queue) >= c.b.opts.QueueLimit {
		c.notify(m.MessageID, fmt.Sprintf("Queue is full (%d messages); the message was not queued.", c.b.opts.QueueLimit))
		return
	}
	c.queue = append(c.queue, m)
	if c.running {
		return
	}
	c.b.mu.Lock()
	closing := c.b.closing
	if !closing {
		c.b.wg.Add(1)
	}
	c.b.mu.Unlock()
	if closing {
		c.queue = nil
		return
	}
	c.running = true
	go c.work()
}

func (c *chat) work() {
	defer c.b.wg.Done()
	for {
		c.mu.Lock()
		if len(c.queue) == 0 || c.b.workCtx.Err() != nil {
			c.running = false
			c.queue = nil
			c.mu.Unlock()
			return
		}
		m := c.queue[0]
		c.queue = c.queue[1:]
		c.stopped = false
		c.mu.Unlock()
		c.turn(m)
	}
}

// turn runs one message through the agent and sends the reply.
func (c *chat) turn(m Message) {
	ctx := c.b.workCtx
	typingDone := make(chan struct{})
	defer close(typingDone)
	go c.typing(ctx, typingDone)

	var (
		sid  string
		stop acp.StopReason
		err  error
	)
	for attempt := 0; attempt < 2; attempt++ {
		if sid, err = c.session(ctx, m.MessageID); err != nil {
			break
		}
		c.mu.Lock()
		if c.stopped {
			c.mu.Unlock()
			c.send(ctx, m.MessageID, "Stopped.")
			return
		}
		c.sid = sid
		c.reply.Reset()
		c.brk = false
		c.mu.Unlock()
		c.b.mu.Lock()
		c.b.bySession[sid] = c
		c.b.mu.Unlock()

		stop, err = c.b.opts.Agent.Prompt(ctx, sid, m.Text)

		c.b.mu.Lock()
		delete(c.b.bySession, sid)
		c.b.mu.Unlock()
		if !errors.Is(err, agent.ErrNotOpen) {
			break
		}
	}
	c.mu.Lock()
	c.sid = ""
	text := strings.TrimSpace(c.reply.String())
	c.reply.Reset()
	c.mu.Unlock()
	if ctx.Err() != nil {
		return
	}
	switch {
	case err != nil:
		slog.Warn("prompt failed", "chat", c.key, "error", err)
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
		c.send(ctx, m.MessageID, text+"Stop reason: "+string(stop))
	case text != "":
		c.send(ctx, m.MessageID, text)
	}
}

// session returns the chat's session id, loading the stored one or creating
// a new one. A load that the agent refuses is replaced by a new session and a
// notice.
func (c *chat) session(ctx context.Context, replyTo string) (string, error) {
	if id := c.b.opts.Store.Session(c.key); id != "" {
		err := c.b.opts.Agent.Load(ctx, id)
		if err == nil {
			return id, nil
		}
		var ee *agent.ExitError
		if errors.As(err, &ee) || ctx.Err() != nil {
			return "", err
		}
		slog.Warn("session load failed; creating a new session", "chat", c.key, "error", err)
		c.send(ctx, replyTo, "The previous session could not be loaded; started a new one.")
	}
	id, err := c.b.opts.Agent.New(ctx)
	if err != nil {
		return "", err
	}
	if err := c.b.opts.Store.SetSession(c.key, id); err != nil {
		slog.Warn("saving session id failed", "chat", c.key, "error", err)
	}
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
	}
}

func (c *chat) cmdNew(m Message) {
	if err := c.b.opts.Store.SetSession(c.key, ""); err != nil {
		slog.Warn("clearing session id failed", "chat", c.key, "error", err)
	}
	c.notify(m.MessageID, "The next message starts a new session.")
}

func (c *chat) cmdStop(m Message) {
	c.mu.Lock()
	if !c.running {
		c.mu.Unlock()
		c.notify(m.MessageID, "Nothing is running.")
		return
	}
	c.queue = nil
	c.stopped = true
	sid := c.sid
	if c.perm != nil {
		c.perm.decided <- cancel
		c.perm = nil
	}
	c.mu.Unlock()
	// The running turn replies "Stopped." when the prompt returns
	// cancelled. ponytail: a cancel that reaches the agent before its
	// session/prompt request is ignored by it; the turn then runs to the end.
	if sid != "" {
		go func() {
			ctx, stop := context.WithTimeout(c.b.workCtx, 10*time.Second)
			defer stop()
			if err := c.b.opts.Agent.Cancel(ctx, sid); err != nil {
				slog.Warn("session/cancel failed", "chat", c.key, "error", err)
			}
		}()
	}
}

func (c *chat) cmdDecide(m Message, ok bool) {
	c.mu.Lock()
	perm := c.perm
	c.perm = nil
	c.mu.Unlock()
	if perm == nil {
		c.notify(m.MessageID, "No pending request.")
		return
	}
	if ok {
		perm.decided <- allow
		c.notify(m.MessageID, "Allowed.")
	} else {
		perm.decided <- deny
		c.notify(m.MessageID, "Denied.")
	}
}

func (c *chat) askPermission(ctx context.Context, req acp.RequestPermissionRequest) acp.RequestPermissionResponse {
	cancelled := acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeCancelled()}
	pick := func(kind acp.PermissionOptionKind) (acp.PermissionOptionId, bool) {
		for _, o := range req.Options {
			if o.Kind == kind {
				return o.OptionId, true
			}
		}
		return "", false
	}
	allowID, okA := pick(acp.PermissionOptionKindAllowOnce)
	denyID, okD := pick(acp.PermissionOptionKindRejectOnce)
	if !okA || !okD {
		slog.Warn("permission request lacks allow_once or reject_once option", "chat", c.key)
		return cancelled
	}
	c.mu.Lock()
	if c.perm != nil {
		c.mu.Unlock()
		return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeSelected(denyID)}
	}
	perm := &permission{decided: make(chan decision, 1)}
	c.perm = perm
	c.mu.Unlock()

	title := "(unnamed tool call)"
	if req.ToolCall.Title != nil {
		title = *req.ToolCall.Title
	}
	c.send(ctx, "", title+"\n\nReply /allow or /deny")

	timer := time.NewTimer(c.b.opts.PermissionTimeout)
	defer timer.Stop()
	select {
	case d := <-perm.decided:
		switch d {
		case allow:
			return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeSelected(allowID)}
		case deny:
			return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeSelected(denyID)}
		}
		return cancelled
	case <-timer.C:
	case <-ctx.Done():
	}
	// Timeout or connection end. When /allow, /deny or /stop already took the
	// request (c.perm != perm), its decision is on its way.
	c.mu.Lock()
	taken := c.perm != perm
	if !taken {
		c.perm = nil
	}
	c.mu.Unlock()
	if taken {
		switch <-perm.decided {
		case allow:
			return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeSelected(allowID)}
		case cancel:
			return cancelled
		}
		return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeSelected(denyID)}
	}
	if ctx.Err() == nil {
		c.send(ctx, "", "Permission request timed out; denied.")
	}
	return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeSelected(denyID)}
}
