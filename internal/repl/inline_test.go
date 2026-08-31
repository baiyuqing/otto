package repl

import (
	"context"
	"errors"
	"testing"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

func TestInlineModelInitShowsLogoAndSession(t *testing.T) {
	backend := &fakeBackend{info: app.Info{SessionID: "sess-1"}}
	m := newInlineModel(context.Background(), backend)
	cmd := m.Init()
	if cmd == nil {
		t.Fatal("Init() returned nil cmd")
	}
}

func TestInlineModelExitCommand(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80
	m, _ = updateInline(m, tea.WindowSizeMsg{Width: 80, Height: 24})

	typeText(&m, "/exit")
	m, cmd := updateInline(m, keyEnter())
	if !m.exitReq {
		t.Fatal("exitReq not set after /exit")
	}
	if cmd == nil {
		t.Fatal("expected tea.Quit cmd")
	}
}

func TestInlineModelHelpCommand(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80
	typeText(&m, "/help")
	m, cmd := updateInline(m, keyEnter())
	if cmd == nil {
		t.Fatal("expected cmd from /help")
	}
	if m.running {
		t.Fatal("model should not be running after /help")
	}
}

func TestInlineModelSessionCommand(t *testing.T) {
	backend := &fakeBackend{info: app.Info{
		SessionID:   "s-1",
		SessionPath: "/sessions/s-1.jsonl",
		Provider:    "openai-compatible",
		Model:       "gpt-4",
	}}
	m := newInlineModel(context.Background(), backend)
	m.width = 80
	typeText(&m, "/session")
	m, cmd := updateInline(m, keyEnter())
	if cmd == nil {
		t.Fatal("expected cmd from /session")
	}
}

func TestInlineModelNewSessionCommand(t *testing.T) {
	backend := &fakeBackend{info: app.Info{SessionID: "old"}}
	backend.newSession = func() error {
		backend.info.SessionID = "new"
		return nil
	}
	m := newInlineModel(context.Background(), backend)
	m.width = 80
	typeText(&m, "/new")
	m, _ = updateInline(m, keyEnter())
	if backend.newCalls != 1 {
		t.Fatalf("newCalls = %d, want 1", backend.newCalls)
	}
}

func TestInlineModelUnknownCommand(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80
	typeText(&m, "/bogus")
	m, cmd := updateInline(m, keyEnter())
	if cmd == nil {
		t.Fatal("expected cmd for unknown command")
	}
	if m.running {
		t.Fatal("model should not be running after unknown command")
	}
}

func TestInlineModelStartsTurnOnPrompt(t *testing.T) {
	prompted := make(chan string, 1)
	backend := &fakeBackend{
		prompt: func(_ context.Context, p string, emit func(agent.Event)) error {
			prompted <- p
			emit(agent.Event{Type: agent.EventTextDelta, Text: "response"})
			return nil
		},
	}
	m := newInlineModel(context.Background(), backend)
	m.width = 80
	typeText(&m, "hello")
	m, cmd := updateInline(m, keyEnter())
	if !m.running {
		t.Fatal("model should be running after prompt submit")
	}
	if cmd == nil {
		t.Fatal("expected cmd to start turn")
	}
}

func TestInlineModelHandlesTextDelta(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.running = true
	m.turnCh = make(chan inlineTurnEnvelope)

	m, _ = updateInline(m, inlineTurnMsg{
		event: &agent.Event{Type: agent.EventTextDelta, Text: "hello\nworld"},
	})

	// "hello\n" should have been flushed, "world" remains in buffer
	if m.streamBuf != "world" {
		t.Fatalf("streamBuf = %q, want %q", m.streamBuf, "world")
	}
}

func TestInlineModelHandlesToolEvents(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.running = true
	m.turnCh = make(chan inlineTurnEnvelope)

	m, cmd := updateInline(m, inlineTurnMsg{
		event: &agent.Event{Type: agent.EventToolCallStarted, ToolName: "read", ToolCallID: "c1"},
	})
	if cmd == nil {
		t.Fatal("expected cmd after tool call started")
	}

	m, cmd = updateInline(m, inlineTurnMsg{
		event: &agent.Event{Type: agent.EventToolCallFinished, ToolName: "read", ToolCallID: "c1",
			ToolResult: tool.Result{Content: "file contents\nsecond line"}},
	})
	if cmd == nil {
		t.Fatal("expected cmd after tool call finished")
	}
}

func TestInlineModelFinishesTurn(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.running = true
	m.streamBuf = "trailing"
	m.turnCh = make(chan inlineTurnEnvelope)

	m, cmd := updateInline(m, inlineTurnMsg{done: true})
	if m.running {
		t.Fatal("model should not be running after turn done")
	}
	if m.streamBuf != "" {
		t.Fatalf("streamBuf = %q, want empty", m.streamBuf)
	}
	if cmd == nil {
		t.Fatal("expected cmd to flush remaining text")
	}
}

func TestInlineModelFinishTurnWithError(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.running = true
	m.turnCh = make(chan inlineTurnEnvelope)

	m, cmd := updateInline(m, inlineTurnMsg{done: true, err: errors.New("provider down")})
	if m.running {
		t.Fatal("model should not be running")
	}
	if cmd == nil {
		t.Fatal("expected cmd for error output")
	}
}

func TestInlineModelFatalPersistenceErrorQuits(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.running = true
	m.turnCh = make(chan inlineTurnEnvelope)

	fatalErr := errors.Join(session.ErrFatalPersistence, errors.New("disk full"))
	m, cmd := updateInline(m, inlineTurnMsg{done: true, err: fatalErr})
	if m.fatalErr == nil {
		t.Fatal("fatalErr not set")
	}
	if cmd == nil {
		t.Fatal("expected tea.Quit cmd")
	}
}

func TestInlineModelCtrlCInterruptsTurn(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	m := newInlineModel(ctx, &fakeBackend{})
	m.running = true
	m.cancel = cancel

	m, _ = updateInline(m, ctrlC())
	// cancel should have been called — verify the context was canceled
	if ctx.Err() == nil {
		t.Fatal("context should be canceled after Ctrl+C during turn")
	}
}

func TestInlineModelCtrlCQuitsWhenIdle(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m, cmd := updateInline(m, ctrlC())
	if !m.exitReq {
		t.Fatal("exitReq not set")
	}
	if cmd == nil {
		t.Fatal("expected tea.Quit cmd")
	}
}

func TestInlineModelEmptySubmitIgnored(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80
	// Don't type anything, just press Enter
	m, cmd := updateInline(m, keyEnter())
	if m.running {
		t.Fatal("should not start turn on empty input")
	}
	if cmd != nil {
		t.Fatal("expected nil cmd on empty submit")
	}
}

func TestInlineModelViewShowsEditorWhenIdle(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80
	view := m.View()
	if view.Content == "" {
		t.Fatal("View() should show editor when idle")
	}
}

func TestInlineModelViewShowsStreamBufWhenRunning(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.running = true
	m.streamBuf = "streaming..."
	view := m.View()
	if view.Content != "streaming..." {
		t.Fatalf("View() = %q, want streaming text", view.Content)
	}
}

func TestInlineModelIgnoresKeysWhileRunning(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80
	m.running = true
	m.turnCh = make(chan inlineTurnEnvelope)

	before := m.editor.Value()
	m, _ = updateInline(m, tea.KeyPressMsg{Code: 'x', Text: "x"})
	after := m.editor.Value()
	if before != after {
		t.Fatalf("editor changed while running: %q → %q", before, after)
	}
}

func TestInlineModelCompactCommand(t *testing.T) {
	backend := &fakeBackend{}
	backend.compact = func(_ context.Context, focus string, _ func(agent.Event)) (agent.CompactionResult, error) {
		if focus != "auth" {
			t.Fatalf("focus = %q, want auth", focus)
		}
		return agent.CompactionResult{TokensBefore: 50000, EstimatedTokensAfter: 10000}, nil
	}
	m := newInlineModel(context.Background(), backend)
	m.width = 80
	typeText(&m, "/compact auth")
	m, cmd := updateInline(m, keyEnter())
	if cmd == nil {
		t.Fatal("expected cmd from /compact")
	}
}

func TestInlineModelHistoryRecallsPreviousPrompt(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80

	typeText(&m, "hello")
	m, _ = updateInline(m, keyEnter())
	m.running = false // submitting a real prompt sets running=true; not under test here

	m, _ = updateInline(m, keyUp())
	if got := m.editor.Value(); got != "hello" {
		t.Fatalf("editor value = %q, want %q", got, "hello")
	}
}

func TestInlineModelHistoryCyclesThroughEntries(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80

	typeText(&m, "first")
	m, _ = updateInline(m, keyEnter())
	m.running = false
	typeText(&m, "second")
	m, _ = updateInline(m, keyEnter())
	m.running = false

	m, _ = updateInline(m, keyUp())
	if got := m.editor.Value(); got != "second" {
		t.Fatalf("editor value = %q, want %q", got, "second")
	}
	m, _ = updateInline(m, keyUp())
	if got := m.editor.Value(); got != "first" {
		t.Fatalf("editor value = %q, want %q", got, "first")
	}
}

func TestInlineModelHistoryDownReturnsToDraft(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80

	typeText(&m, "hello")
	m, _ = updateInline(m, keyEnter())
	m.running = false

	typeText(&m, "draft")
	m, _ = updateInline(m, keyUp())
	if got := m.editor.Value(); got != "hello" {
		t.Fatalf("editor value = %q, want %q", got, "hello")
	}
	m, _ = updateInline(m, keyDown())
	if got := m.editor.Value(); got != "draft" {
		t.Fatalf("editor value = %q, want %q", got, "draft")
	}
	if m.historyIndex != -1 {
		t.Fatalf("historyIndex = %d, want -1", m.historyIndex)
	}
}

func TestInlineModelHistoryUpOnEmptyHistoryIsNoop(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80

	m, cmd := updateInline(m, keyUp())
	if got := m.editor.Value(); got != "" {
		t.Fatalf("editor value = %q, want empty", got)
	}
	if cmd != nil {
		t.Fatal("expected nil cmd when history is empty")
	}
}

func TestInlineModelHistoryExcludesCommands(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80

	typeText(&m, "/help")
	m, _ = updateInline(m, keyEnter())

	m, _ = updateInline(m, keyUp())
	if got := m.editor.Value(); got != "" {
		t.Fatalf("editor value = %q, want empty (commands should not enter history)", got)
	}
}

func TestInlineModelSuggestionsShowOnPartialCommand(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80
	typeText(&m, "/he")

	found := false
	for _, s := range m.suggestions {
		if s.Name == "/help" {
			found = true
		}
	}
	if !found {
		t.Fatalf("suggestions = %+v, want /help present", m.suggestions)
	}
}

func TestInlineModelTabAcceptsSuggestion(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80
	typeText(&m, "/he")

	m, _ = updateInline(m, tea.KeyPressMsg{Code: tea.KeyTab})
	if m.editor.Value() != "/help" {
		t.Fatalf("editor value = %q, want /help", m.editor.Value())
	}
	if len(m.suggestions) != 0 {
		t.Fatalf("suggestions not cleared after accept: %+v", m.suggestions)
	}
}

func TestInlineModelEscapeClearsSuggestions(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80
	typeText(&m, "/he")
	if len(m.suggestions) == 0 {
		t.Fatal("expected suggestions before escape")
	}

	m, _ = updateInline(m, tea.KeyPressMsg{Code: tea.KeyEscape})
	if len(m.suggestions) != 0 {
		t.Fatalf("suggestions not cleared after escape: %+v", m.suggestions)
	}
}

func TestInlineModelNoSuggestionsForNonCommandText(t *testing.T) {
	m := newInlineModel(context.Background(), &fakeBackend{})
	m.width = 80
	typeText(&m, "hello")

	if len(m.suggestions) != 0 {
		t.Fatalf("suggestions = %+v, want none", m.suggestions)
	}
}

// helpers

func updateInline(m inlineModel, msg tea.Msg) (inlineModel, tea.Cmd) {
	model, cmd := m.Update(msg)
	return model.(inlineModel), cmd
}

func typeText(m *inlineModel, text string) {
	for _, ch := range text {
		model, _ := m.Update(tea.KeyPressMsg{Code: ch, Text: string(ch)})
		*m = model.(inlineModel)
	}
}

func keyEnter() tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: tea.KeyEnter}
}

func keyUp() tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: tea.KeyUp}
}

func keyDown() tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: tea.KeyDown}
}

func ctrlC() tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: 'c', Mod: tea.ModCtrl}
}
