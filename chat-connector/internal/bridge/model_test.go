package bridge

import (
	"errors"
	"slices"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/chat-connector/internal/agent/fake"
)

// staticProfiles is a ProfileSource with a fixed answer.
type staticProfiles struct {
	snapshot ProfileSnapshot
	err      error
}

func (s staticProfiles) Profiles() (ProfileSnapshot, error) { return s.snapshot, s.err }

func pickerFixture() staticProfiles {
	return staticProfiles{snapshot: ProfileSnapshot{
		Default: "deepseek",
		Profiles: []ProfileOption{
			{Name: "deepseek", Provider: "openai-compatible", Model: "deepseek-v4-flash"},
			{Name: "terra", Provider: "chatgpt", Model: "gpt-5.6-terra"},
		},
	}}
}

const pickerText = "Profiles:\n" +
	"• deepseek — openai-compatible / deepseek-v4-flash (default)\n" +
	"• terra — chatgpt / gpt-5.6-terra\n" +
	"Send /model <profile> to switch this conversation."

func TestBareModelPostsThePickerWithoutReachingTheAgent(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{profiles: pickerFixture()})
	h.say("/model")
	h.waitFor(func() bool {
		_, ok := h.plat.picker()
		return ok
	}, "profile picker")
	call, _ := h.plat.picker()
	if call.chat != "c1" || call.replyTo != "m1" {
		t.Errorf("picker went to chat %q replying to %q", call.chat, call.replyTo)
	}
	if call.text != pickerText {
		t.Errorf("picker text = %q, want %q", call.text, pickerText)
	}
	want := []Choice{
		{Label: "deepseek", Command: "/model deepseek"},
		{Label: "terra", Command: "/model terra"},
	}
	if !slices.Equal(call.choices, want) {
		t.Errorf("choices = %+v, want %+v", call.choices, want)
	}
	settle()
	if calls := agentWork(h.dir); slices.Contains(calls, "start:/model") {
		t.Errorf("bare /model reached the agent: %q", calls)
	}
}

func TestModelWithAProfileStillReachesTheAgent(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{profiles: pickerFixture()})
	h.say("/model terra")
	h.waitFor(func() bool {
		return slices.Contains(fake.Calls(h.dir), "start:/model terra")
	}, "agent command prompt")
	settle()
	if _, ok := h.plat.picker(); ok {
		t.Error("/model <profile> posted the picker instead of switching")
	}
}

func TestTextOnlyPlatformGetsTheProfileListAsText(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{textOnly: true, profiles: pickerFixture()})
	h.say("/model")
	sent := h.waitSent(1)
	if sent[0] != pickerText {
		t.Errorf("sent = %q, want %q", sent[0], pickerText)
	}
	settle()
	if calls := agentWork(h.dir); slices.Contains(calls, "start:/model") {
		t.Errorf("bare /model reached the agent: %q", calls)
	}
}

func TestPickerFallsBackToTextWhenButtonsAreRefused(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{profiles: pickerFixture()})
	h.plat.mu.Lock()
	h.plat.choicesErr = errors.New("button too long")
	h.plat.mu.Unlock()
	h.say("/model")
	sent := h.waitSent(1)
	if sent[0] != pickerText {
		t.Errorf("sent = %q, want the plain list", sent[0])
	}
}

func TestProfileSourceFailureIsReportedAndNotForwarded(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{profiles: staticProfiles{err: errors.New("permission denied")}})
	h.say("/model")
	sent := h.waitSent(1)
	if !strings.Contains(sent[0], "Could not read the local Otto configuration: permission denied") {
		t.Errorf("sent = %q", sent[0])
	}
	settle()
	if calls := agentWork(h.dir); slices.Contains(calls, "start:/model") {
		t.Errorf("bare /model reached the agent: %q", calls)
	}
}

func TestEmptyProfileListIsReported(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{profiles: staticProfiles{}})
	h.say("/model")
	sent := h.waitSent(1)
	if !strings.Contains(sent[0], "lists no profiles") {
		t.Errorf("sent = %q", sent[0])
	}
	if _, ok := h.plat.picker(); ok {
		t.Error("the picker was posted with no profiles")
	}
}

func TestBareModelWithoutASourceReachesTheAgent(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("/model")
	h.waitFor(func() bool {
		return slices.Contains(fake.Calls(h.dir), "start:/model")
	}, "agent command prompt")
}

func TestValidProfileArgMatchesOttoProfileNames(t *testing.T) {
	t.Parallel()
	for _, name := range []string{"deepseek", "chatgpt-5.5", "gpt_5", "a", "x.y+z@w"} {
		if !ValidProfileArg(name) {
			t.Errorf("%q must be a valid profile argument", name)
		}
	}
	for _, name := range []string{"", "with space", "a/b", "a:b", `a"b`, `a\b`, "é", strings.Repeat("x", 65)} {
		if ValidProfileArg(name) {
			t.Errorf("%q must not be a valid profile argument", name)
		}
	}
}
