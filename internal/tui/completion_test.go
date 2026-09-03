package tui

import (
	"context"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
)

func typeEditorText(t *testing.T, model Model, text string) Model {
	t.Helper()
	for _, r := range text {
		updated, _ := model.Update(keyPress(r))
		model = updated.(Model)
	}
	return model
}

func TestSlashCommandSuggestionsFilterByPrefix(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 16)
	// Seed the transcript so the empty-state hint (which legitimately mentions
	// /help and /resume) does not interfere with the suggestion scan below.
	m.entries = []Entry{{ID: "assistant", Kind: EntryAssistant, Raw: "context", Rendered: "context", RenderWidth: 80}}
	m.rerenderAndRefreshViewportContent()
	m = typeEditorText(t, m, "/s")

	content := m.View().Content
	if !strings.Contains(content, "/session") || !strings.Contains(content, "show session details") {
		t.Fatalf("view = %q, want matching session suggestion", content)
	}
	for _, command := range []string{"/help", "/new", "/resume", "/compact", "/exit"} {
		if strings.Contains(content, command) {
			t.Fatalf("view = %q, contains nonmatching suggestion %q", content, command)
		}
	}
}

func TestSlashCommandSuggestionSelectionAndTabCompletion(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 16)
	m = typeEditorText(t, m, "/")

	updated, _ := m.Update(keyPress(tea.KeyDown))
	updated, _ = updated.(Model).Update(keyPress(tea.KeyDown))
	updated, _ = updated.(Model).Update(keyPress(tea.KeyTab))
	completed := updated.(Model)
	if got := completed.editor.Value(); got != "/new" {
		t.Fatalf("completed editor = %q, want /new", got)
	}

	m = resizeModel(t, newTestModel(t), 80, 16)
	m = typeEditorText(t, m, "/")
	updated, _ = m.Update(keyPress(tea.KeyUp))
	updated, _ = updated.(Model).Update(keyPress(tea.KeyTab))
	if got := updated.(Model).editor.Value(); got != "/exit" {
		t.Fatalf("wrapped completion = %q, want /exit", got)
	}
}

func TestSlashCommandEnterRequiresExactCommand(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 16)
	m = typeEditorText(t, m, "/se")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if cmd != nil || got.statusText != "unknown command: /se" || got.editor.Value() != "/se" {
		t.Fatalf("partial submit: cmd=%v status=%q editor=%q", cmd, got.statusText, got.editor.Value())
	}
}

func TestSlashCommandKeysPassThroughOutsideSuggestionMode(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 16)
	m = typeEditorText(t, m, "hello")

	updated, _ := m.Update(keyPress(tea.KeyTab))
	got := updated.(Model)
	if strings.HasPrefix(got.editor.Value(), "/") {
		t.Fatalf("ordinary tab activated slash completion: editor=%q", got.editor.Value())
	}
	for _, description := range []string{"show help", "show session details", "start a new session", "resume a session", "compact context", "quit"} {
		if strings.Contains(got.View().Content, description) {
			t.Fatalf("ordinary view contains suggestion description %q", description)
		}
	}
}

func TestSlashCommandSuggestionPanelUsesRegistryAndStaysWithinBounds(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 40, 8)
	m = typeEditorText(t, m, "/")
	content := m.View().Content
	assertRenderedBounds(t, content, 40, 8)
	// The minimum terminal fits only a few suggestion rows above the taller input box.
	for _, text := range []string{"/help", "show help", "/session", "show session details", "/new", "start a new session"} {
		if !strings.Contains(content, text) {
			t.Fatalf("suggestion panel = %q, want visible %q", content, text)
		}
	}

	m = resizeModel(t, m, 60, 18)
	content = m.View().Content
	assertRenderedBounds(t, content, 60, 18)
	for _, text := range []string{"/resume", "resume a session", "/compact", "compact context", "/exit", "quit"} {
		if !strings.Contains(content, text) {
			t.Fatalf("suggestion panel = %q, want %q", content, text)
		}
	}

	m.overlay = overlayHelp
	help := m.View().Content
	assertRenderedBounds(t, help, 60, 18)
	for _, command := range []string{"/help", "/session", "/new", "/resume", "/compact", "/exit"} {
		if !strings.Contains(help, command) {
			t.Fatalf("help = %q, want registry command %q", help, command)
		}
	}
}

func TestSlashCommandModifiedKeysPassThrough(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 16)
	m = typeEditorText(t, m, "/")

	shiftDown := keyPress(tea.KeyDown, tea.ModShift)
	if _, _, handled := m.handleCommandSuggestionKey(shiftDown); handled {
		t.Fatal("shift+down was handled as suggestion navigation")
	}
	updated, _ := m.Update(shiftDown)
	got := updated.(Model)
	if got.commandSuggestionIndex != 0 {
		t.Fatalf("shift+down selected suggestion %d, want unchanged", got.commandSuggestionIndex)
	}

	shiftTab := keyPress(tea.KeyTab, tea.ModShift)
	if _, _, handled := got.handleCommandSuggestionKey(shiftTab); handled {
		t.Fatal("shift+tab was handled as command completion")
	}
	updated, _ = got.Update(shiftTab)
	got = updated.(Model)
	if got.editor.Value() == "/help" {
		t.Fatalf("shift+tab completed command: editor=%q", got.editor.Value())
	}
}

func TestSlashCommandCompletionClampsStaleSelection(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 16)
	m = typeEditorText(t, m, "/")
	m.commandSuggestionIndex = 99

	updated, _ := m.Update(keyPress(tea.KeyTab))
	if got := updated.(Model).editor.Value(); got != "/exit" {
		t.Fatalf("clamped completion = %q, want /exit", got)
	}
}

func TestSlashCommandPasteBackspaceAndSelectionTransitionsUseUpdate(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	// Seed a live (uncommitted) assistant entry tall enough to fill the
	// available transcript space; with no live content the transcript
	// height floors at 1 regardless of the suggestion panel, so the height
	// transitions this test exercises need real live content to observe.
	m.running = true
	m.activeAssistant = 0
	m.entries = []Entry{{ID: "assistant", Kind: EntryAssistant, Raw: strings.Repeat("line\n", 40)}}
	m.rerenderAndRefreshViewportContent()
	if got := m.viewport.Height(); got != 6 {
		t.Fatalf("initial viewport height = %d, want 6", got)
	}

	updated, _ := m.Update(tea.PasteMsg{Content: "/s"})
	m = updated.(Model)
	if m.editor.Value() != "/s" || len(m.commandSuggestions()) != 1 || m.viewport.Height() != 5 {
		t.Fatalf("paste state: editor=%q suggestions=%d viewport=%d", m.editor.Value(), len(m.commandSuggestions()), m.viewport.Height())
	}

	updated, _ = m.Update(tea.KeyPressMsg(tea.Key{Code: tea.KeyBackspace}))
	m = updated.(Model)
	if m.editor.Value() != "/" || len(m.commandSuggestions()) != 12 || m.viewport.Height() != 1 {
		t.Fatalf("first backspace: editor=%q suggestions=%d viewport=%d", m.editor.Value(), len(m.commandSuggestions()), m.viewport.Height())
	}
	updated, _ = m.Update(keyPress(tea.KeyDown))
	updated, _ = updated.(Model).Update(keyPress(tea.KeyDown))
	m = updated.(Model)
	if m.commandSuggestionIndex != 2 {
		t.Fatalf("selected index = %d, want 2", m.commandSuggestionIndex)
	}
	updated, _ = m.Update(keyPress('n'))
	m = updated.(Model)
	if m.editor.Value() != "/n" || m.commandSuggestionIndex != 0 || len(m.commandSuggestions()) != 1 {
		t.Fatalf("prefix reset: editor=%q selected=%d suggestions=%d", m.editor.Value(), m.commandSuggestionIndex, len(m.commandSuggestions()))
	}
	updated, _ = m.Update(tea.KeyPressMsg(tea.Key{Code: tea.KeyBackspace}))
	updated, _ = updated.(Model).Update(tea.KeyPressMsg(tea.Key{Code: tea.KeyBackspace}))
	m = updated.(Model)
	if m.editor.Value() != "" || len(m.commandSuggestions()) != 0 || m.viewport.Height() != 6 {
		t.Fatalf("closed suggestions: editor=%q suggestions=%d viewport=%d", m.editor.Value(), len(m.commandSuggestions()), m.viewport.Height())
	}
}

func TestSlashCommandMultilineArrowsAndOverlayTransitionsUseUpdate(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 16)
	m.editor.SetValue("/s\nmore")
	m.editor.CursorEnd()
	if m.editor.Line() != 1 {
		t.Fatalf("initial editor line = %d, want 1", m.editor.Line())
	}
	updated, _ := m.Update(keyPress(tea.KeyUp))
	m = updated.(Model)
	if m.editor.Line() != 0 || len(m.commandSuggestions()) != 0 {
		t.Fatalf("multiline up: line=%d suggestions=%d", m.editor.Line(), len(m.commandSuggestions()))
	}
	updated, _ = m.Update(keyPress(tea.KeyDown))
	m = updated.(Model)
	if m.editor.Line() != 1 || len(m.commandSuggestions()) != 0 {
		t.Fatalf("multiline down: line=%d suggestions=%d", m.editor.Line(), len(m.commandSuggestions()))
	}

	m.editor.SetValue("/s")
	updated, _ = m.Update(showHelpOverlayMsg{})
	m = updated.(Model)
	if m.overlay != overlayHelp || len(m.commandSuggestions()) != 0 || strings.Contains(m.View().Content, "> /session") {
		t.Fatalf("help transition: overlay=%v suggestions=%d", m.overlay, len(m.commandSuggestions()))
	}
	updated, _ = m.Update(hideOverlayMsg{})
	m = updated.(Model)
	if m.overlay != overlayNone || len(m.commandSuggestions()) != 1 || !strings.Contains(m.View().Content, "> /session") {
		t.Fatalf("hide transition: overlay=%v suggestions=%d view=%q", m.overlay, len(m.commandSuggestions()), m.View().Content)
	}
}

// TestSlashCommandResizeStaysConsistent replaces the old scroll/autoFollow
// test: PageUp/PageDown/Home/End no longer route to the viewport (native
// terminal scrollback replaces them), so this only checks that resizing a
// live (uncommitted) multi-line entry keeps the live region within bounds
// and that suggestions still work afterward.
func TestSlashCommandResizeStaysConsistent(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.running = true
	m.activeAssistant = 0
	m.entries = []Entry{{ID: "assistant", Kind: EntryAssistant, Raw: strings.Repeat("line\n", 40)}}
	m.rerenderAndRefreshViewportContent()
	if m.viewport.Height() < 1 {
		t.Fatalf("viewport height = %d, want at least 1", m.viewport.Height())
	}
	assertRenderedBounds(t, m.View().Content, 80, 12)

	m = resizeModel(t, m, 40, 8)
	assertRenderedBounds(t, m.View().Content, 40, 8)
	m = resizeModel(t, m, 100, 20)
	assertRenderedBounds(t, m.View().Content, 100, 20)

	m.editor.SetValue("")
	m.commandSuggestionIndex = 0
	m.rerenderAndRefreshViewportContent()
	m = typeEditorText(t, m, "/")
	if len(m.commandSuggestions()) == 0 {
		t.Fatalf("expected slash suggestions after typing /")
	}
}

func TestCompactSuggestionCompletesAndClosesWhenFocusStarts(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 16)
	m = typeEditorText(t, m, "/c")
	if suggestions := m.commandSuggestions(); len(suggestions) != 1 || suggestions[0].Name != "/compact" {
		t.Fatalf("/c suggestions = %#v", suggestions)
	}
	updated, _ := m.Update(keyPress(tea.KeyTab))
	m = updated.(Model)
	if got := m.editor.Value(); got != "/compact" {
		t.Fatalf("completion = %q, want /compact", got)
	}
	m = typeEditorText(t, m, " focus")
	if suggestions := m.commandSuggestions(); len(suggestions) != 0 {
		t.Fatalf("focus kept suggestions open: %#v", suggestions)
	}
}

func TestSlashCommandSuggestionsDoNotOverrideRunningCancellation(t *testing.T) {
	started := make(chan struct{})
	backend := &fakeBackend{prompt: func(ctx context.Context, _ string, _ func(agent.Event)) error {
		close(started)
		<-ctx.Done()
		return ctx.Err()
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")
	// dispatch, not Update: submitting "question" commits a final User
	// entry within this same call, which Update's auto-flush wrapper would
	// batch with the real turn-start command. tea.Batch doesn't invoke its
	// sub-commands when called, so start() below would never start the
	// backend goroutine and <-started would hang forever.
	updated, start := m.dispatch(keyPress(tea.KeyEnter))
	m = updated.(Model)
	done := make(chan tea.Msg, 1)
	go func() { done <- start() }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("turn did not start")
	}

	m = typeEditorText(t, m, "/")
	// dispatch, not Update: the "question" submission above left
	// pendingPrints undrained (it also used dispatch, for the same reason
	// noted there). An Update call here would auto-flush that leftover
	// chunk and return a non-nil cmd unrelated to escape handling.
	updated, cmd := m.dispatch(keyPress(tea.KeyEscape))
	m = updated.(Model)
	if cmd != nil || !m.running {
		t.Fatalf("escape state: cmd=%v running=%v", cmd, m.running)
	}
	select {
	case msg := <-done:
		updated, _ = m.Update(msg)
		if updated.(Model).running {
			t.Fatal("turn remained active after suggestion-mode escape cancellation")
		}
	case <-time.After(time.Second):
		t.Fatal("suggestion-mode escape did not cancel running turn")
	}
}

func TestSlashCommandSuggestionsHideForMultilineNoMatchAndOverlay(t *testing.T) {
	for name, value := range map[string]string{
		"multiline": "/s\nmore",
		"no match":  "/wat",
	} {
		t.Run(name, func(t *testing.T) {
			m := resizeModel(t, newTestModel(t), 80, 16)
			m.editor.SetValue(value)
			if content := m.View().Content; strings.Contains(content, "show session details") {
				t.Fatalf("view = %q, unexpectedly shows suggestions", content)
			}
		})
	}

	m := resizeModel(t, newTestModel(t), 80, 16)
	m = typeEditorText(t, m, "/s")
	m.overlay = overlayHelp
	if content := m.View().Content; strings.Contains(content, "> /session") {
		t.Fatalf("overlay view = %q, suggestion panel remained visible", content)
	}
}
