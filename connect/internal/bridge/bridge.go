// Package bridge connects chat platforms to one ACP agent: admission,
// per-chat queues, connector commands, replies and permission requests.
package bridge

import (
	"context"
	"crypto/rand"
	"encoding/hex"
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
	sessLocks map[string]*sync.Mutex // one per session id; guarded by mu
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
		sessLocks: map[string]*sync.Mutex{},
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
	if m.ApprovalID != "" {
		if m.Text == "/allow" || m.Text == "/deny" {
			c.cmdDecide(m, m.Text == "/allow")
		}
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
	case "/sessions":
		c.cmdSessions(m)
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

// sessionLock returns the mutex that serializes prompts on session id. Two
// chats bound to one session would otherwise overwrite each other in
// bySession and receive each other's updates. Go mutexes are not FIFO; the
// order of chats that wait together is not guaranteed.
func (b *Bridge) sessionLock(id string) *sync.Mutex {
	b.mu.Lock()
	defer b.mu.Unlock()
	l := b.sessLocks[id]
	if l == nil {
		l = &sync.Mutex{}
		b.sessLocks[id] = l
	}
	return l
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
	using   bool // a /use is loading a session
	sid     string
	stopped bool // /stop arrived during the current turn
	perm    *permission
	reply   strings.Builder
	brk     bool // a tool call followed text; the next text starts a paragraph
	// proposed is set when the turn called remember or forget.
	proposed bool
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
	if c.using {
		c.notify(m.MessageID, "A session switch is in progress; the message was not queued.")
		return
	}
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
		lock := c.b.sessionLock(sid)
		lock.Lock()
		c.mu.Lock()
		if c.stopped {
			c.mu.Unlock()
			lock.Unlock()
			c.send(ctx, m.MessageID, "Stopped.")
			return
		}
		c.sid = sid
		c.reply.Reset()
		c.brk = false
		c.proposed = false
		c.mu.Unlock()
		c.b.mu.Lock()
		c.b.bySession[sid] = c
		c.b.mu.Unlock()

		stop, err = c.b.opts.Agent.Prompt(ctx, sid, m.Text)

		c.b.mu.Lock()
		delete(c.b.bySession, sid)
		c.b.mu.Unlock()
		lock.Unlock()
		if !errors.Is(err, agent.ErrNotOpen) {
			break
		}
	}
	c.mu.Lock()
	c.sid = ""
	text := strings.TrimSpace(c.reply.String())
	c.reply.Reset()
	proposed := c.proposed
	c.proposed = false
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
	default:
		if proposed {
			if text != "" {
				text += "\n\n"
			}
			text += memoryHint
		}
		if text != "" {
			c.send(ctx, m.MessageID, text)
		}
	}
}

// memoryHint follows a turn that proposed a memory change. Only a person can
// decide it, through the connector's commands, not by writing to the model.
const memoryHint = "Memory changes are proposals. Send /memory to review them."

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
		if t := u.ToolCall.Title; t == "remember" || t == "forget" {
			c.proposed = true
		}
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
	if m.ApprovalID != "" && (perm == nil || perm.id != m.ApprovalID) {
		c.mu.Unlock()
		c.notify("", "This approval request is no longer pending.")
		return
	}
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
	var nonce [16]byte
	if _, err := rand.Read(nonce[:]); err != nil {
		c.mu.Unlock()
		return cancelled
	}
	perm := &permission{id: hex.EncodeToString(nonce[:]), decided: make(chan decision, 1)}
	c.perm = perm
	c.mu.Unlock()

	title := "(unnamed tool call)"
	if req.ToolCall.Title != nil {
		title = *req.ToolCall.Title
	}
	text := title + "\n\nReply /allow or /deny"
	status := "Denied / expired."
	if p, ok := c.p.(ApprovalPlatform); ok {
		finish, err := p.SendApproval(ctx, c.id, perm.id, text)
		if err != nil {
			slog.Warn("approval card send failed", "chat", c.key, "error", err)
			c.send(ctx, "", text)
		} else {
			defer func() {
				cleanup, stop := context.WithTimeout(context.Background(), 10*time.Second)
				defer stop()
				if err := finish(cleanup, status); err != nil {
					slog.Warn("approval card update failed", "chat", c.key, "error", err)
				}
			}()
		}
	} else {
		c.send(ctx, "", text)
	}

	timer := time.NewTimer(c.b.opts.PermissionTimeout)
	defer timer.Stop()
	select {
	case d := <-perm.decided:
		switch d {
		case allow:
			status = "Approved."
			return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeSelected(allowID)}
		case deny:
			status = "Denied."
			return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeSelected(denyID)}
		}
		status = "Cancelled."
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
			status = "Approved."
			return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeSelected(allowID)}
		case cancel:
			status = "Cancelled."
			return cancelled
		}
		status = "Denied."
		return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeSelected(denyID)}
	}
	if ctx.Err() != nil {
		status = "Closed; answered elsewhere or cancelled."
	}
	switch {
	case ctx.Err() == nil:
		c.send(ctx, "", "Permission request timed out; denied.")
	case context.Cause(ctx) == context.Canceled && c.b.workCtx.Err() == nil:
		// The SDK cancels a request context with the cause context.Canceled
		// only for $/cancel_request; a closed connection ends it with the
		// connection's error. /stop, /allow and /deny took the request above
		// (taken), and shutdown cancels workCtx, so neither reaches here.
		c.send(c.b.workCtx, "", "permission request answered elsewhere")
	}
	return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeSelected(denyID)}
}

// cmdSessions lists up to 10 sessions of the agent's workspace.
func (c *chat) cmdSessions(m Message) {
	go func() {
		ctx, stop := context.WithTimeout(c.b.workCtx, 30*time.Second)
		defer stop()
		list, err := c.b.opts.Agent.List(ctx)
		if err != nil {
			c.send(c.b.workCtx, m.MessageID, "Error: session/list failed: "+err.Error())
			return
		}
		if len(list) == 0 {
			c.send(c.b.workCtx, m.MessageID, "No sessions.")
			return
		}
		cur := c.b.opts.Store.Session(c.key)
		var sb strings.Builder
		for i, s := range list[:min(len(list), maxListed)] {
			if i > 0 {
				sb.WriteByte('\n')
			}
			id := string(s.SessionId)
			mark := " "
			if id == cur {
				mark = "*"
			}
			fmt.Fprintf(&sb, "%s %s  %s  %s", mark, id[:min(len(id), 8)], updated(s), title(s))
		}
		c.send(c.b.workCtx, m.MessageID, sb.String())
	}()
}

const (
	maxListed    = 10
	minPrefix    = 4
	fullIDLength = 32
)

func title(s acp.SessionInfo) string {
	if s.Title == nil || *s.Title == "" {
		return "(untitled)"
	}
	return *s.Title
}

// updated formats UpdatedAt (RFC 3339) in the zone it carries.
func updated(s acp.SessionInfo) string {
	if s.UpdatedAt == nil {
		return "-"
	}
	t, err := time.Parse(time.RFC3339, *s.UpdatedAt)
	if err != nil {
		return *s.UpdatedAt
	}
	return t.Format("2006-01-02 15:04")
}

// cmdUse binds the chat to a session: it resolves id (full or a unique prefix
// of a listed session), sends session/load and stores the binding.
func (c *chat) cmdUse(m Message, arg string) {
	c.mu.Lock()
	if c.running || c.using {
		c.mu.Unlock()
		c.notify(m.MessageID, "A message is running or queued; /use is refused. Send /stop first.")
		return
	}
	c.using = true
	c.mu.Unlock()
	go func() {
		defer func() {
			c.mu.Lock()
			c.using = false
			c.mu.Unlock()
		}()
		reply := func(text string) { c.send(c.b.workCtx, m.MessageID, text) }
		if arg == "" {
			reply("Usage: /use <session id>")
			return
		}
		if len(arg) < minPrefix {
			reply(fmt.Sprintf("A session id prefix needs at least %d characters.", minPrefix))
			return
		}
		ctx, stop := context.WithTimeout(c.b.workCtx, 2*time.Minute)
		defer stop()
		list, err := c.b.opts.Agent.List(ctx)
		if err != nil {
			reply("Error: session/list failed: " + err.Error())
			return
		}
		var found []acp.SessionInfo
		for _, s := range list {
			id := string(s.SessionId)
			if id == arg || len(arg) < fullIDLength && strings.HasPrefix(id, arg) {
				found = append(found, s)
			}
		}
		id := arg
		switch {
		case len(found) > 1:
			reply(fmt.Sprintf("%q matches %d sessions; use more characters.", arg, len(found)))
			return
		case len(found) == 1:
			id = string(found[0].SessionId)
		case len(arg) < fullIDLength:
			reply(fmt.Sprintf("No listed session starts with %q.", arg))
			return
		}
		if err := c.b.opts.Agent.Load(ctx, id); err != nil {
			reply("Error: could not load session " + id + ": " + err.Error())
			return
		}
		if err := c.b.opts.Store.SetSession(c.key, id); err != nil {
			slog.Warn("saving session id failed", "chat", c.key, "error", err)
		}
		t := "(untitled)"
		if len(found) == 1 {
			t = title(found[0])
		}
		reply("Using session " + id + ": " + t)
	}()
}

const memoryUsage = "Usage: /memory | /memory accept <id> | /memory reject <id>"

// cmdMemory is the human side of memory review: it lists the chat session's
// pending candidates, or accepts or rejects one. It never goes through the
// model, and it does not wait for a running turn.
func (c *chat) cmdMemory(m Message, args []string) {
	go func() {
		reply := func(text string) { c.send(c.b.workCtx, m.MessageID, text) }
		if len(args) != 0 && (len(args) != 2 || args[0] != "accept" && args[0] != "reject") {
			reply(memoryUsage)
			return
		}
		sid := c.b.opts.Store.Session(c.key)
		if sid == "" {
			reply("This chat has no session yet; send a message first.")
			return
		}
		ctx, stop := context.WithTimeout(c.b.workCtx, 2*time.Minute)
		defer stop()
		if err := c.b.opts.Agent.Load(ctx, sid); err != nil {
			reply("Error: could not load session " + sid + ": " + err.Error())
			return
		}
		if len(args) == 0 {
			page, err := c.b.opts.Agent.MemoryPending(ctx, sid, "")
			if err != nil {
				reply(memoryError(err))
				return
			}
			reply(renderPending(page))
			return
		}
		id, err := c.findCandidate(ctx, sid, args[1])
		if err != nil {
			reply(memoryError(err))
			return
		}
		if id == "" {
			reply(fmt.Sprintf("No single pending candidate starts with %q.", args[1]))
			return
		}
		res, err := c.b.opts.Agent.MemoryReview(ctx, sid, id, args[0])
		if err != nil {
			reply(memoryError(err))
			return
		}
		short := id[:min(len(id), shortID)]
		switch {
		case res.Decision == "reject":
			reply("Rejected " + short + ".")
		case res.Record != nil:
			reply(fmt.Sprintf("Accepted %s as record %s (revision %d).", short, res.Record.ID, res.Record.Revision))
		case res.Forgotten != "":
			reply(fmt.Sprintf("Accepted %s: forgot %s.", short, res.Forgotten))
		default:
			reply("Accepted " + short + ".")
		}
	}()
}

const (
	shortID       = 8
	candidateText = 200
)

func memoryError(err error) string {
	switch {
	case errors.Is(err, agent.ErrMemoryUnsupported):
		return "This agent does not support memory review."
	case errors.Is(err, agent.ErrMemoryUnavailable):
		return "Memory is not available in this session."
	case errors.Is(err, agent.ErrMemoryConflict):
		return "That candidate was already decided or changed; send /memory to refresh."
	case errors.Is(err, agent.ErrCandidateNotFound):
		return "That candidate no longer exists; send /memory to refresh."
	}
	var ee *agent.ExitError
	if errors.As(err, &ee) {
		return ee.Error()
	}
	return "Error: " + err.Error()
}

func renderPending(page agent.MemoryPage) string {
	list := page.Candidates
	if len(list) == 0 {
		return "No pending memory candidates."
	}
	var sb strings.Builder
	fmt.Fprintf(&sb, "%d pending memory candidates (reply /memory accept <id> or /memory reject <id>):", len(list))
	for _, cand := range list {
		text := strings.Join(strings.Fields(cand.Text), " ")
		if r := []rune(text); len(r) > candidateText {
			text = string(r[:candidateText]) + "…"
		}
		fmt.Fprintf(&sb, "\n%s  %s %s/%s  %s  (%s)", cand.ID[:min(len(cand.ID), shortID)], cand.Action, cand.Kind, cand.Key, text, cand.Origin)
	}
	if page.NextCursor != "" {
		sb.WriteString("\nMore are pending; decide some, then send /memory again.")
	}
	return sb.String()
}

// maxPendingPages bounds the search for a candidate id across pages.
const maxPendingPages = 10

// findCandidate returns the id of the one pending candidate that arg names,
// either in full or as a prefix of at least minPrefix characters, reading
// pages until it is found or the pages end. It returns "" when no single
// candidate matches.
func (c *chat) findCandidate(ctx context.Context, sid, arg string) (string, error) {
	var matches []string
	cursor := ""
	for range maxPendingPages {
		page, err := c.b.opts.Agent.MemoryPending(ctx, sid, cursor)
		if err != nil {
			return "", err
		}
		for _, cand := range page.Candidates {
			if cand.ID == arg || len(arg) >= minPrefix && strings.HasPrefix(cand.ID, arg) {
				matches = append(matches, cand.ID)
			}
		}
		if page.NextCursor == "" {
			break
		}
		cursor = page.NextCursor
	}
	if len(matches) != 1 {
		return "", nil
	}
	return matches[0], nil
}
