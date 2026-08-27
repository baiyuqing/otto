package tui

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
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

func runCommandWithin(t *testing.T, cmd tea.Cmd, timeout time.Duration) tea.Msg {
	t.Helper()
	result := make(chan tea.Msg, 1)
	go func() {
		result <- cmd()
	}()
	select {
	case msg := <-result:
		return msg
	case <-time.After(timeout):
		t.Fatal("command did not return")
		return nil
	}
}

func keyPress(code rune, modifiers ...tea.KeyMod) tea.KeyPressMsg {
	var mod tea.KeyMod
	for _, modifier := range modifiers {
		mod |= modifier
	}
	key := tea.Key{Code: code, Mod: mod}
	if code >= 0x20 && code != tea.KeySpace && mod == 0 {
		key.Text = string(code)
	}
	return tea.KeyPressMsg(key)
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

func TestPromptCommandStreamsEventsAndCompletes(t *testing.T) {
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventTextDelta, Text: "hello"})
		emit(agent.Event{Type: agent.EventTextDelta, Text: " world"})
		emit(agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 3, OutputTokens: 2}})
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	if !running.running || running.editor.Value() != "" {
		t.Fatalf("running=%v editor=%q", running.running, running.editor.Value())
	}

	first := cmd()
	firstTurn, ok := first.(turnMsg)
	if !ok {
		t.Fatalf("first message type = %T, want turnMsg", first)
	}
	if cap(firstTurn.channel) != 64 {
		t.Fatalf("turn channel capacity = %d, want 64", cap(firstTurn.channel))
	}

	afterFirst, next := running.Update(first)
	streaming := afterFirst.(Model)
	if len(streaming.entries) != 2 || streaming.entries[0].Kind != EntryUser || streaming.entries[1].Kind != EntryAssistant {
		t.Fatalf("entries = %#v", streaming.entries)
	}
	if streaming.entries[1].Raw != "hello" || streaming.entries[1].Rendered != "" || !streaming.dirtyStreaming {
		t.Fatalf("streaming assistant = %#v dirty=%v", streaming.entries[1], streaming.dirtyStreaming)
	}

	nextMsg := next()
	batch, ok := nextMsg.(tea.BatchMsg)
	if !ok || len(batch) != 2 {
		t.Fatalf("next command = %T %#v, want two-command batch", nextMsg, nextMsg)
	}

	second := batch[0]()
	afterSecond, next := streaming.Update(second)
	more := afterSecond.(Model)
	if more.entries[1].Raw != "hello world" || !more.renderTickActive {
		t.Fatalf("second delta state = %#v tick=%v", more.entries[1], more.renderTickActive)
	}

	usageMsg := next()
	if _, ok := usageMsg.(tea.BatchMsg); ok {
		t.Fatalf("second delta scheduled an extra render batch: %#v", usageMsg)
	}
	afterUsage, next := more.Update(usageMsg)
	withUsage := afterUsage.(Model)
	if withUsage.usage.InputTokens != 3 || withUsage.usage.OutputTokens != 2 {
		t.Fatalf("usage = %#v", withUsage.usage)
	}

	done := next()
	afterDone, doneCmd := withUsage.Update(done)
	final := afterDone.(Model)
	if doneCmd != nil || final.running || final.cancel != nil || final.dirtyStreaming || final.renderTickActive {
		t.Fatalf("final running=%v cancel=%v dirty=%v tick=%v cmd=%v", final.running, final.cancel != nil, final.dirtyStreaming, final.renderTickActive, doneCmd)
	}
	if final.entries[1].Rendered != "hello world" || !strings.Contains(final.View().Content, "hello world") {
		t.Fatalf("final rendered assistant = %#v view=%q", final.entries[1], final.View().Content)
	}
}

func TestToolEventsUpdateTranscript(t *testing.T) {
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventToolCallStarted, ToolName: "read", ToolCallID: "call-1"})
		emit(agent.Event{Type: agent.EventToolCallFinished, ToolName: "read", ToolCallID: "call-1", ToolResult: tool.Result{Content: "README\nfull output", IsError: true}})
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	first := cmd()
	afterStart, next := running.Update(first)
	started := afterStart.(Model)
	if len(started.entries) != 2 || started.entries[1].Kind != EntryTool || started.entries[1].ToolName != "read" || started.entries[1].ToolDone {
		t.Fatalf("started entries = %#v", started.entries)
	}

	finished := next()
	afterFinish, next := started.Update(finished)
	result := afterFinish.(Model)
	if !result.entries[1].ToolDone || !result.entries[1].ToolError || result.entries[1].ToolOutput != "README\nfull output" {
		t.Fatalf("finished tool entry = %#v", result.entries[1])
	}

	afterDone, doneCmd := result.Update(next())
	idle := afterDone.(Model)
	if doneCmd != nil || idle.running || idle.cancel != nil {
		t.Fatalf("idle running=%v cancel=%v cmd=%v", idle.running, idle.cancel != nil, doneCmd)
	}
}

func TestDraftRemainsEditableWhileRunningAndEnterDoesNotQueue(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		close(started)
		<-release
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	turnDone := make(chan tea.Msg, 1)
	go func() { turnDone <- cmd() }()
	<-started

	typed, _ := running.Update(keyPress('n'))
	withDraft := typed.(Model)
	if withDraft.editor.Value() != "n" {
		t.Fatalf("draft = %q, want editable draft while running", withDraft.editor.Value())
	}

	submitted, submitCmd := withDraft.Update(keyPress(tea.KeyEnter))
	stillRunning := submitted.(Model)
	if submitCmd != nil || stillRunning.editor.Value() != "n" || !stillRunning.running {
		t.Fatalf("running=%v draft=%q cmd=%v", stillRunning.running, stillRunning.editor.Value(), submitCmd)
	}

	close(release)
	select {
	case <-turnDone:
	case <-time.After(time.Second):
		t.Fatal("turn command did not complete")
	}
}

func TestEscapeCancelsActiveTurnAndWaitsForCompletion(t *testing.T) {
	started := make(chan struct{})
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		close(started)
		<-ctx.Done()
		return ctx.Err()
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	turnDone := make(chan tea.Msg, 1)
	go func() { turnDone <- cmd() }()
	<-started

	cancelled, cancelCmd := running.Update(keyPress(tea.KeyEscape))
	stillRunning := cancelled.(Model)
	if cancelCmd != nil || !stillRunning.running {
		t.Fatalf("running=%v cmd=%v, want active until done", stillRunning.running, cancelCmd)
	}

	var done tea.Msg
	select {
	case done = <-turnDone:
	case <-time.After(time.Second):
		t.Fatal("canceled turn did not complete")
	}

	afterDone, doneCmd := stillRunning.Update(done)
	idle := afterDone.(Model)
	if doneCmd != nil || idle.running || idle.cancel != nil {
		t.Fatalf("idle running=%v cancel=%v cmd=%v", idle.running, idle.cancel != nil, doneCmd)
	}
	if len(idle.entries) == 0 || idle.entries[len(idle.entries)-1].Kind != EntryError || !strings.Contains(idle.entries[len(idle.entries)-1].Raw, context.Canceled.Error()) {
		t.Fatalf("entries = %#v", idle.entries)
	}
}

func TestPromptErrorLeavesModelUsable(t *testing.T) {
	providerErr := errors.New("provider offline")
	releaseFirst := make(chan struct{})
	calls := 0
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		calls++
		if calls == 1 {
			emit(agent.Event{Type: agent.EventAgentError, Err: providerErr})
			<-releaseFirst
			return providerErr
		}
		emit(agent.Event{Type: agent.EventTextDelta, Text: "recovered"})
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("first")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	firstTurn, waitDone := updated.(Model).Update(cmd())
	afterErrEvent := firstTurn.(Model)
	if waitDone == nil || !afterErrEvent.running || afterErrEvent.cancel == nil {
		t.Fatalf("error event state running=%v cancel=%v cmd=%v", afterErrEvent.running, afterErrEvent.cancel != nil, waitDone)
	}
	if len(afterErrEvent.entries) == 0 || afterErrEvent.entries[len(afterErrEvent.entries)-1].Kind != EntryError || !strings.Contains(afterErrEvent.entries[len(afterErrEvent.entries)-1].Raw, providerErr.Error()) {
		t.Fatalf("entries = %#v", afterErrEvent.entries)
	}

	blocked, blockedCmd := afterErrEvent.Update(keyPress(tea.KeyEnter))
	if blockedCmd != nil || !blocked.(Model).running {
		t.Fatalf("resubmit before done running=%v cmd=%v", blocked.(Model).running, blockedCmd)
	}

	close(releaseFirst)
	completed, completionCmd := afterErrEvent.Update(waitDone())
	afterErr := completed.(Model)
	if completionCmd != nil || afterErr.running || afterErr.cancel != nil {
		t.Fatalf("completion state running=%v cancel=%v cmd=%v", afterErr.running, afterErr.cancel != nil, completionCmd)
	}
	if got := countEntriesOfKind(afterErr.entries, EntryError); got != 1 {
		t.Fatalf("error entries = %d, want exactly one: %#v", got, afterErr.entries)
	}

	afterErr.editor.SetValue("second")
	retryUpdated, retryCmd := afterErr.Update(keyPress(tea.KeyEnter))
	retrying := retryUpdated.(Model)
	if !retrying.running || retryCmd == nil {
		t.Fatalf("retry running=%v cmd=%v", retrying.running, retryCmd)
	}
}

func TestFatalPersistenceErrorQuitsAfterCompletion(t *testing.T) {
	fatalErr := errors.Join(session.ErrFatalPersistence, errors.New("disk full"))
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventAgentError, Err: fatalErr})
		return fatalErr
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	afterEvent, waitDone := updated.(Model).Update(cmd())
	waiting := afterEvent.(Model)
	if !waiting.running || waitDone == nil || waiting.fatalErr != nil {
		t.Fatalf("event state running=%v wait=%v fatalErr=%v", waiting.running, waitDone != nil, waiting.fatalErr)
	}

	afterDone, quitCmd := waiting.Update(waitDone())
	fatalModel := afterDone.(Model)
	if fatalModel.running || fatalModel.cancel != nil || fatalModel.fatalErr == nil || !errors.Is(fatalModel.fatalErr, session.ErrFatalPersistence) {
		t.Fatalf("completion state running=%v cancel=%v fatalErr=%v", fatalModel.running, fatalModel.cancel != nil, fatalModel.fatalErr)
	}
	if quitCmd == nil {
		t.Fatal("quit command = nil, want tea.Quit")
	}
	if msg := quitCmd(); msg == nil {
		t.Fatalf("quit command message = %T, want non-nil quit message", msg)
	}
	if got := countEntriesOfKind(fatalModel.entries, EntryError); got != 1 {
		t.Fatalf("error entries = %d, want exactly one: %#v", got, fatalModel.entries)
	}
}

func TestStreamingRenderTickThenLaterDeltaCompletesWithFullRaw(t *testing.T) {
	releaseSecond := make(chan struct{})
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventTextDelta, Text: "first"})
		<-releaseSecond
		emit(agent.Event{Type: agent.EventTextDelta, Text: " second"})
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, start := m.Update(keyPress(tea.KeyEnter))
	afterFirst, batchCmd := updated.(Model).Update(start())
	batch := batchCmd().(tea.BatchMsg)

	afterTick, _ := afterFirst.(Model).Update(batch[1]())
	renderedFirst := afterTick.(Model)
	if renderedFirst.entries[1].Rendered != "first" || renderedFirst.dirtyStreaming {
		t.Fatalf("first tick entry=%#v dirty=%v", renderedFirst.entries[1], renderedFirst.dirtyStreaming)
	}

	close(releaseSecond)
	afterSecond, secondBatchCmd := renderedFirst.Update(batch[0]())
	withSecond := afterSecond.(Model)
	if withSecond.entries[1].Raw != "first second" || !withSecond.dirtyStreaming {
		t.Fatalf("second delta entry=%#v dirty=%v", withSecond.entries[1], withSecond.dirtyStreaming)
	}
	secondBatch := secondBatchCmd().(tea.BatchMsg)
	afterDone, _ := withSecond.Update(secondBatch[0]())
	completed := afterDone.(Model)
	if completed.entries[1].Rendered != "first second" || !strings.Contains(completed.View().Content, "first second") {
		t.Fatalf("completed entry=%#v view=%q", completed.entries[1], completed.View().Content)
	}
}

func TestToolTransitionFlushesDirtyAssistantText(t *testing.T) {
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventTextDelta, Text: "before tool"})
		emit(agent.Event{Type: agent.EventToolCallStarted, ToolName: "read", ToolCallID: "call-1"})
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, start := m.Update(keyPress(tea.KeyEnter))
	afterText, batchCmd := updated.(Model).Update(start())
	batch := batchCmd().(tea.BatchMsg)
	afterTool, _ := afterText.(Model).Update(batch[0]())
	got := afterTool.(Model)
	if len(got.entries) != 3 || got.entries[1].Rendered != "before tool" || got.dirtyStreaming {
		t.Fatalf("tool transition entries=%#v dirty=%v", got.entries, got.dirtyStreaming)
	}
	if got.activeAssistant != -1 {
		t.Fatalf("activeAssistant=%d, want -1 after tool transition", got.activeAssistant)
	}
}

func TestToolResultTransitionFlushesDirtyAssistantText(t *testing.T) {
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventTextDelta, Text: "before result"})
		emit(agent.Event{
			Type:       agent.EventToolCallFinished,
			ToolName:   "read",
			ToolCallID: "call-1",
			ToolResult: tool.Result{Content: "result"},
		})
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, start := m.Update(keyPress(tea.KeyEnter))
	afterText, batchCmd := updated.(Model).Update(start())
	batch := batchCmd().(tea.BatchMsg)
	afterResult, _ := afterText.(Model).Update(batch[0]())
	got := afterResult.(Model)
	if len(got.entries) != 3 || got.entries[1].Rendered != "before result" || got.dirtyStreaming {
		t.Fatalf("tool result transition entries=%#v dirty=%v", got.entries, got.dirtyStreaming)
	}
	if got.activeAssistant != -1 || !got.entries[2].ToolDone {
		t.Fatalf("activeAssistant=%d tool=%#v", got.activeAssistant, got.entries[2])
	}
}

func TestCanceledFullTurnChannelDeliversRealCompletion(t *testing.T) {
	completionErr := errors.New("backend completion identity")
	backendFinished := make(chan struct{})
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		defer close(backendFinished)
		for i := 0; i < turnChannelCapacity+16; i++ {
			emit(agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 1}})
		}
		<-ctx.Done()
		return completionErr
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, start := m.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	first := start().(turnMsg)
	deadline := time.Now().Add(time.Second)
	for len(first.channel) < turnChannelCapacity-1 && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if len(first.channel) < turnChannelCapacity-1 {
		t.Fatalf("turn channel only filled to %d", len(first.channel))
	}
	time.Sleep(20 * time.Millisecond)
	cancelled, _ := running.Update(keyPress(tea.KeyEscape))
	if !cancelled.(Model).running {
		t.Fatal("model stopped before completion")
	}
	time.Sleep(20 * time.Millisecond)

	doneCount := 0
	var gotErr error
	current := first
	for {
		if current.value.done {
			doneCount++
			gotErr = current.value.err
			break
		}
		current = runCommandWithin(t, waitTurn(first.stream), time.Second).(turnMsg)
	}
	if doneCount != 1 || !errors.Is(gotErr, completionErr) {
		t.Fatalf("completion count=%d err=%v, want one real %v", doneCount, gotErr, completionErr)
	}
	select {
	case extra := <-first.channel:
		t.Fatalf("unexpected envelope after completion: %#v", extra)
	default:
	}
	select {
	case <-backendFinished:
	case <-time.After(time.Second):
		t.Fatal("backend worker did not exit")
	}
}

func TestStaleRenderTickDoesNotMutateNextTurn(t *testing.T) {
	releaseFirst := make(chan struct{})
	releaseSecond := make(chan struct{})
	calls := 0
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		calls++
		if calls == 1 {
			emit(agent.Event{Type: agent.EventTextDelta, Text: "turn A"})
			<-releaseFirst
			return nil
		}
		emit(agent.Event{Type: agent.EventTextDelta, Text: "turn B"})
		<-releaseSecond
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("first")

	startedA, startA := m.Update(keyPress(tea.KeyEnter))
	afterDeltaA, batchCmdA := startedA.(Model).Update(startA())
	batchA := batchCmdA().(tea.BatchMsg)
	staleTick := batchA[1]
	close(releaseFirst)
	finishedA, _ := afterDeltaA.(Model).Update(batchA[0]())

	idle := finishedA.(Model)
	idle.editor.SetValue("second")
	startedB, startB := idle.Update(keyPress(tea.KeyEnter))
	afterDeltaB, _ := startedB.(Model).Update(startB())
	streamingB := afterDeltaB.(Model)
	if !streamingB.dirtyStreaming || !streamingB.renderTickActive {
		t.Fatalf("turn B dirty=%v tick=%v", streamingB.dirtyStreaming, streamingB.renderTickActive)
	}

	afterStale, _ := streamingB.Update(staleTick())
	got := afterStale.(Model)
	if !got.dirtyStreaming || !got.renderTickActive || got.entries[len(got.entries)-1].Rendered != "" {
		t.Fatalf("stale tick changed turn B: dirty=%v tick=%v entry=%#v", got.dirtyStreaming, got.renderTickActive, got.entries[len(got.entries)-1])
	}
	close(releaseSecond)
}

func TestViewportRefreshPreservesOffsetBeforeTemporaryClamp(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 40, 10)
	m.viewport.SetContent(strings.Repeat("old line\n", 59))
	m.viewport.SetHeight(5)
	m.viewport.SetYOffset(40)
	m.autoFollow = false
	width := m.transcriptWidth()
	longContent := strings.Repeat("new line\n", 119)
	m.entries = []Entry{{ID: "assistant", Kind: EntryAssistant, Raw: longContent, Rendered: longContent, RenderWidth: width}}

	updated, _ := m.Update(tea.WindowSizeMsg{Width: 40, Height: 30})
	got := updated.(Model)
	if got.viewport.YOffset() != 40 {
		t.Fatalf("viewport offset=%d, want exact prior offset 40", got.viewport.YOffset())
	}
}

func countEntriesOfKind(entries []Entry, kind EntryKind) int {
	count := 0
	for _, entry := range entries {
		if entry.Kind == kind {
			count++
		}
	}
	return count
}

func TestTurnChannelCancellationDoesNotLeakWorker(t *testing.T) {
	for i := 0; i < 16; i++ {
		finished := make(chan struct{})
		backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
			defer close(finished)
			emit(agent.Event{Type: agent.EventTextDelta, Text: "x"})
			for {
				select {
				case <-ctx.Done():
					return ctx.Err()
				default:
					emit(agent.Event{Type: agent.EventTextDelta, Text: "y"})
				}
			}
		}}
		m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
		m.editor.SetValue("question")

		updated, cmd := m.Update(keyPress(tea.KeyEnter))
		running := updated.(Model)
		first := cmd()
		afterFirst, next := running.Update(first)
		nextMsg := next()
		if batch, ok := nextMsg.(tea.BatchMsg); !ok || len(batch) != 2 {
			t.Fatalf("iteration %d next command = %#v, want wait+render batch", i, nextMsg)
		}
		cancelled, _ := afterFirst.(Model).Update(keyPress(tea.KeyEscape))
		_ = cancelled.(Model)

		select {
		case <-finished:
		case <-time.After(time.Second):
			t.Fatalf("iteration %d worker did not exit after cancellation", i)
		}
	}
}
