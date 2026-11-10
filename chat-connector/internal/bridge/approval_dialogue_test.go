package bridge

import (
	"context"
	"fmt"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/chat-connector/internal/agent/fake"
)

func waitApprovalDialogue(t *testing.T, dialogue *approvalDialogue) {
	t.Helper()
	select {
	case <-dialogue.done:
	case <-time.After(20 * time.Second):
		t.Fatal("approval dialogue did not settle")
	}
}

func TestApprovalDialogueStopAndStaleCompletionOwnOnlyCapturedRecord(t *testing.T) {
	t.Parallel()
	c := &chat{sid: "old"}
	c.mu.Lock()
	old := c.beginApprovalDialogueLocked(context.Background(), "old")
	old.messages = []Message{{Text: "discard"}}
	c.stopApprovalDialogueLocked()
	c.stopApprovalDialogueLocked()
	if old.ctx.Err() == nil || len(old.messages) != 0 || c.dialogue != nil {
		t.Fatal("stop did not revoke the dialogue and its pending input")
	}
	select {
	case <-old.done:
		t.Fatal("stop completed a worker before it settled")
	default:
	}
	c.sid = "new"
	current := c.beginApprovalDialogueLocked(context.Background(), "new")
	t.Cleanup(current.cancel)
	current.messages = []Message{{Text: "keep"}}
	c.mu.Unlock()
	// A stale worker must return before attempting an RPC; c has no agent.
	c.runApprovalDialogue(old)
	c.mu.Lock()
	defer c.mu.Unlock()
	c.finishApprovalDialogueLocked(old)
	if c.dialogue != current || !c.approvalDialogueCurrentLocked(current) || len(current.messages) != 1 {
		t.Fatal("stale completion changed the new owner or queue")
	}
	select {
	case <-current.done:
		t.Fatal("stale completion closed the new owner's completion channel")
	default:
	}
	waitApprovalDialogue(t, old)
	c.finishApprovalDialogueLocked(current)
}

func TestApprovalDialogueOwnsConcurrentFIFOThroughReply(t *testing.T) {
	t.Parallel()
	h, gate := activityGateSetup(t, setup{env: []string{"FAKE_APPROVAL_DIALOGUE=1"}, mod: func(opts *Options) {
		opts.QueueLimit = 32
	}}, func(text string) bool { return text == "The agent needs approval to run this tool call." })
	h.say("perm:ls")
	h.waitSent(1)
	h.say("Why do you need this?")
	waitActivityGate(t, gate)
	c := h.b.chat(gate, "c1")
	c.mu.Lock()
	dialogue := c.dialogue
	c.mu.Unlock()
	var enqueues sync.WaitGroup
	var accepted []string
	for index := range 12 {
		enqueues.Go(func() {
			text := fmt.Sprintf("approval-follow-up-%02d", index)
			c.mu.Lock()
			defer c.mu.Unlock()
			if c.enqueueApprovalMessageLocked(Message{Text: text, Platform: "fake", ChatID: "c1", MessageID: text}) {
				accepted = append(accepted, "approval:"+text)
			}
			if c.dialogue != dialogue {
				t.Error("enqueue replaced the worker before its reply settled")
			}
		})
	}
	enqueues.Wait()
	if len(accepted) != 12 || len(h.calls("approval:")) != 1 {
		t.Fatal("concurrent input was lost or overtook reply delivery")
	}
	gate.unblock()
	waitApprovalDialogue(t, dialogue)
	if got, want := h.calls("approval:"), append([]string{"approval:Why do you need this?"}, accepted...); !slices.Equal(got, want) {
		t.Fatalf("dialogue order = %q, want %q", got, want)
	}
	c.mu.Lock()
	if c.dialogue != nil || len(c.queue) != 12 || c.perm == nil {
		t.Error("completion lost follow-ups or decided the permission")
	}
	c.mu.Unlock()
	if got := h.calls("perm:"); len(got) != 0 {
		t.Fatalf("natural dialogue decided permission: %q", got)
	}
	// Enqueue after the empty-queue completion must start a fresh owner.
	h.say("Why do you need this?")
	h.waitFor(func() bool { return len(h.calls("approval:")) == 14 }, "fresh dialogue after drain")
	h.waitSent(15)
	h.say("/deny")
	h.waitFor(func() bool { return len(h.calls("start:")) == 13 }, "follow-ups after denial")
	waitChatIdle(h, c)
}

func TestApprovalDialogueCapacityAndClosingAdmission(t *testing.T) {
	t.Parallel()
	h, gate := activityGateSetup(t, setup{env: []string{"FAKE_APPROVAL_DIALOGUE=1"}, mod: func(opts *Options) {
		opts.QueueLimit = 3
	}}, func(text string) bool { return text == "The agent needs approval to run this tool call." })
	h.say("perm:ls")
	h.waitSent(1)
	h.say("Why do you need this?")
	waitActivityGate(t, gate)
	h.say("/queue separate")
	c := h.b.chat(gate, "c1")
	c.mu.Lock()
	defer c.mu.Unlock()
	for _, text := range []string{"first", "second"} {
		if !c.enqueueApprovalMessageLocked(Message{Text: text}) {
			t.Fatal("input below combined queue limit was refused")
		}
	}
	if c.enqueueApprovalMessageLocked(Message{Text: "overflow"}) {
		t.Fatal("approval input bypassed combined queue limit")
	}
	c.dialogue.messages = nil
	h.b.mu.Lock()
	h.b.closing = true
	h.b.mu.Unlock()
	if c.enqueueApprovalMessageLocked(Message{Text: "closing-existing"}) {
		t.Fatal("shutdown admitted input to an existing worker")
	}
	c.stopApprovalDialogueLocked()
	if c.enqueueApprovalMessageLocked(Message{Text: "closing-new"}) {
		t.Fatal("shutdown admitted a new worker")
	}
}

func TestApprovalDialogueUsesSavedSessionForBackgroundApproval(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{env: []string{"FAKE_APPROVAL_DIALOGUE=1"}, chats: []string{"c1", "c2"}})
	h.say("perm:ls")
	h.waitSent(1)
	if err := h.store.SetSession("fake:c2", h.store.Session("fake:c1")); err != nil {
		t.Fatal(err)
	}
	c := h.b.chat(h.plat, "c2")
	c.mu.Lock()
	c.sharedBlocking = true
	c.mu.Unlock()
	h.plat.deliver(h.msg("c2", "u1", "Why do you need this?"))
	h.waitSent(2)
	if got := h.calls("approval:"); !slices.Equal(got, []string{"approval:Why do you need this?"}) {
		t.Fatalf("background dialogue missed the bound approval: %q", got)
	}
	if got := h.calls("perm:"); len(got) != 0 {
		t.Fatalf("background dialogue decided permission: %q", got)
	}
	h.say("/deny")
	h.waitCall("perm:reject_once")
}

func TestApprovalDialogueDropsResultAfterSessionScopeChanges(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{env: []string{"FAKE_APPROVAL_DIALOGUE=1"}})
	h.say("perm:ls")
	h.waitSent(1)
	h.say("hold-approval")
	h.waitCall("approval:hold-approval")
	c := h.b.chat(h.plat, "c1")
	c.mu.Lock()
	dialogue := c.dialogue
	// Change scope without cancellation to prove the result-adoption guard.
	c.sid = "replacement-session"
	c.mu.Unlock()
	if err := fake.Release(h.dir); err != nil {
		t.Fatal(err)
	}
	waitApprovalDialogue(t, dialogue)
	c.mu.Lock()
	queued, current := len(c.queue), c.dialogue
	c.sid = dialogue.sessionID
	c.mu.Unlock()
	if queued != 0 || current != nil || len(h.plat.texts()) != 1 {
		t.Fatal("stale RPC result queued input or published a reply in the new scope")
	}
	h.say("/deny")
	h.waitCall("perm:reject_once")
}

func TestApprovalDialogueDoesNotReplayUnknownErrors(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{env: []string{"FAKE_APPROVAL_DIALOGUE=1"}})
	h.say("perm:ls")
	h.waitSent(1)
	h.say("approval-error")
	if got := h.waitSent(2)[1]; !strings.Contains(got, "Could not handle the approval message") {
		t.Fatalf("unknown error reply = %q", got)
	}
	c := h.b.chat(h.plat, "c1")
	h.waitFor(func() bool {
		c.mu.Lock()
		defer c.mu.Unlock()
		return c.dialogue == nil
	}, "failed dialogue release")
	c.mu.Lock()
	queued := len(c.queue)
	c.mu.Unlock()
	if queued != 0 {
		t.Fatal("unknown error replayed input as a follow-up")
	}
}

func TestApprovalDialogueStopCancelsRPCAndQueuedInput(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{env: []string{"FAKE_APPROVAL_DIALOGUE=1"}})
	h.say("perm:ls")
	h.waitSent(1)
	h.say("hold-approval")
	h.waitCall("approval:hold-approval")
	h.say("discard-after-stop")
	c := h.b.chat(h.plat, "c1")
	c.mu.Lock()
	dialogue := c.dialogue
	c.mu.Unlock()
	h.say("/stop")
	waitApprovalDialogue(t, dialogue)
	waitChatIdle(h, c)
	if got := h.calls("approval:"); !slices.Equal(got, []string{"approval:hold-approval"}) {
		t.Fatalf("stopped dialogue dispatched queued input: %q", got)
	}
	h.say("fresh")
	h.waitCall("start:fresh")
	waitChatIdle(h, c)
	if got := h.calls("start:"); !slices.Equal(got, []string{"start:perm:ls", "start:fresh"}) {
		t.Fatalf("canceled dialogue replayed input: %q", got)
	}
}

func TestApprovalDialogueNewKeepsLiveScopeUntilPromptFinishes(t *testing.T) {
	t.Parallel()
	h, gate := activityGateSetup(t, setup{env: []string{"FAKE_APPROVAL_DIALOGUE=1"}}, func(text string) bool {
		return text == "The agent needs approval to run this tool call."
	})
	h.say("perm:ls")
	h.waitSent(1)
	h.say("Why do you need this?")
	waitActivityGate(t, gate)
	c := h.b.chat(gate, "c1")
	c.mu.Lock()
	dialogue := c.dialogue
	c.mu.Unlock()
	h.say("/new")
	if dialogue.ctx.Err() != nil {
		t.Fatal("/new canceled dialogue for a prompt that is still live")
	}
	h.say("/deny")
	waitChatIdle(h, c)
	waitApprovalDialogue(t, dialogue)
	if slices.Contains(h.plat.texts(), "The agent needs approval to run this tool call.") {
		t.Fatal("old session's admitted reply survived the live prompt's completion")
	}
	h.say("fresh")
	h.waitCall("start:fresh")
	waitChatIdle(h, c)
	if h.store.Session("fake:c1") == dialogue.sessionID {
		t.Fatal("/new did not create a fresh session")
	}
}

func TestApprovalDialogueBindingCommandsCancelBackgroundOwner(t *testing.T) {
	t.Parallel()
	for _, command := range []string{"/new", "/use " + sid("bbbb2222")} {
		t.Run(command, func(t *testing.T) {
			t.Parallel()
			dir := t.TempDir()
			seedSessions(t, dir, seedLine(sid("bbbb2222"), "other"))
			h, gate := activityGateSetup(t, setup{dir: dir, env: []string{"FAKE_APPROVAL_DIALOGUE=1"}, chats: []string{"c1", "c2"}}, func(text string) bool {
				return text == "The agent needs approval to run this tool call."
			})
			h.say("perm:ls")
			h.waitSent(1)
			if err := h.store.SetSession("fake:c2", h.store.Session("fake:c1")); err != nil {
				t.Fatal(err)
			}
			c := h.b.chat(gate, "c2")
			c.mu.Lock()
			c.sharedBlocking = true
			c.mu.Unlock()
			h.plat.deliver(h.msg("c2", "u1", "Why do you need this?"))
			waitActivityGate(t, gate)
			c.mu.Lock()
			dialogue := c.dialogue
			c.mu.Unlock()
			h.plat.deliver(h.msg("c2", "u1", command))
			waitApprovalDialogue(t, dialogue)
			waitChatIdle(h, c)
			want := ""
			if strings.HasPrefix(command, "/use") {
				want = sid("bbbb2222")
			}
			if dialogue.ctx.Err() == nil || h.store.Session("fake:c2") != want {
				t.Fatal("binding change did not revoke the previous dialogue owner")
			}
			if slices.Contains(h.plat.texts(), "The agent needs approval to run this tool call.") {
				t.Fatal("previous session's canceled reply was published after rebinding")
			}
			h.say("/deny")
			h.waitCall("perm:reject_once")
		})
	}
}

// This platform deliberately keeps an admitted Send alive after cancellation.
// Shutdown must drain it before closing the agent and returning from Run.
type approvalDrainGate struct {
	Platform
	entered   chan struct{}
	cancelled chan struct{}
	release   chan struct{}
	once      sync.Once
}

func (p *approvalDrainGate) Send(ctx context.Context, chatID, replyTo, text string, markdown bool) error {
	if text == "The agent needs approval to run this tool call." {
		close(p.entered)
		<-ctx.Done()
		close(p.cancelled)
		<-p.release
		return ctx.Err()
	}
	return p.Platform.Send(ctx, chatID, replyTo, text, markdown)
}

func TestApprovalDialogueShutdownDrainsAdmittedReply(t *testing.T) {
	t.Parallel()
	gate := &approvalDrainGate{entered: make(chan struct{}), cancelled: make(chan struct{}), release: make(chan struct{})}
	h := newHarness(t, setup{env: []string{"FAKE_APPROVAL_DIALOGUE=1"}, mod: func(opts *Options) {
		gate.Platform = opts.Platforms[0]
		opts.Platforms = []Platform{gate}
	}})
	t.Cleanup(func() { gate.once.Do(func() { close(gate.release) }) })
	h.say("perm:ls")
	h.waitSent(1)
	h.say("Why do you need this?")
	select {
	case <-gate.entered:
	case <-time.After(20 * time.Second):
		t.Fatal("dialogue reply did not reach platform")
	}
	c := h.b.chat(gate, "c1")
	c.mu.Lock()
	dialogue := c.dialogue
	c.mu.Unlock()
	h.stop()
	select {
	case <-gate.cancelled:
	case <-time.After(20 * time.Second):
		t.Fatal("shutdown did not cancel dialogue delivery")
	}
	select {
	case <-dialogue.done:
		t.Fatal("cancellation completed a reply still owned by the platform")
	case err := <-h.done:
		h.done <- err
		t.Fatal("Run returned while an admitted dialogue reply was unsettled")
	default:
	}
	gate.once.Do(func() { close(gate.release) })
	if err := h.shutdown(); err != nil {
		t.Fatal(err)
	}
	select {
	case <-dialogue.done:
	default:
		t.Fatal("Run did not drain the approval worker before returning")
	}
	if got := h.plat.texts(); len(got) != 1 {
		t.Fatalf("shutdown published a canceled dialogue reply: %q", got)
	}
}
