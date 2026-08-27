package tui

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
)

type fakeBackend struct {
	prompt     func(context.Context, string, func(agent.Event)) error
	newSession func() error
	info       app.Info
	history    []model.Message
}

func (f *fakeBackend) Prompt(ctx context.Context, text string, emit func(agent.Event)) error {
	if f.prompt == nil {
		return nil
	}
	return f.prompt(ctx, text, emit)
}

func (f *fakeBackend) NewSession() error {
	if f.newSession == nil {
		return nil
	}
	return f.newSession()
}

func (f *fakeBackend) Info() app.Info { return f.info }

func (f *fakeBackend) History() []model.Message {
	return append([]model.Message(nil), f.history...)
}

type rendererFunc func(string, int) (string, error)

func (f rendererFunc) Render(text string, width int) (string, error) { return f(text, width) }

func keyPress(code rune, modifiers ...tea.KeyMod) tea.KeyPressMsg {
	var mod tea.KeyMod
	for _, modifier := range modifiers {
		mod |= modifier
	}
	return tea.KeyPressMsg(tea.Key{Code: code, Mod: mod})
}

var _ = keyPress

func newTestModelWithBackend(t *testing.T, backend *fakeBackend) Model {
	t.Helper()
	return NewModel(context.Background(), backend, WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	})))
}

func newTestModel(t *testing.T) Model {
	t.Helper()
	return newTestModelWithBackend(t, &fakeBackend{info: app.Info{Profile: "profile", Model: "model", SessionID: "session"}})
}

func resizeModel(t *testing.T, model Model, width, height int) Model {
	t.Helper()
	updated, _ := model.Update(tea.WindowSizeMsg{Width: width, Height: height})
	got, ok := updated.(Model)
	if !ok {
		t.Fatalf("updated model type = %T, want tui.Model", updated)
	}
	return got
}

func TestWindowResizeProducesResponsiveLayout(t *testing.T) {
	model := newTestModel(t)
	got := resizeModel(t, model, 100, 30)
	if got.viewport.Width() != 100 || got.viewport.Height() <= 0 {
		t.Fatalf("viewport = %dx%d", got.viewport.Width(), got.viewport.Height())
	}
	view := got.View()
	if !view.AltScreen || !strings.Contains(view.Content, "profile/model") {
		t.Fatalf("view = %#v", view)
	}
}

func TestSmallTerminalShowsResizeMessage(t *testing.T) {
	model := newTestModel(t)
	got := resizeModel(t, model, 30, 6)
	if content := got.View().Content; !strings.Contains(content, "terminal is too small") {
		t.Fatalf("content = %q", content)
	}

	restored := resizeModel(t, got, 100, 30)
	if content := restored.View().Content; strings.Contains(content, "terminal is too small") || !strings.Contains(content, "profile/model") {
		t.Fatalf("restored content = %q", content)
	}
}

func TestInitialHistoryStartsAtBottom(t *testing.T) {
	history := make([]model.Message, 0, 12)
	for i := range 12 {
		history = append(history, model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: fmt.Sprintf("line %02d", i)}}})
	}
	model := newTestModelWithBackend(t, &fakeBackend{
		info:    app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		history: history,
	})

	got := resizeModel(t, model, 60, 10)
	content := got.View().Content
	if !strings.Contains(content, "line 11") {
		t.Fatalf("content = %q, want newest history visible", content)
	}
	if strings.Contains(content, "line 00") {
		t.Fatalf("content = %q, want viewport positioned at newest history", content)
	}
}

func TestWidthChangeRerendersCachedEntries(t *testing.T) {
	backend := &fakeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		history: []model.Message{{
			Role:   model.RoleAssistant,
			Blocks: []model.Block{{Type: model.BlockText, Text: "render me"}},
		}},
	}
	var widths []int
	model := NewModel(context.Background(), backend, WithRenderer(rendererFunc(func(text string, width int) (string, error) {
		widths = append(widths, width)
		return fmt.Sprintf("%s [w=%d]", text, width), nil
	})))

	wide := resizeModel(t, model, 100, 20)
	if len(wide.entries) != 1 || wide.entries[0].RenderWidth <= 0 {
		t.Fatalf("entries = %#v", wide.entries)
	}
	wideWidth := wide.entries[0].RenderWidth
	if !strings.Contains(wide.View().Content, fmt.Sprintf("[w=%d]", wideWidth)) {
		t.Fatalf("wide content = %q", wide.View().Content)
	}

	narrow := resizeModel(t, wide, 60, 20)
	if narrow.entries[0].RenderWidth <= 0 || narrow.entries[0].RenderWidth == wideWidth {
		t.Fatalf("render widths = %d then %d", wideWidth, narrow.entries[0].RenderWidth)
	}
	if !strings.Contains(narrow.View().Content, fmt.Sprintf("[w=%d]", narrow.entries[0].RenderWidth)) {
		t.Fatalf("narrow content = %q", narrow.View().Content)
	}
	if len(widths) < 2 {
		t.Fatalf("renderer call widths = %v, want rerender on width change", widths)
	}
}

func TestNarrowFooterDropsLowerPriorityFields(t *testing.T) {
	backend := &fakeBackend{info: app.Info{Profile: "profile", Model: "model", SessionID: "session"}}
	wide := resizeModel(t, newTestModelWithBackend(t, backend), 100, 20)
	wideContent := wide.View().Content
	if !strings.Contains(wideContent, "session") {
		t.Fatalf("wide content = %q, want session footer field", wideContent)
	}

	narrow := resizeModel(t, newTestModelWithBackend(t, backend), 40, 20)
	narrowContent := narrow.View().Content
	if !strings.Contains(narrowContent, "profile/model") {
		t.Fatalf("narrow content = %q, want profile/model footer field", narrowContent)
	}
	if strings.Contains(narrowContent, "session") {
		t.Fatalf("narrow content = %q, want lower-priority session field removed", narrowContent)
	}
}

func TestOverlayContentIncludesHelpAndSession(t *testing.T) {
	backend := &fakeBackend{info: app.Info{
		Profile:     "profile",
		Model:       "model",
		SessionID:   "session-123",
		SessionPath: "/tmp/session.jsonl",
		Provider:    "openai-compatible",
	}}
	model := resizeModel(t, newTestModelWithBackend(t, backend), 100, 30)

	updated, _ := model.Update(showHelpOverlayMsg{})
	help := updated.(Model).View().Content
	if !strings.Contains(help, "Ctrl+O") || !strings.Contains(help, "/session") {
		t.Fatalf("help overlay = %q", help)
	}

	updated, _ = updated.(Model).Update(showSessionOverlayMsg{})
	session := updated.(Model).View().Content
	if !strings.Contains(session, "session-123") || !strings.Contains(session, "/tmp/session.jsonl") || !strings.Contains(session, "openai-compatible") {
		t.Fatalf("session overlay = %q", session)
	}
}

func TestToolExpansionToggle(t *testing.T) {
	backend := &fakeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		history: []model.Message{
			{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)}}},
			{Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, ToolCallID: "call-1", ToolName: "read", Text: "tool output line"}}},
		},
	}
	model := resizeModel(t, newTestModelWithBackend(t, backend), 100, 20)
	collapsed := model.View().Content
	if strings.Contains(collapsed, "tool output line") {
		t.Fatalf("collapsed content = %q, want tool output hidden by default", collapsed)
	}

	updated, _ := model.Update(toggleToolsMsg{})
	expanded := updated.(Model)
	if !expanded.expandedTools {
		t.Fatalf("expandedTools = false, want true")
	}
	if content := expanded.View().Content; !strings.Contains(content, "tool output line") {
		t.Fatalf("expanded content = %q, want tool output shown", content)
	}
}

func TestViewportScrollDisablesAutoFollow(t *testing.T) {
	history := make([]model.Message, 0, 16)
	for i := range 16 {
		history = append(history, model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: fmt.Sprintf("entry %02d", i)}}})
	}
	model := resizeModel(t, newTestModelWithBackend(t, &fakeBackend{
		info:    app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		history: history,
	}), 80, 10)
	if !model.autoFollow {
		t.Fatalf("autoFollow = false, want true at bottom")
	}

	updated, _ := model.Update(scrollViewportMsg{Delta: -3})
	got := updated.(Model)
	if got.autoFollow {
		t.Fatalf("autoFollow = true, want false after scrolling up")
	}
}
