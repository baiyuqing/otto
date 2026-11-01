package bridge

import (
	"errors"
	"slices"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/chat-connector/internal/agent/fake"
)

// staticModels is a ProfileSource with a fixed answer.
type staticModels struct {
	snapshot ModelSnapshot
	err      error
}

func (s staticModels) Models() (ModelSnapshot, error) { return s.snapshot, s.err }

func pickerFixture() staticModels {
	return staticModels{snapshot: ModelSnapshot{
		Default: "deepseek/deepseek-v4-flash",
		Models:  []string{"deepseek/deepseek-v4-flash", "chatgpt/gpt-5.6-terra"},
	}}
}

const pickerText = "Configured models:\n" +
	"• deepseek/deepseek-v4-flash (default)\n" +
	"• chatgpt/gpt-5.6-terra\n" +
	"Switch with /model <provider>/<model_id>."

func TestBareModelPostsThePickerWithoutReachingTheAgent(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{models: pickerFixture()})
	h.say("/model")
	h.waitFor(func() bool {
		_, ok := h.plat.picker()
		return ok
	}, "model picker")
	call, _ := h.plat.picker()
	if call.chat != "c1" || call.replyTo != "m1" {
		t.Errorf("picker went to chat %q replying to %q", call.chat, call.replyTo)
	}
	if call.text != pickerText {
		t.Errorf("picker text = %q, want %q", call.text, pickerText)
	}
	want := []Choice{
		{Label: "deepseek/deepseek-v4-flash", Command: "/model deepseek/deepseek-v4-flash"},
		{Label: "chatgpt/gpt-5.6-terra", Command: "/model chatgpt/gpt-5.6-terra"},
	}
	if !slices.Equal(call.choices, want) {
		t.Errorf("choices = %+v, want %+v", call.choices, want)
	}
	settle()
	if calls := agentWork(h.dir); slices.Contains(calls, "start:/model") {
		t.Errorf("bare /model reached the agent: %q", calls)
	}
}

func TestModelWithASelectorStillReachesTheAgent(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{models: pickerFixture()})
	h.say("/model chatgpt/gpt-5.6-terra")
	h.waitFor(func() bool {
		return slices.Contains(fake.Calls(h.dir), "start:/model chatgpt/gpt-5.6-terra")
	}, "agent command prompt")
	settle()
	if _, ok := h.plat.picker(); ok {
		t.Error("/model <selector> posted the picker instead of switching")
	}
}

func TestTextOnlyPlatformGetsTheModelListAsText(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{textOnly: true, models: pickerFixture()})
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
	h := newHarness(t, setup{models: pickerFixture()})
	h.plat.mu.Lock()
	h.plat.choicesErr = errors.New("button too long")
	h.plat.mu.Unlock()
	h.say("/model")
	sent := h.waitSent(1)
	if sent[0] != pickerText {
		t.Errorf("sent = %q, want the plain list", sent[0])
	}
}

func TestModelSourceFailureIsReportedAndNotForwarded(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{models: staticModels{err: errors.New("permission denied")}})
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

func TestEmptyModelListIsReported(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{models: staticModels{}})
	h.say("/model")
	sent := h.waitSent(1)
	if !strings.Contains(sent[0], "lists no models") {
		t.Errorf("sent = %q", sent[0])
	}
	if _, ok := h.plat.picker(); ok {
		t.Error("the picker was posted with no models")
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

func TestValidModelSelectorAcceptsOnlyOneProviderSlashModelWord(t *testing.T) {
	t.Parallel()
	for _, selector := range []string{"chatgpt/gpt-5.6-luna", "openrouter/anthropic/claude-x", "deepseek/deepseek-v4-flash", "p_1/x.y+z@w"} {
		if !ValidModelSelector(selector) {
			t.Errorf("%q must be a valid model selector", selector)
		}
	}
	for _, selector := range []string{"", "terra", "/gpt", "chatgpt/", "a b/c", "a/b c", "a.b/c", "a/b:c", `a/b"c`, `a/b\c`, "é/x", "p/" + strings.Repeat("x", 128)} {
		if ValidModelSelector(selector) {
			t.Errorf("%q must not be a valid model selector", selector)
		}
	}
}
