package bridge

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"log/slog"
	"strings"
	"time"

	"github.com/baiyuqing/otto/chat-connector/internal/agent"
	"github.com/coder/acp-go-sdk"
)

func (c *chat) cmdNew(m Message) {
	c.mu.Lock()
	if err := c.b.opts.Store.SetSession(c.key, ""); err != nil {
		slog.Warn("clearing session id failed", "chat", c.key, "error", err)
	}
	c.cancelReboundApprovalDialogueLocked()
	c.mu.Unlock()
	c.notify(m.MessageID, "The next message starts a new session.")
}

func (c *chat) cmdStop(m Message) {
	c.mu.Lock()
	wasRunning := c.requestStopLocked()
	if !wasRunning && c.dialogue == nil {
		c.mu.Unlock()
		c.notify(m.MessageID, "Nothing is running.")
		return
	}
	c.queue = nil
	c.stopTaskControlLocked()
	c.stopApprovalDialogueLocked()

	sid := c.sid
	if c.perm != nil {
		c.perm.decided <- cancel
		c.perm = nil
	}
	c.mu.Unlock()
	// The running turn replies "Stopped." when the prompt returns
	// cancelled. ponytail: a cancel that reaches the agent before its
	// session/prompt request is ignored by it; the turn then runs to the end.
	if wasRunning && sid != "" {
		go func() {
			ctx, stop := context.WithTimeout(c.b.workCtx, 10*time.Second)
			defer stop()
			if err := c.b.opts.Agent.Cancel(ctx, sid); err != nil {
				slog.Warn("session/cancel failed", "chat", c.key, "error", err)
			}
		}()
	} else if !wasRunning {
		c.notify(m.MessageID, "Stopped.")
	}
}

func (c *chat) cmdSteer(m Message, guidance string) {
	if guidance == "" {
		c.notify(m.MessageID, "Usage: /steer <guidance>")
		return
	}
	if len(guidance) > 32_000 {
		c.notify(m.MessageID, "Guidance is too long; limit is 32000 bytes.")
		return
	}
	m.Text = guidance
	c.mu.Lock()
	if c.perm != nil || c.approvalDialogueAvailableLocked() || c.taskControlPhaseLocked() != taskControlActive || c.sid == "" {
		c.mu.Unlock()
		c.notify(m.MessageID, "/steer only works during an active task without a pending approval.")
		return
	}
	accepted := c.enqueueTaskControlLocked(controlItem{message: m, steer: true, text: guidance})
	c.mu.Unlock()
	if !accepted {
		c.notify(m.MessageID, fmt.Sprintf("Active-task control queue is full (%d messages); guidance was not accepted.", c.b.opts.QueueLimit))
	}
}

func (c *chat) cmdQueue(m Message, prompt string) {
	if prompt == "" {
		c.notify(m.MessageID, "Usage: /queue <prompt>")
		return
	}
	if len(prompt) > 32_000 {
		c.notify(m.MessageID, "Prompt is too long; limit is 32000 bytes.")
		return
	}
	if m.Attachment {
		c.notify(m.MessageID, "Attachments are not supported and were ignored.")
	}
	m.Text = prompt
	if c.enqueue(m) {
		c.notify(m.MessageID, "Accepted as a separate prompt; its answer will be sent separately.")
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
	if !okA {
		allowID, okA = pick(acp.PermissionOptionKindAllowAlways)
	}
	denyID, okD := pick(acp.PermissionOptionKindRejectOnce)
	if !okA || !okD {
		slog.Warn("permission request lacks allow or reject_once option", "chat", c.key)
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
	if !c.beginSwitchLocked() {
		c.mu.Unlock()
		c.notify(m.MessageID, "A message is running or queued; /use is refused. Send /stop first.")
		return
	}
	c.mu.Unlock()
	go func() {
		defer func() {
			c.mu.Lock()
			c.finishSwitchLocked()
			c.mu.Unlock()
		}()
		c.memoryMu.Lock()
		defer c.memoryMu.Unlock()
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
		if err := c.b.load(ctx, id); err != nil {
			reply("Error: could not load session " + id + ": " + err.Error())
			return
		}
		c.mu.Lock()
		if err := c.b.opts.Store.SetSession(c.key, id); err != nil {
			slog.Warn("saving session id failed", "chat", c.key, "error", err)
		}
		c.cancelReboundApprovalDialogueLocked()
		c.mu.Unlock()
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
	if len(args) == 1 && args[0] == "review" {
		args = nil
	}
	go func() {
		c.memoryMu.Lock()
		defer c.memoryMu.Unlock()
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
		if err := c.b.load(ctx, sid); err != nil {
			reply("Error: could not load session " + sid + ": " + err.Error())
			return
		}
		if len(args) == 0 {
			c.showMemory(ctx, sid, m.MessageID, true)
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
		if err == nil || errors.Is(err, agent.ErrMemoryConflict) || errors.Is(err, agent.ErrCandidateNotFound) {
			c.finishMemory(ctx, sid, id, memoryStatus(args[0], err))
		}
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
		if cand.Advice != "" {
			fmt.Fprintf(&sb, "\n    %s", cand.Advice)
		}
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

// chatInputID is the idempotency key Otto receives for a chat message:
// chat:<platform>:<chat id>:<message id>, so a platform redelivery or a
// connector retry after a restart runs the message once. Characters outside
// Otto's input-id alphabet become '_'; it is empty without a message id or
// when longer than 128 bytes.
func chatInputID(m Message) string {
	if m.MessageID == "" {
		return ""
	}
	raw := "chat:" + m.Platform + ":" + m.ChatID + ":" + m.MessageID
	if len(raw) > 128 {
		return ""
	}
	out := []byte(raw)
	for i, c := range out {
		ok := c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' ||
			c == '.' || c == '_' || c == ':' || c == '-'
		if !ok {
			out[i] = '_'
		}
	}
	return string(out)
}
