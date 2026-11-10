package bridge

import (
	"context"
	"fmt"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestChatActivityTransitionsPreserveOwnership(t *testing.T) {
	t.Parallel()
	t.Run("worker retains ownership across stop", func(t *testing.T) {
		c := &chat{}
		c.mu.Lock()
		defer c.mu.Unlock()
		if !c.beginWorkerLocked() || !c.workerOwnedLocked() || c.switchingLocked() || c.stopRequestedLocked() {
			t.Fatal("idle chat did not become the sole worker owner")
		}
		if c.beginWorkerLocked() || c.beginSwitchLocked() {
			t.Fatal("working chat admitted another owner")
		}
		if !c.requestStopLocked() || !c.requestStopLocked() || !c.stopRequestedLocked() || !c.workerOwnedLocked() {
			t.Fatal("repeated stop lost worker ownership or stop intent")
		}
		c.finishSwitchLocked()
		if !c.stopRequestedLocked() || c.beginSwitchLocked() || c.beginWorkerLocked() {
			t.Fatal("switch completion or admission stole the stopping worker")
		}
		c.beginMessageLocked()
		if !c.workerOwnedLocked() || c.stopRequestedLocked() {
			t.Fatal("next dequeued message did not retain its worker with fresh stop intent")
		}
		c.finishWorkerLocked()
		if c.activity != chatIdle || c.workerOwnedLocked() || c.stopRequestedLocked() {
			t.Fatal("worker completion did not return the chat to idle")
		}
	})
	t.Run("switch rejects unrelated ownership changes", func(t *testing.T) {
		c := &chat{}
		c.mu.Lock()
		defer c.mu.Unlock()
		if c.requestStopLocked() || c.activity != chatIdle {
			t.Fatal("approval-only stop claimed an idle queue")
		}
		if !c.beginSwitchLocked() || !c.switchingLocked() || c.workerOwnedLocked() || c.stopRequestedLocked() {
			t.Fatal("idle chat did not become the sole switch owner")
		}
		if c.beginSwitchLocked() || c.beginWorkerLocked() || c.requestStopLocked() {
			t.Fatal("switch admitted another owner or claimed a queue stop")
		}
		c.finishWorkerLocked()
		c.beginMessageLocked()
		if !c.switchingLocked() {
			t.Fatal("worker completion or dequeue released switch ownership")
		}
		c.finishSwitchLocked()
		if c.activity != chatIdle || c.switchingLocked() {
			t.Fatal("switch completion did not return the chat to idle")
		}
		if !c.beginWorkerLocked() {
			t.Fatal("completed switch prevented a fresh worker")
		}
	})
}

// activityReplyGate holds one selected reply before the platform records it.
// Other replies remain observable while the owner is waiting for delivery.
type activityReplyGate struct {
	Platform
	match       func(string) bool
	entered     chan struct{}
	release     chan struct{}
	once        sync.Once
	releaseOnce sync.Once
}

func (p *activityReplyGate) Send(ctx context.Context, chatID, replyTo, text string, markdown bool) error {
	if p.match(text) {
		p.once.Do(func() { close(p.entered) })
		select {
		case <-p.release:
		case <-ctx.Done():
			return ctx.Err()
		}
	}
	return p.Platform.Send(ctx, chatID, replyTo, text, markdown)
}

func (p *activityReplyGate) unblock() {
	p.releaseOnce.Do(func() { close(p.release) })
}

func activityGateSetup(t *testing.T, s setup, match func(string) bool) (*harness, *activityReplyGate) {
	t.Helper()
	gate := &activityReplyGate{match: match, entered: make(chan struct{}), release: make(chan struct{})}
	previousMod := s.mod
	s.mod = func(opts *Options) {
		if previousMod != nil {
			previousMod(opts)
		}
		gate.Platform = opts.Platforms[0]
		opts.Platforms = []Platform{gate}
	}
	h := newHarness(t, s)
	// Gate cleanup must run before harness shutdown, even after an assertion fails.
	t.Cleanup(gate.unblock)
	return h, gate
}

func waitActivityGate(t *testing.T, gate *activityReplyGate) {
	t.Helper()
	select {
	case <-gate.entered:
	case <-time.After(20 * time.Second):
		t.Fatal("reply did not reach delivery gate")
	}
}

func requireChatActivity(t *testing.T, c *chat, want chatActivity) {
	t.Helper()
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.activity != want {
		t.Fatalf("activity = %v, want %v", c.activity, want)
	}
}

func waitChatIdle(h *harness, c *chat) {
	h.t.Helper()
	h.waitFor(func() bool {
		c.mu.Lock()
		defer c.mu.Unlock()
		return c.activity == chatIdle
	}, "chat worker release")
}

func TestChatWorkerOwnsConcurrentQueueThroughReply(t *testing.T) {
	t.Parallel()
	h, gate := activityGateSetup(t, setup{mod: func(opts *Options) { opts.QueueLimit = 32 }}, func(text string) bool { return text == "echo: first" })
	h.say("first")
	waitActivityGate(t, gate)
	c := h.b.chat(gate, "c1")
	requireChatActivity(t, c, chatWorking)
	var queued []string
	var enqueueWG sync.WaitGroup
	for index := range 12 {
		enqueueWG.Go(func() {
			text := fmt.Sprintf("queued-%02d", index)
			c.mu.Lock()
			defer c.mu.Unlock()
			if c.enqueueLocked(Message{Platform: "fake", ChatID: "c1", SenderID: "u1", MessageID: text, Text: text}) {
				queued = append(queued, "start:"+text)
			}
		})
	}
	enqueueWG.Wait()
	if len(queued) != 12 {
		t.Fatalf("accepted messages = %d, want 12", len(queued))
	}
	if got := h.calls("start:"); !slices.Equal(got, []string{"start:first"}) {
		t.Fatalf("another worker started before reply delivery: %q", got)
	}
	h.say("/use aaaa")
	if got := h.waitSent(1)[0]; !strings.Contains(got, "running or queued") {
		t.Fatalf("/use during reply = %q", got)
	}
	gate.unblock()
	h.waitSent(14)
	waitChatIdle(h, c)
	if got, want := h.calls("start:"), append([]string{"start:first"}, queued...); !slices.Equal(got, want) {
		t.Fatalf("prompt order = %q, want %q", got, want)
	}
}

func TestChatStopBeforePromptRetainsWorkerUntilReply(t *testing.T) {
	t.Parallel()
	for _, barrier := range []string{"session load", "session lock"} {
		t.Run(barrier, func(t *testing.T) {
			t.Parallel()
			dir := t.TempDir()
			id := sid("aaaa1111")
			seedSessions(t, dir, seedLine(id, "alpha"))
			h, gate := activityGateSetup(t, setup{dir: dir}, func(text string) bool { return text == "Stopped." })
			if err := h.store.SetSession("fake:c1", id); err != nil {
				t.Fatal(err)
			}
			c := h.b.chat(gate, "c1")
			var releaseBarrier func()
			if barrier == "session load" {
				h.b.loadMu.Lock()
				releaseBarrier = h.b.loadMu.Unlock
			} else {
				releaseBarrier = h.b.lockSession(id)
			}
			var barrierOnce sync.Once
			t.Cleanup(func() { barrierOnce.Do(releaseBarrier) })
			h.say("must-not-start")
			h.waitFor(func() bool {
				if barrier == "session load" {
					c.mu.Lock()
					defer c.mu.Unlock()
					return c.turnDone != nil
				}
				h.b.mu.Lock()
				defer h.b.mu.Unlock()
				return h.b.sessLocks[id].refs == 2
			}, "worker at pre-prompt barrier")
			h.say("/stop")
			h.say("/stop")
			requireChatActivity(t, c, chatStopping)
			h.say("after-stop")
			h.say("/use aaaa")
			if got := h.waitSent(1)[0]; !strings.Contains(got, "running or queued") {
				t.Fatalf("/use during stop = %q", got)
			}
			barrierOnce.Do(releaseBarrier)
			waitActivityGate(t, gate)
			requireChatActivity(t, c, chatStopping)
			if got := h.calls("start:"); len(got) != 0 {
				t.Fatalf("prompt started despite stop before session acquisition: %q", got)
			}
			h.say("/use aaaa")
			if got := h.waitSent(2)[1]; !strings.Contains(got, "running or queued") {
				t.Fatalf("/use during stopped reply = %q", got)
			}
			gate.unblock()
			h.waitSent(4)
			waitChatIdle(h, c)
			if got := h.calls("start:"); !slices.Equal(got, []string{"start:after-stop"}) {
				t.Fatalf("fresh message did not run exactly once: %q", got)
			}
			if got := h.plat.texts(); !slices.Contains(got, "echo: after-stop") {
				t.Fatalf("fresh message inherited stop intent: %q", got)
			}
		})
	}
}

func TestChatCancelledPromptOwnsStoppedReply(t *testing.T) {
	t.Parallel()
	h, gate := activityGateSetup(t, setup{}, func(text string) bool { return text == "Stopped." })
	h.say("block")
	h.waitCall("start:block")
	h.say("/stop")
	waitActivityGate(t, gate)
	c := h.b.chat(gate, "c1")
	requireChatActivity(t, c, chatStopping)
	h.say("/stop")
	h.say("next")
	h.say("/use aaaa")
	if got := h.waitSent(1)[0]; !strings.Contains(got, "running or queued") {
		t.Fatalf("/use during cancelled reply = %q", got)
	}
	if got := h.calls("start:"); !slices.Equal(got, []string{"start:block"}) {
		t.Fatalf("new worker overtook cancelled reply: %q", got)
	}
	gate.unblock()
	h.waitSent(3)
	waitChatIdle(h, c)
	if got := h.calls("start:"); !slices.Equal(got, []string{"start:block", "start:next"}) {
		t.Fatalf("prompt sequence after cancellation = %q", got)
	}
}

func TestChatSwitchOwnsSuccessAndFailureReplies(t *testing.T) {
	t.Parallel()
	for _, succeed := range []bool{true, false} {
		t.Run(fmt.Sprint(succeed), func(t *testing.T) {
			t.Parallel()
			dir := t.TempDir()
			id := sid("aaaa1111")
			seedSessions(t, dir, seedLine(id, "alpha"))
			arg, prefix := id, "Using session "
			if !succeed {
				arg, prefix = sid("bbbb2222"), "Error: could not load session "
			}
			h, gate := activityGateSetup(t, setup{dir: dir}, func(text string) bool { return strings.HasPrefix(text, prefix) })
			h.say("/use " + arg)
			waitActivityGate(t, gate)
			c := h.b.chat(gate, "c1")
			requireChatActivity(t, c, chatSwitching)
			// Independent approval cancellation must not release a switch.
			c.mu.Lock()
			c.beginApprovalDialogueLocked(h.b.workCtx, "")
			c.mu.Unlock()
			h.say("/stop")
			h.say("/stop")
			requireChatActivity(t, c, chatSwitching)
			h.say("must-not-queue")
			h.say("/use aaaa")
			h.waitSent(4)
			if got := strings.Join(h.plat.texts(), "|"); !strings.Contains(got, "session switch is in progress") || !strings.Contains(got, "running or queued") {
				t.Fatalf("switch did not exclude queue and competing switch: %q", got)
			}
			c.mu.Lock()
			queued := len(c.queue)
			c.mu.Unlock()
			if queued != 0 {
				t.Fatalf("switch accepted %d normal messages", queued)
			}
			gate.unblock()
			h.waitSent(5)
			waitChatIdle(h, c)
			wantBinding := ""
			if succeed {
				wantBinding = id
			}
			if got := h.store.Session("fake:c1"); got != wantBinding {
				t.Fatalf("switch binding = %q, want %q", got, wantBinding)
			}
			h.say("after-switch")
			h.waitSent(6)
			waitChatIdle(h, c)
			if got := h.calls("start:"); !slices.Equal(got, []string{"start:after-switch"}) {
				t.Fatalf("prompts after switch = %q", got)
			}
		})
	}
}

func TestChatApprovalOnlyStopLeavesQueueIdle(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	c := h.b.chat(h.plat, "c1")
	c.mu.Lock()
	dialogue := c.beginApprovalDialogueLocked(h.b.workCtx, "")
	c.mu.Unlock()
	h.say("/stop")
	select {
	case <-dialogue.ctx.Done():
	case <-time.After(20 * time.Second):
		t.Fatal("approval-only stop did not cancel approval dialogue")
	}
	requireChatActivity(t, c, chatIdle)
	h.say("/stop")
	h.waitSent(2)
	requireChatActivity(t, c, chatIdle)
	h.say("fresh")
	h.waitSent(3)
	waitChatIdle(h, c)
	if got := h.calls("start:"); !slices.Equal(got, []string{"start:fresh"}) {
		t.Fatalf("approval-only stop prevented fresh prompt: %q", got)
	}
}

func TestChatConcurrentIdleEnqueuesStartOneWorker(t *testing.T) {
	t.Parallel()
	h, gate := activityGateSetup(t, setup{mod: func(opts *Options) { opts.QueueLimit = 32 }}, func(text string) bool {
		return strings.HasPrefix(text, "echo: idle-queued-")
	})
	c := h.b.chat(gate, "c1")
	start := make(chan struct{})
	var enqueues sync.WaitGroup
	var accepted []string
	for index := range 12 {
		enqueues.Go(func() {
			<-start
			text := fmt.Sprintf("idle-queued-%02d", index)
			c.mu.Lock()
			defer c.mu.Unlock()
			if c.enqueueLocked(Message{Platform: "fake", ChatID: "c1", SenderID: "u1", MessageID: text, Text: text}) {
				accepted = append(accepted, "start:"+text)
			}
		})
	}
	close(start)
	enqueues.Wait()
	waitActivityGate(t, gate)
	if len(accepted) != 12 {
		t.Fatalf("accepted messages = %d, want 12", len(accepted))
	}
	requireChatActivity(t, c, chatWorking)
	if got := h.calls("start:"); !slices.Equal(got, accepted[:1]) {
		t.Fatalf("idle enqueues started competing workers: %q, want %q", got, accepted[:1])
	}
	gate.unblock()
	h.waitSent(12)
	waitChatIdle(h, c)
	if got := h.calls("start:"); !slices.Equal(got, accepted) {
		t.Fatalf("idle enqueue prompt order = %q, want %q", got, accepted)
	}
}
