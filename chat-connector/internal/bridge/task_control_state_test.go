package bridge

import (
	"context"
	"slices"
	"strings"
	"testing"
	"time"
)

// Caller holds chat.mu, as with the production worker admission.
func controlWorkerForTask(task *controlTask) *controlWorker {
	worker := &controlWorker{task: task, done: make(chan struct{})}
	task.worker = worker
	return worker
}

func TestTaskControlStopInvalidatesTaskWithoutCompletingWorker(t *testing.T) {
	t.Parallel()
	c := &chat{}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.taskControlPhaseLocked() != taskControlIdle || c.taskControlAvailableLocked() {
		t.Fatal("an idle chat admitted task control")
	}
	task := c.beginTaskControlLocked(context.Background(), "session")
	t.Cleanup(task.cancel)
	worker := controlWorkerForTask(task)
	task.controls = []controlItem{{text: "queued"}}
	if c.taskControlPhaseLocked() != taskControlActive || !c.controlWorkerCurrentLocked(worker) {
		t.Fatal("a new task did not bind its active worker")
	}
	c.stopTaskControlLocked()
	c.stopTaskControlLocked()
	if task.ctx.Err() == nil || len(task.controls) != 0 || c.taskControlAvailableLocked() {
		t.Fatal("stop did not cancel and invalidate queued controls")
	}
	if c.taskControlPhaseLocked() != taskControlIdle || c.controlWorkerCurrentLocked(worker) {
		t.Fatal("a canceled task retained routing authority")
	}
	if task.worker != worker {
		t.Fatal("stop discarded in-flight worker ownership")
	}
	select {
	case <-worker.done:
		t.Fatal("cancellation prematurely completed the worker")
	default:
	}
	c.finishTaskControlWorkerLocked(worker)
	select {
	case <-worker.done:
	default:
		t.Fatal("worker completion did not release its waiter")
	}
}

func TestStaleControlWorkerCannotClearNewTaskQueueOrWorker(t *testing.T) {
	t.Parallel()
	c := &chat{}
	c.mu.Lock()
	old := c.beginTaskControlLocked(context.Background(), "old")
	t.Cleanup(old.cancel)
	oldWorker := controlWorkerForTask(old)
	c.stopTaskControlLocked()
	current := c.beginTaskControlLocked(context.Background(), "new")
	t.Cleanup(current.cancel)
	current.controls = []controlItem{{text: "keep the new input"}}
	currentWorker := controlWorkerForTask(current)
	c.mu.Unlock()

	// This worker must exit before attempting any RPC; c has no agent.
	c.taskControlDialogue(oldWorker)
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.controlTask != current || !c.controlWorkerCurrentLocked(currentWorker) {
		t.Fatal("a stale worker released the current task or worker")
	}
	if len(current.controls) != 1 || current.controls[0].text != "keep the new input" {
		t.Fatal("a stale worker discarded the new task's queue")
	}
	select {
	case <-oldWorker.done:
	default:
		t.Fatal("stale worker did not settle its own completion")
	}
	c.finishTaskControlWorkerLocked(oldWorker)
	c.releaseTaskControlLocked(old)
	if c.controlTask != current || current.worker != currentWorker {
		t.Fatal("repeated stale completion affected the new owner")
	}
	select {
	case <-currentWorker.done:
		t.Fatal("stale completion closed the new worker's channel")
	default:
	}
	c.finishTaskControlWorkerLocked(currentWorker)
	c.releaseTaskControlLocked(current)
}

func TestStaleTaskCompletionWaitsForItsOwnWorker(t *testing.T) {
	t.Parallel()
	c := &chat{}
	c.mu.Lock()
	old := c.beginTaskControlLocked(context.Background(), "old")
	t.Cleanup(old.cancel)
	oldWorker := controlWorkerForTask(old)
	c.stopTaskControlLocked()
	current := c.beginTaskControlLocked(context.Background(), "new")
	t.Cleanup(current.cancel)
	currentWorker := controlWorkerForTask(current)
	c.mu.Unlock()
	t.Cleanup(func() {
		c.mu.Lock()
		defer c.mu.Unlock()
		c.finishTaskControlWorkerLocked(oldWorker)
		c.finishTaskControlWorkerLocked(currentWorker)
		c.stopTaskControlLocked()
	})
	finished := make(chan struct{})
	go func() {
		c.finishTaskControl(old)
		close(finished)
	}()
	c.mu.Lock()
	c.finishTaskControlWorkerLocked(oldWorker)
	c.mu.Unlock()
	select {
	case <-finished:
	case <-time.After(5 * time.Second):
		t.Fatal("stale task completion waited on a newer worker")
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.controlTask != current || c.taskControlPhaseLocked() != taskControlActive || current.worker != currentWorker {
		t.Fatal("stale task completion released newer ownership")
	}
	select {
	case <-currentWorker.done:
		t.Fatal("new worker was completed by an old task")
	default:
	}
}

func TestControlWorkerCompletionMatchesHandleAndIsIdempotent(t *testing.T) {
	t.Parallel()
	c := &chat{}
	c.mu.Lock()
	defer c.mu.Unlock()
	task := c.beginTaskControlLocked(context.Background(), "session")
	t.Cleanup(task.cancel)
	first := controlWorkerForTask(task)
	c.finishTaskControlWorkerLocked(first)
	second := controlWorkerForTask(task)
	c.finishTaskControlWorkerLocked(first)
	if task.worker != second || !c.controlWorkerCurrentLocked(second) {
		t.Fatal("old worker completion released its successor")
	}
	select {
	case <-second.done:
		t.Fatal("old worker completion closed its successor")
	default:
	}
	c.finishTaskControlWorkerLocked(second)
	c.finishTaskControlWorkerLocked(second)
	c.releaseTaskControlLocked(task)
	if c.taskControlPhaseLocked() != taskControlIdle {
		t.Fatal("matching task completion did not release admission")
	}
}

func TestDrainingTaskKeepsNaturalControlButRejectsSteer(t *testing.T) {
	t.Parallel()
	h, gate := activityGateSetup(t, setup{}, func(text string) bool { return text == "Guidance accepted." })
	h.say("hold:1")
	h.waitCall("start:hold:1")
	h.say("guidance")
	waitActivityGate(t, gate)
	h.waitCall("end:hold:1")
	c := h.b.chat(gate, "c1")
	h.waitFor(func() bool {
		c.mu.Lock()
		defer c.mu.Unlock()
		return c.taskControlPhaseLocked() == taskControlDraining
	}, "main task draining its control reply")
	if got := h.plat.texts(); len(got) != 0 {
		t.Fatalf("main task reply overtook control delivery: %q", got)
	}
	h.say("/steer too late")
	if got := h.waitSent(1)[0]; !strings.Contains(got, "only works during an active task") {
		t.Fatalf("draining steer response = %q", got)
	}
	h.say("after-drain")
	c.mu.Lock()
	queued := len(c.controlTask.controls)
	c.mu.Unlock()
	if queued != 1 {
		t.Fatalf("draining natural control queue = %d, want 1", queued)
	}
	gate.unblock()
	h.waitSent(5)
	waitChatIdle(h, c)
	want := []string{
		"Guidance accepted.",
		"The active task had already finished; your message was queued as a follow-up.",
		"echo: hold:1",
		"echo: after-drain",
	}
	if got := h.plat.texts()[1:]; !slices.Equal(got, want) {
		t.Fatalf("control/main/follow-up reply order = %q, want %q", got, want)
	}
	if got := h.calls("start:"); !slices.Equal(got, []string{"start:hold:1", "start:after-drain"}) {
		t.Fatalf("unexpected prompt routing: %q", got)
	}
	if got := h.calls("steer:"); len(got) != 0 {
		t.Fatalf("draining task admitted steer: %q", got)
	}
}

func TestStopRetainsControlReplyOwnershipUntilItSettles(t *testing.T) {
	t.Parallel()
	h, gate := activityGateSetup(t, setup{}, func(text string) bool { return text == "Guidance accepted." })
	h.say("hold:1")
	h.waitCall("start:hold:1")
	h.say("guidance")
	waitActivityGate(t, gate)
	h.waitCall("end:hold:1")
	c := h.b.chat(gate, "c1")
	c.mu.Lock()
	task := c.controlTask
	worker := task.worker
	c.mu.Unlock()
	h.say("/stop")
	h.say("/stop")
	h.say("fresh")
	h.say("/use aaaa")
	if got := h.waitSent(1)[0]; !strings.Contains(got, "running or queued") {
		t.Fatalf("control reply lost chat ownership: %q", got)
	}
	c.mu.Lock()
	if c.taskControlPhaseLocked() != taskControlIdle || task.ctx.Err() == nil {
		c.mu.Unlock()
		t.Fatal("stop did not revoke control routing and cancellation")
	}
	if task.worker != worker {
		c.mu.Unlock()
		t.Fatal("stop discarded the worker delivering a reply")
	}
	c.mu.Unlock()
	select {
	case <-worker.done:
		t.Fatal("stop completed a worker before its owned reply settled")
	default:
	}
	if got := h.calls("start:"); !slices.Equal(got, []string{"start:hold:1"}) {
		t.Fatalf("new task started before control reply settled: %q", got)
	}
	gate.unblock()
	h.waitSent(4)
	waitChatIdle(h, c)
	if got := h.calls("start:"); !slices.Equal(got, []string{"start:hold:1", "start:fresh"}) {
		t.Fatalf("post-stop prompt routing = %q", got)
	}
	select {
	case <-worker.done:
	default:
		t.Fatal("worker completion did not settle after delivery")
	}
}
