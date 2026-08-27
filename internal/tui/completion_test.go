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
	m = typeEditorText(t, m, "/s")

	content := m.View().Content
	if !strings.Contains(content, "/session") || !strings.Contains(content, "show session details") {
		t.Fatalf("view = %q, want matching session suggestion", content)
	}
	for _, command := range []string{"/help", "/new", "/exit"} {
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
	for _, description := range []string{"show help", "show session details", "start a new session", "quit"} {
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

	for _, text := range []string{
		"/help", "show help",
		"/session", "show session details",
		"/new", "start a new session",
		"/exit", "quit",
	} {
		if !strings.Contains(content, text) {
			t.Fatalf("suggestion panel = %q, want %q", content, text)
		}
	}

	m.overlay = overlayHelp
	help := m.View().Content
	assertRenderedBounds(t, help, 40, 8)
	for _, command := range []string{"/help", "/session", "/new", "/exit"} {
		if !strings.Contains(help, command) {
			t.Fatalf("help = %q, want registry command %q", help, command)
		}
	}
}

func TestSlashCommandModifiedKeysPassThrough(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 16)
	m = typeEditorText(t, m, "/")

	updated, _ := m.Update(keyPress(tea.KeyDown, tea.ModShift))
	got := updated.(Model)
	if got.commandSuggestionIndex != 0 {
		t.Fatalf("shift+down selected suggestion %d, want unchanged", got.commandSuggestionIndex)
	}

	updated, _ = got.Update(keyPress(tea.KeyTab, tea.ModShift))
	got = updated.(Model)
	if got.editor.Value() == "/help" {
		t.Fatalf("shift+tab completed command: editor=%q", got.editor.Value())
	}
}

func TestSlashCommandPasteBackspaceAndSelectionTransitionsUseUpdate(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	if got := m.viewport.Height(); got != 10 {
		t.Fatalf("initial viewport height = %d, want 10", got)
	}

	updated, _ := m.Update(tea.PasteMsg{Content: "/s"})
	m = updated.(Model)
	if m.editor.Value() != "/s" || len(m.commandSuggestions()) != 1 || m.viewport.Height() != 9 {
		t.Fatalf("paste state: editor=%q suggestions=%d viewport=%d", m.editor.Value(), len(m.commandSuggestions()), m.viewport.Height())
	}

	updated, _ = m.Update(tea.KeyPressMsg(tea.Key{Code: tea.KeyBackspace}))
	m = updated.(Model)
	if m.editor.Value() != "/" || len(m.commandSuggestions()) != 4 || m.viewport.Height() != 6 {
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
	if m.editor.Value() != "" || len(m.commandSuggestions()) != 0 || m.viewport.Height() != 10 {
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

func TestSlashCommandResizeAndScrollStateStayConsistent(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.entries = []Entry{{ID: "assistant", Kind: EntryAssistant, Raw: strings.Repeat("line\n", 40)}}
	m.rerenderAndRefreshViewportContent(false)
	m.autoFollow = false
	m.viewport.SetYOffset(3)
	m = typeEditorText(t, m, "/")
	if m.viewport.YOffset() != 3 || m.autoFollow || m.viewport.Height() != 6 {
		t.Fatalf("suggestion scroll state: offset=%d follow=%v height=%d", m.viewport.YOffset(), m.autoFollow, m.viewport.Height())
	}

	m = resizeModel(t, m, 40, 8)
	if m.viewport.Height() != 2 {
		t.Fatalf("minimum viewport height = %d, want 2", m.viewport.Height())
	}
	assertRenderedBounds(t, m.View().Content, 40, 8)
	m = resizeModel(t, m, 100, 20)
	if m.viewport.Height() != 14 {
		t.Fatalf("expanded viewport height = %d, want 14", m.viewport.Height())
	}
	assertRenderedBounds(t, m.View().Content, 100, 20)
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
	updated, start := m.Update(keyPress(tea.KeyEnter))
	m = updated.(Model)
	done := make(chan tea.Msg, 1)
	go func() { done <- start() }()
	<-started

	m = typeEditorText(t, m, "/")
	updated, cmd := m.Update(keyPress(tea.KeyEscape))
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
