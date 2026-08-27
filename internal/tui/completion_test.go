package tui

import (
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
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
