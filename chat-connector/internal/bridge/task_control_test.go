package bridge

import (
	"slices"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/chat-connector/internal/agent/fake"
)

func TestNaturalTaskControlStaysSeparateAndQueuesFollowups(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{platform: "telegram"})
	h.say("hold:2")
	h.waitCall("start:hold:2")
	h.say("focus on the tests")
	h.say("follow-up: summarize the result")

	h.waitFor(func() bool {
		return slices.Contains(fake.Calls(h.dir), "task_control:focus on the tests") &&
			slices.Contains(fake.Calls(h.dir), "task_control:follow-up: summarize the result")
	}, "serialized task-control requests")
	h.waitFor(func() bool { return slices.Contains(h.plat.texts(), "Guidance accepted.") }, "separate control reply")
	h.waitFor(func() bool { return slices.Contains(h.plat.texts(), "I will handle that after the current task.") }, "follow-up routing reply")
	h.waitCall("start:follow-up: summarize the result")

	calls := fake.Calls(h.dir)
	if slices.Contains(calls, "start:focus on the tests") {
		t.Fatalf("guidance became an ordinary prompt: %q", calls)
	}
	end := slices.Index(calls, "end:hold:2")
	followup := slices.Index(calls, "start:follow-up: summarize the result")
	if end < 0 || followup < 0 || followup < end {
		t.Fatalf("follow-up ran before active task ended: %q", calls)
	}
	if !slices.Contains(calls, "task_control_channel:telegram") {
		t.Fatalf("channel was not passed to task control: %q", calls)
	}
	got := h.waitSent(4)
	if !slices.Contains(got, "echo: follow-up: summarize the result") {
		t.Fatalf("follow-up answer missing from separate replies: %q", got)
	}
}

func TestTaskControlNoDecisionQueuesOriginalMessage(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("hold:1")
	h.waitCall("start:hold:1")
	h.say("no-decision")
	h.waitCall("task_control:no-decision")
	h.waitCall("start:no-decision")
	calls := fake.Calls(h.dir)
	end := slices.Index(calls, "end:hold:1")
	followup := slices.Index(calls, "start:no-decision")
	if end < 0 || followup < 0 || followup < end {
		t.Fatalf("message after a no-decision result did not queue after the task: %q", calls)
	}
	got := h.waitSent(3)
	if !slices.Contains(got, "echo: no-decision") {
		t.Fatalf("original message was not run after the no-decision result: %q", got)
	}
}

func TestNaturalTaskControlQueueUsesTheConfiguredLimit(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("block")
	h.waitCall("start:block")
	h.say("hold-control")
	h.waitCall("task_control:hold-control")
	for i := 1; i <= 10; i++ {
		h.say("control " + string(rune('0'+i)))
	}
	h.say("overflow control")
	h.waitFor(func() bool {
		return slices.ContainsFunc(h.plat.texts(), func(text string) bool { return strings.Contains(text, "Queue is full") })
	}, "active-task control queue limit")
	h.say("/stop")
	h.waitFor(func() bool { return slices.Contains(h.plat.texts(), "Stopped.") }, "stopped active task")
}

func TestTaskControlErrorReportsUnknownRouteWithoutPromptFallback(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("hold:1")
	h.waitCall("start:hold:1")
	h.say("control-error")
	h.waitFor(func() bool {
		for _, sent := range h.plat.texts() {
			if strings.Contains(sent, "Could not confirm how this message was routed") {
				return true
			}
		}
		return false
	}, "uncertain control response")
	h.waitCall("end:hold:1")
	if slices.Contains(fake.Calls(h.dir), "start:control-error") {
		t.Fatalf("failed control message was silently replayed as a prompt: %q", fake.Calls(h.dir))
	}
}

func TestStopInvalidatesALateQueuedControlResult(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("block")
	h.waitCall("start:block")
	h.say("slow-follow-up: do not run")
	h.waitCall("task_control:slow-follow-up: do not run")
	h.say("/stop")
	h.waitSent(1)
	settle()
	if slices.Contains(fake.Calls(h.dir), "start:slow-follow-up: do not run") {
		t.Fatalf("late queued control survived /stop: %q", fake.Calls(h.dir))
	}
}

func TestSteerAndQueueCommandsUseTheirExplicitRoutes(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("/steer idle request")
	h.waitSent(1)
	if !strings.Contains(h.plat.texts()[0], "only works during an active task") {
		t.Fatalf("idle steer reply = %q", h.plat.texts()[0])
	}

	h.say("hold:1")
	h.waitCall("start:hold:1")
	h.say("/steer focus on the failing test")
	h.waitCall("steer:focus on the failing test")
	h.say("/queue prepare a short summary")
	h.waitFor(func() bool {
		return slices.Contains(h.plat.texts(), "Guidance accepted; it will be used at the next safe task boundary.") &&
			slices.Contains(h.plat.texts(), "Accepted as a separate prompt; its answer will be sent separately.")
	}, "steer and queue acknowledgements")
	h.waitCall("start:prepare a short summary")

	calls := fake.Calls(h.dir)
	if slices.Contains(calls, "start:/steer focus on the failing test") ||
		slices.Contains(calls, "start:/queue prepare a short summary") {
		t.Fatalf("control commands became ordinary prompts: %q", calls)
	}
	end := slices.Index(calls, "end:hold:1")
	followup := slices.Index(calls, "start:prepare a short summary")
	if end < 0 || followup < 0 || followup < end {
		t.Fatalf("/queue did not wait for the active task: %q", calls)
	}
}

func TestSteeringCommandsPreserveUserOrder(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("block")
	h.waitCall("start:block")
	h.say("hold-control")
	h.waitCall("task_control:hold-control")
	h.say("/steer investigate first")
	h.say("/steer then fix the cause")
	h.waitCall("steer:then fix the cause")
	calls := fake.Calls(h.dir)
	first := slices.Index(calls, "steer:investigate first")
	second := slices.Index(calls, "steer:then fix the cause")
	if first < 0 || second <= first {
		t.Fatalf("steering reordered the user's constraints: %q", calls)
	}
	h.say("/stop")
}

func TestSteerAndQueueValidateTextLengthAndEmptiness(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	long := strings.Repeat("x", 32_001)
	for _, command := range []string{"/steer  ", "/queue \n", "/steer " + long, "/queue " + long} {
		h.say(command)
	}
	got := h.waitSent(4)
	for _, want := range []string{"Usage: /steer", "Usage: /queue", "Guidance is too long", "Prompt is too long"} {
		if !slices.ContainsFunc(got, func(text string) bool { return strings.Contains(text, want) }) {
			t.Errorf("reply %q missing %q", got, want)
		}
	}
	if calls := agentWork(h.dir); len(calls) != 0 {
		t.Fatalf("invalid command text reached the agent: %q", calls)
	}
}

func TestNaturalTaskControlRejectsOversizeInput(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("wait")
	h.waitCall("start:wait")
	tooLong := strings.Repeat("x", 32_001)
	h.say(tooLong)
	h.waitFor(func() bool {
		return slices.Contains(h.plat.texts(), "Message is too long; limit is 32000 bytes.")
	}, "oversized active-task message rejection")
	if slices.Contains(fake.Calls(h.dir), "task_control:"+tooLong) ||
		slices.Contains(fake.Calls(h.dir), "start:"+tooLong) {
		t.Fatal("oversized message reached task control or prompt")
	}
	if err := fake.Release(h.dir); err != nil {
		t.Fatal(err)
	}
	h.waitCall("end:wait")
}

func TestQueueCommandBypassesApprovalDialogueButWaitsForDecision(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{env: []string{"FAKE_APPROVAL_DIALOGUE=1"}})
	h.say("perm:ls")
	h.waitSent(1)
	h.say("/queue summarize afterward")
	h.waitFor(func() bool {
		return slices.Contains(h.plat.texts(), "Accepted as a separate prompt; its answer will be sent separately.")
	}, "queued-prompt acknowledgement")
	if got := h.calls("start:"); len(got) != 1 || got[0] != "start:perm:ls" {
		t.Fatalf("queued prompt ran before the permission decision: %q", got)
	}
	if got := h.calls("approval:"); len(got) != 0 {
		t.Fatalf("explicit /queue entered approval dialogue: %q", got)
	}
	h.say("/deny")
	h.waitCall("start:summarize afterward")
}

func TestSteerCannotBypassPendingApproval(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("perm:ls")
	h.waitSent(1)
	h.say("/steer run it anyway")
	h.waitFor(func() bool {
		return slices.ContainsFunc(h.plat.texts(), func(text string) bool {
			return strings.Contains(text, "without a pending approval")
		})
	}, "approval-time steer rejection")
	if got := h.calls("task_control:"); len(got) != 0 {
		t.Fatalf("/steer bypassed pending approval: %q", got)
	}
	h.say("/deny")
}

func TestLegacyAgentQueuesNaturalTextButNeverPromptsSteer(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{env: []string{"FAKE_NO_TASK_CONTROL=1"}})
	h.say("wait")
	h.waitCall("start:wait")
	h.say("/steer change the plan")
	h.waitFor(func() bool {
		return slices.ContainsFunc(h.plat.texts(), func(text string) bool {
			return strings.Contains(text, "does not support /steer")
		})
	}, "unsupported steer explanation")
	h.say("please check the test output")
	h.waitFor(func() bool {
		return slices.ContainsFunc(h.plat.texts(), func(text string) bool {
			return strings.Contains(text, "does not support active-task guidance")
		})
	}, "legacy natural-message queue fallback")
	if err := fake.Release(h.dir); err != nil {
		t.Fatal(err)
	}
	h.waitCall("start:please check the test output")
	calls := fake.Calls(h.dir)
	if slices.Contains(calls, "start:/steer change the plan") {
		t.Fatalf("unsupported /steer was sent as a prompt: %q", calls)
	}
	end := slices.Index(calls, "end:wait")
	followup := slices.Index(calls, "start:please check the test output")
	if end < 0 || followup < 0 || followup < end {
		t.Fatalf("legacy natural message was not queued after the task: %q", calls)
	}
}
