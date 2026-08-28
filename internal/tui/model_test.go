package tui

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"image/color"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"

	"charm.land/bubbles/v2/spinner"
	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
	"github.com/charmbracelet/x/ansi"
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

type fakeClock struct {
	mu     sync.Mutex
	now    time.Time
	timers []fakeTimer
}

type fakeTimer struct {
	at time.Time
	ch chan time.Time
}

func newFakeClock(now time.Time) *fakeClock {
	return &fakeClock{now: now}
}

func (f *fakeClock) Now() time.Time {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.now
}

func (f *fakeClock) After(d time.Duration) <-chan time.Time {
	f.mu.Lock()
	defer f.mu.Unlock()
	ch := make(chan time.Time, 1)
	f.timers = append(f.timers, fakeTimer{at: f.now.Add(d), ch: ch})
	return ch
}

func (f *fakeClock) Advance(d time.Duration) {
	f.mu.Lock()
	f.now = f.now.Add(d)
	now := f.now
	due := make([]fakeTimer, 0, len(f.timers))
	pending := f.timers[:0]
	for _, timer := range f.timers {
		if !timer.at.After(now) {
			due = append(due, timer)
			continue
		}
		pending = append(pending, timer)
	}
	f.timers = pending
	f.mu.Unlock()

	for _, timer := range due {
		timer.ch <- timer.at
	}
}

func (f *fakeClock) WaitForTimers(t *testing.T, want int, timeout time.Duration) {
	t.Helper()
	deadline := time.Now().Add(timeout)
	for {
		f.mu.Lock()
		got := len(f.timers)
		f.mu.Unlock()
		if got >= want {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("timers = %d, want at least %d", got, want)
		}
		time.Sleep(time.Millisecond)
	}
}

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
	if code >= 0x20 && code < tea.KeyExtended && code != tea.KeySpace && mod == 0 {
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

func TestInitRequestsBackgroundColorAndStartsSingleSpinnerTick(t *testing.T) {
	m := newTestModel(t)
	initCmd := m.Init()
	if initCmd == nil {
		t.Fatal("Init() command = nil")
	}
	initMsg := initCmd()
	batch, ok := initMsg.(tea.BatchMsg)
	if !ok || len(batch) != 2 {
		t.Fatalf("Init() message = %T %#v, want two-command batch", initMsg, initMsg)
	}
	var requestedBackground, tickedSpinner bool
	for _, cmd := range batch {
		msg := cmd()
		if reflect.TypeOf(msg) == reflect.TypeOf(tea.RequestBackgroundColor()) {
			requestedBackground = true
		}
		if _, ok := msg.(spinner.TickMsg); ok {
			tickedSpinner = true
		}
	}
	if !requestedBackground || !tickedSpinner {
		t.Fatalf("background request=%v spinner tick=%v", requestedBackground, tickedSpinner)
	}
}

func TestBackgroundColorMessageCachesDarkAndLightRenderers(t *testing.T) {
	m := newTestModelWithBackend(t, &fakeBackend{history: []model.Message{{
		Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "history"}},
	}}})
	// Remove the injected test renderer so this test exercises the production renderer.
	m.renderer = newGlamourRenderer(true)
	m.rendererInjected = false

	updated, _ := m.Update(tea.BackgroundColorMsg{Color: color.RGBA{R: 0xff, G: 0xff, B: 0xff, A: 0xff}})
	light := updated.(Model)
	lightRenderer, ok := light.renderer.(GlamourRenderer)
	if !ok || light.darkBackground || lightRenderer.styleName != "light" {
		t.Fatalf("light renderer = %#v dark=%v", light.renderer, light.darkBackground)
	}

	updated, _ = light.Update(tea.BackgroundColorMsg{Color: color.RGBA{A: 0xff}})
	dark := updated.(Model)
	darkRenderer, ok := dark.renderer.(GlamourRenderer)
	if !ok || !dark.darkBackground || darkRenderer.styleName != "dark" {
		t.Fatalf("dark renderer = %#v dark=%v", dark.renderer, dark.darkBackground)
	}
}

func TestBackgroundColorMessagePreservesInjectedRenderer(t *testing.T) {
	injected := rendererFunc(func(text string, _ int) (string, error) { return "injected:" + text, nil })
	m := NewModel(context.Background(), &fakeBackend{}, WithRenderer(injected))
	updated, _ := m.Update(tea.BackgroundColorMsg{Color: color.RGBA{R: 0xff, G: 0xff, B: 0xff, A: 0xff}})
	got := updated.(Model)
	if _, ok := got.renderer.(rendererFunc); !ok || !got.rendererInjected {
		t.Fatalf("renderer = %T injected=%v, want injected renderer", got.renderer, got.rendererInjected)
	}
}

func TestRunningTurnRendersAndAdvancesSpinnerOneTickAtATime(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.editor.SetValue("question")
	updated, _ := m.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	if content := running.View().Content; !strings.Contains(content, running.spinner.View()) || !strings.Contains(content, "working") {
		t.Fatalf("running view = %q, want visible spinner status", content)
	}
	updated, next := running.Update(running.spinner.Tick())
	advanced := updated.(Model)
	if next == nil || advanced.spinner.View() == running.spinner.View() {
		t.Fatalf("spinner did not advance exactly one frame: before=%q after=%q next=%v", running.spinner.View(), advanced.spinner.View(), next)
	}
}

func TestWindowResizeProducesResponsiveLayout(t *testing.T) {
	model := newTestModel(t)
	got := resizeModel(t, model, 100, 30)
	if got.viewport.Width() != 100 || got.viewport.Height() <= 0 {
		t.Fatalf("viewport = %dx%d", got.viewport.Width(), got.viewport.Height())
	}
	view := got.View()
	if !view.AltScreen || view.MouseMode != tea.MouseModeCellMotion || !strings.Contains(view.Content, "profile/model") {
		t.Fatalf("view = %#v, want alternate screen with mouse-wheel reporting", view)
	}
}

func TestViewPositionsRealCursorAtEditorLocation(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.editor.SetValue("hello")
	m.editor.CursorEnd()

	cursor := m.View().Cursor
	if cursor == nil {
		t.Fatal("view cursor = nil, want a real terminal cursor for IME positioning")
	}
	if cursor.X != 7 || cursor.Y != 10 {
		t.Fatalf("view cursor = (%d,%d), want (7,10) at the visible editor", cursor.X, cursor.Y)
	}
}

func TestViewKeepsRealCursorAtEditorWhenSuggestionsAreVisible(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.editor.SetValue("/")
	m.editor.CursorEnd()
	m.rerenderAndRefreshViewportContent(false)
	if len(m.commandSuggestions()) == 0 {
		t.Fatal("test setup has no slash-command suggestions")
	}

	cursor := m.View().Cursor
	if cursor == nil || cursor.X != 3 || cursor.Y != 10 {
		t.Fatalf("suggestion view cursor = %#v, want (3,10) at the editor", cursor)
	}
}

func TestViewTracksRealCursorAcrossMultilineEditorRows(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.editor.SetValue("first\nsecond")
	m.rerenderAndRefreshViewportContent(false)

	m.editor.CursorUp()
	m.editor.CursorStart()
	first := m.View().Cursor
	if first == nil || first.Y != 9 {
		t.Fatalf("first-line cursor = %#v, want row 9", first)
	}

	m.editor.CursorDown()
	m.editor.CursorEnd()
	last := m.View().Cursor
	if last == nil || last.Y != 10 {
		t.Fatalf("last-line cursor = %#v, want row 10", last)
	}
}

func TestOverlayHidesRealCursor(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.overlay = overlayHelp

	if cursor := m.View().Cursor; cursor != nil {
		t.Fatalf("help overlay cursor = (%d,%d), want hidden", cursor.X, cursor.Y)
	}

	m.overlay = overlaySession
	if cursor := m.View().Cursor; cursor != nil {
		t.Fatalf("session overlay cursor = (%d,%d), want hidden", cursor.X, cursor.Y)
	}
}

func TestSmallTerminalShowsResizeMessage(t *testing.T) {
	model := newTestModel(t)
	got := resizeModel(t, model, 30, 6)
	view := got.View()
	if !strings.Contains(view.Content, "terminal is too small") {
		t.Fatalf("content = %q", view.Content)
	}
	if cursor := view.Cursor; cursor != nil {
		t.Fatalf("small-terminal cursor = (%d,%d), want hidden", cursor.X, cursor.Y)
	}

	restored := resizeModel(t, got, 100, 30)
	if content := restored.View().Content; strings.Contains(content, "terminal is too small") || !strings.Contains(content, "profile/model") {
		t.Fatalf("restored content = %q", content)
	}
}

func TestViewResetsMalformedAssistantStyleBeforeNextEntryAndFooter(t *testing.T) {
	backend := &fakeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		history: []model.Message{
			{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "&#27;[31mred&#27;]unterminated"}}},
			{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "next entry"}}},
		},
	}
	view := resizeModel(t, NewModel(context.Background(), backend), 100, 30).View().Content
	red := strings.Index(view, "red")
	next := strings.Index(view, "next entry")
	footer := strings.Index(view, "profile/model")
	if red < 0 || next <= red || footer <= next {
		t.Fatalf("view does not contain entries and footer in order: %q", view)
	}
	if reset := strings.Index(view[red:next], "\x1b[0m"); reset < 0 {
		t.Fatalf("assistant SGR was not reset before the next entry: %q", view)
	}
	if reset := strings.Index(view[red:footer], "\x1b[0m"); reset < 0 {
		t.Fatalf("assistant SGR was not reset before the footer: %q", view)
	}
}

func TestInitialUsagePrefersBackendAggregateOverCompactedHistoryFallback(t *testing.T) {
	backend := &fakeBackend{
		info: app.Info{
			Profile: "profile", Model: "model", SessionID: "session",
			Usage: model.Usage{InputTokens: 20, OutputTokens: 6}, UsagePresent: true,
		},
		history: []model.Message{{
			Role: model.RoleContext, ContextType: "compaction", Display: true,
			Blocks: []model.Block{{Type: model.BlockText, Text: "[Compaction summary]\ncompact"}},
			Usage:  &model.Usage{InputTokens: 6, OutputTokens: 1},
		}},
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 100, 12)
	if m.usage != backend.info.Usage {
		t.Fatalf("model usage = %#v, want aggregate %#v", m.usage, backend.info.Usage)
	}
	if content := m.View().Content; !strings.Contains(content, "tokens 20/6") {
		t.Fatalf("footer = %q, want aggregate usage", content)
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
	if !strings.Contains(help, "Ctrl+O") || !strings.Contains(help, "Shift+drag") || !strings.Contains(help, "/session") {
		t.Fatalf("help overlay = %q, want tool and terminal-selection guidance", help)
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
	collapsedByDefault := model.View().Content
	if model.expandedTools || strings.Contains(collapsedByDefault, "tool output line") {
		t.Fatalf("default expanded=%v content=%q, want tool output folded", model.expandedTools, collapsedByDefault)
	}

	updated, _ := model.Update(toggleToolsMsg{})
	expanded := updated.(Model)
	if !expanded.expandedTools {
		t.Fatalf("expandedTools = false after toggle, want true")
	}
	if content := expanded.View().Content; !strings.Contains(content, "tool output line") || !strings.Contains(content, `{"path":"README.md"}`) {
		t.Fatalf("expanded content = %q, want full arguments and output", content)
	}

	toggledBack, _ := expanded.Update(toggleToolsMsg{})
	if toggledBack.(Model).expandedTools {
		t.Fatalf("expandedTools = true after second toggle, want false")
	}
	if content := toggledBack.View().Content; strings.Contains(content, "tool output line") {
		t.Fatalf("re-collapsed content = %q, want tool output hidden", content)
	}
}

func TestCollapsedToolSummaryIsSingleLineBoundedAndExpandable(t *testing.T) {
	entry := Entry{
		Kind:       EntryTool,
		ToolName:   "write",
		ToolArgs:   "{\"path\":\"README.md\",\n\"content\":\"long argument SECOND-TAIL\"}",
		ToolOutput: "complete output THIRD-TAIL",
		ToolDone:   true,
	}

	const width = 40
	collapsed := renderToolBlock(entry, width, false)
	if strings.Contains(collapsed, "\n") || ansi.StringWidth(collapsed) > width {
		t.Fatalf("collapsed summary = %q width=%d, want one bounded line", collapsed, ansi.StringWidth(collapsed))
	}
	if !strings.Contains(collapsed, "write") || !strings.Contains(collapsed, "complete") || strings.Contains(collapsed, "SECOND-TAIL") || strings.Contains(collapsed, "THIRD-TAIL") {
		t.Fatalf("collapsed summary = %q, want concise name/argument preview/status without full details", collapsed)
	}

	expanded := renderToolBlock(entry, width, true)
	for _, want := range []string{`{"path":"README.md",`, `"content":"long argument SECOND-TAIL"}`, "complete output THIRD-TAIL"} {
		if !strings.Contains(expanded, want) {
			t.Fatalf("expanded tool = %q, want %q", expanded, want)
		}
	}
}

func TestExpandedToolPreservesArgumentAndOutputWhitespace(t *testing.T) {
	entry := Entry{
		Kind:       EntryTool,
		ToolName:   "bash",
		ToolArgs:   "\n  {\"command\":\"printf hi\"}\t\n",
		ToolOutput: " \n output with boundaries \t\n",
		ToolDone:   true,
	}

	expanded := renderToolBlock(entry, 80, true)
	if !strings.Contains(expanded, "Arguments:\n\n  {\"command\":\"printf hi\"}\t\n") {
		t.Fatalf("expanded tool = %q, want exact argument boundary whitespace", expanded)
	}
	if !strings.Contains(expanded, "Output:\n \n output with boundaries \t\n") {
		t.Fatalf("expanded tool = %q, want exact output boundary whitespace", expanded)
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

func TestEnterSubmitsAndAltEnterAddsNewline(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.editor.SetValue("one")

	updated, _ := m.Update(keyPress(tea.KeyEnter, tea.ModAlt))
	withNewline := updated.(Model)
	if !strings.Contains(withNewline.editor.Value(), "\n") {
		t.Fatalf("value = %q", withNewline.editor.Value())
	}

	withNewline.editor.SetValue("send")
	submitted, promptCmd := withNewline.Update(keyPress(tea.KeyEnter))
	if promptCmd == nil || !submitted.(Model).running {
		t.Fatal("enter did not submit")
	}
}

func TestShiftEnterRequiresKeyboardEnhancements(t *testing.T) {
	withoutEnhancements := resizeModel(t, newTestModel(t), 80, 12)
	withoutEnhancements.editor.SetValue("one")
	updated, _ := withoutEnhancements.Update(keyPress(tea.KeyEnter, tea.ModShift))
	if strings.Contains(updated.(Model).editor.Value(), "\n") {
		t.Fatalf("shift+enter inserted newline without keyboard enhancements: %q", updated.(Model).editor.Value())
	}

	withEnhancements, _ := withoutEnhancements.Update(tea.KeyboardEnhancementsMsg{Flags: 1})
	enhanced := withEnhancements.(Model)
	updated, _ = enhanced.Update(keyPress(tea.KeyEnter, tea.ModShift))
	if !strings.Contains(updated.(Model).editor.Value(), "\n") {
		t.Fatalf("shift+enter value = %q", updated.(Model).editor.Value())
	}
}

func TestCtrlOTogglesToolExpansionKey(t *testing.T) {
	backend := &fakeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		history: []model.Message{
			{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)}}},
			{Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, ToolCallID: "call-1", ToolName: "read", Text: "tool output line"}}},
		},
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 100, 20)
	if m.expandedTools || strings.Contains(m.View().Content, "tool output line") {
		t.Fatalf("default expanded=%v content=%q, want output folded", m.expandedTools, m.View().Content)
	}

	updated, _ := m.Update(keyPress('o', tea.ModCtrl))
	expanded := updated.(Model)
	if !expanded.expandedTools || !strings.Contains(expanded.View().Content, "tool output line") {
		t.Fatalf("after ctrl+o expanded=%v content=%q, want output visible", expanded.expandedTools, expanded.View().Content)
	}

	toggledBack, _ := expanded.Update(keyPress('o', tea.ModCtrl))
	got := toggledBack.(Model)
	if got.expandedTools || strings.Contains(got.View().Content, "tool output line") {
		t.Fatalf("after second ctrl+o expanded=%v content=%q, want output folded", got.expandedTools, got.View().Content)
	}
}

func TestPageKeysAndHomeEndRouteBetweenViewportAndEditor(t *testing.T) {
	history := make([]model.Message, 0, 24)
	for i := range 24 {
		history = append(history, model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: fmt.Sprintf("entry %02d", i)}}})
	}
	m := resizeModel(t, newTestModelWithBackend(t, &fakeBackend{
		info:    app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		history: history,
	}), 80, 10)
	m.viewport.SetYOffset(6)
	m.autoFollow = false

	updated, _ := m.Update(keyPress(tea.KeyPgUp))
	pageUp := updated.(Model)
	if pageUp.viewport.YOffset() >= 6 {
		t.Fatalf("pgup offset = %d, want < 6", pageUp.viewport.YOffset())
	}

	updated, _ = pageUp.Update(keyPress(tea.KeyPgDown))
	pageDown := updated.(Model)
	if pageDown.viewport.YOffset() <= pageUp.viewport.YOffset() {
		t.Fatalf("pgdn offset = %d, want > %d", pageDown.viewport.YOffset(), pageUp.viewport.YOffset())
	}

	pageDown.viewport.SetYOffset(4)
	pageDown.autoFollow = false
	pageDown.editor.SetValue("abc")
	updated, _ = pageDown.Update(keyPress(tea.KeyHome))
	consumedHome := updated.(Model)
	if consumedHome.editor.Column() != 0 {
		t.Fatalf("home column = %d, want 0", consumedHome.editor.Column())
	}
	if consumedHome.viewport.YOffset() != 4 {
		t.Fatalf("home consumed viewport offset = %d, want 4", consumedHome.viewport.YOffset())
	}

	updated, _ = consumedHome.Update(keyPress(tea.KeyHome))
	transcriptTop := updated.(Model)
	if transcriptTop.viewport.YOffset() != 0 {
		t.Fatalf("home transcript offset = %d, want 0", transcriptTop.viewport.YOffset())
	}

	updated, _ = transcriptTop.Update(keyPress(tea.KeyEnd))
	consumedEnd := updated.(Model)
	if consumedEnd.editor.Column() != len("abc") {
		t.Fatalf("end column = %d, want %d", consumedEnd.editor.Column(), len("abc"))
	}
	if consumedEnd.viewport.YOffset() != 0 {
		t.Fatalf("end consumed viewport offset = %d, want 0", consumedEnd.viewport.YOffset())
	}

	updated, _ = consumedEnd.Update(keyPress(tea.KeyEnd))
	transcriptBottom := updated.(Model)
	if !transcriptBottom.viewport.AtBottom() || !transcriptBottom.autoFollow {
		t.Fatalf("end did not route to transcript bottom: offset=%d autoFollow=%v", transcriptBottom.viewport.YOffset(), transcriptBottom.autoFollow)
	}
}

func TestQuestionMarkOpensHelpOnlyWhenEditorIsEmpty(t *testing.T) {
	empty := resizeModel(t, newTestModel(t), 80, 12)
	updated, _ := empty.Update(keyPress('?'))
	if updated.(Model).overlay != overlayHelp {
		t.Fatalf("overlay = %v, want help", updated.(Model).overlay)
	}

	nonEmpty := resizeModel(t, newTestModel(t), 80, 12)
	nonEmpty.editor.SetValue("hello")
	updated, _ = nonEmpty.Update(keyPress('?'))
	got := updated.(Model)
	if got.overlay != overlayNone || got.editor.Value() != "hello?" {
		t.Fatalf("overlay=%v value=%q", got.overlay, got.editor.Value())
	}
}

func TestSlashCommandsOpenModalOverlaysWithoutPrompting(t *testing.T) {
	prompted := false
	backend := &fakeBackend{info: app.Info{
		Profile:     "profile",
		Model:       "model",
		SessionID:   "session-123",
		SessionPath: "/tmp/session.jsonl",
		Provider:    "openai-compatible",
	}, prompt: func(context.Context, string, func(agent.Event)) error {
		prompted = true
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 100, 20)
	m.editor.SetValue("  /help  ")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	help := updated.(Model)
	if cmd != nil || help.overlay != overlayHelp || help.editor.Value() != "" {
		t.Fatalf("help overlay=%v editor=%q cmd=%v", help.overlay, help.editor.Value(), cmd)
	}

	sessionModel := resizeModel(t, newTestModelWithBackend(t, backend), 100, 20)
	sessionModel.editor.SetValue(" /session ")
	updated, cmd = sessionModel.Update(keyPress(tea.KeyEnter))
	sessionOverlay := updated.(Model)
	if cmd != nil || sessionOverlay.overlay != overlaySession || sessionOverlay.editor.Value() != "" {
		t.Fatalf("session overlay=%v editor=%q cmd=%v", sessionOverlay.overlay, sessionOverlay.editor.Value(), cmd)
	}
	if content := sessionOverlay.View().Content; !strings.Contains(content, "session-123") || !strings.Contains(content, "/tmp/session.jsonl") {
		t.Fatalf("session content = %q", content)
	}
	if prompted {
		t.Fatal("slash command reached backend prompt")
	}
}

func TestOverlaySwallowsPasteAndMouseInput(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.editor.SetValue("draft")
	m.viewport.SetContent(strings.Repeat("line\n", 40))
	m.viewport.SetYOffset(10)
	m.overlay = overlayHelp

	inputs := []tea.Msg{
		tea.PasteMsg{Content: " pasted"},
		tea.MouseClickMsg(tea.Mouse{X: 2, Y: 2, Button: tea.MouseLeft}),
		tea.MouseWheelMsg(tea.Mouse{X: 2, Y: 2, Button: tea.MouseWheelUp}),
		tea.MouseMotionMsg(tea.Mouse{X: 3, Y: 3, Button: tea.MouseNone}),
		tea.MouseReleaseMsg(tea.Mouse{X: 3, Y: 3, Button: tea.MouseLeft}),
	}
	for _, input := range inputs {
		updated, cmd := m.Update(input)
		got := updated.(Model)
		if cmd != nil || got.overlay != overlayHelp || got.editor.Value() != "draft" || got.viewport.YOffset() != 10 {
			t.Fatalf("input %T changed modal state: cmd=%v overlay=%v editor=%q offset=%d", input, cmd, got.overlay, got.editor.Value(), got.viewport.YOffset())
		}
		m = got
	}
}

func TestOverlayIsModalAndEscapeDismissesBeforeCancel(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		close(started)
		<-ctx.Done()
		<-release
		return ctx.Err()
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	turnDone := make(chan tea.Msg, 1)
	go func() { turnDone <- cmd() }()
	<-started

	running.editor.SetValue("/help")
	updated, _ = running.Update(keyPress(tea.KeyEnter))
	overlay := updated.(Model)
	if overlay.overlay != overlayHelp || overlay.editor.Value() != "" {
		t.Fatalf("overlay=%v editor=%q", overlay.overlay, overlay.editor.Value())
	}

	updated, _ = overlay.Update(keyPress('x'))
	if got := updated.(Model); got.overlay != overlayHelp || got.editor.Value() != "" {
		t.Fatalf("modal overlay changed state: overlay=%v editor=%q", got.overlay, got.editor.Value())
	}

	updated, cancelCmd := overlay.Update(keyPress(tea.KeyEscape))
	dismissed := updated.(Model)
	if cancelCmd != nil || dismissed.overlay != overlayNone || !dismissed.running {
		t.Fatalf("dismiss overlay=%v running=%v cmd=%v", dismissed.overlay, dismissed.running, cancelCmd)
	}

	updated, cancelCmd = dismissed.Update(keyPress(tea.KeyEscape))
	cancelled := updated.(Model)
	if cancelCmd != nil || !cancelled.running {
		t.Fatalf("cancel running=%v cmd=%v", cancelled.running, cancelCmd)
	}

	close(release)
	select {
	case done := <-turnDone:
		updated, _ = cancelled.Update(done)
		if updated.(Model).running {
			t.Fatal("turn still running after cancellation completion")
		}
	case <-time.After(time.Second):
		t.Fatal("canceled turn did not complete")
	}
}

func TestWhitespaceOnlyPromptIsRejectedAndOrdinaryWhitespaceIsPreserved(t *testing.T) {
	var prompted string
	backend := &fakeBackend{prompt: func(_ context.Context, text string, _ func(agent.Event)) error {
		prompted = text
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue(" \t\n ")
	whitespaceDraft := m.editor.Value()

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	rejected := updated.(Model)
	if cmd != nil || rejected.running || rejected.editor.Value() != whitespaceDraft || len(rejected.entries) != 0 {
		t.Fatalf("whitespace submit: cmd=%v running=%v editor=%q entries=%#v", cmd, rejected.running, rejected.editor.Value(), rejected.entries)
	}

	const prompt = "  keep this whitespace \n "
	rejected.editor.SetValue(prompt)
	updated, cmd = rejected.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	if cmd == nil || !running.running || len(running.entries) != 1 || running.entries[0].Raw != prompt {
		t.Fatalf("ordinary submit: cmd=%v running=%v entries=%#v", cmd, running.running, running.entries)
	}
	_ = runCommandWithin(t, cmd, time.Second)
	if prompted != prompt {
		t.Fatalf("backend prompt = %q, want %q", prompted, prompt)
	}
}

func TestNewCommandPendingBlocksPromptsAndDuplicateRequests(t *testing.T) {
	newSessionCalls := 0
	promptCalls := 0
	newSessionStarted := make(chan struct{})
	releaseNewSession := make(chan struct{})
	backend := &fakeBackend{
		newSession: func() error {
			newSessionCalls++
			close(newSessionStarted)
			<-releaseNewSession
			return nil
		},
		prompt: func(context.Context, string, func(agent.Event)) error {
			promptCalls++
			return nil
		},
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("/new")

	updated, firstCmd := m.Update(keyPress(tea.KeyEnter))
	pending := updated.(Model)
	if firstCmd == nil || !pending.newSessionPending || pending.newSessionGeneration != 1 {
		t.Fatalf("first /new: cmd=%v pending=%v generation=%d", firstCmd, pending.newSessionPending, pending.newSessionGeneration)
	}

	resultCh := make(chan tea.Msg, 1)
	go func() { resultCh <- firstCmd() }()
	select {
	case <-newSessionStarted:
	case <-time.After(time.Second):
		t.Fatal("first /new backend call did not start")
	}

	pending.editor.SetValue(" /new ")
	updated, duplicateCmd := pending.Update(keyPress(tea.KeyEnter))
	duplicate := updated.(Model)
	if duplicateCmd != nil || !duplicate.newSessionPending || newSessionCalls != 1 {
		t.Fatalf("duplicate /new: cmd=%v pending=%v calls=%d", duplicateCmd, duplicate.newSessionPending, newSessionCalls)
	}

	duplicate.editor.SetValue("  ordinary prompt  ")
	updated, promptCmd := duplicate.Update(keyPress(tea.KeyEnter))
	blocked := updated.(Model)
	if promptCmd != nil || blocked.running || blocked.editor.Value() != "  ordinary prompt  " || promptCalls != 0 {
		t.Fatalf("pending prompt: cmd=%v running=%v editor=%q calls=%d", promptCmd, blocked.running, blocked.editor.Value(), promptCalls)
	}

	close(releaseNewSession)
	var result tea.Msg
	select {
	case result = <-resultCh:
	case <-time.After(time.Second):
		t.Fatal("first /new backend call did not complete")
	}
	updated, _ = blocked.Update(result)
	if got := updated.(Model); got.newSessionPending || newSessionCalls != 1 {
		t.Fatalf("completed /new: pending=%v calls=%d", got.newSessionPending, newSessionCalls)
	}
}

func TestNewCommandResultCannotResetNewerActiveTurn(t *testing.T) {
	backend := &fakeBackend{
		history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "replacement history"}}}},
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("/new")
	updated, _ := m.Update(keyPress(tea.KeyEnter))
	pending := updated.(Model)
	generation := pending.newSessionGeneration

	updated, _ = pending.startPrompt("newer active prompt")
	active := updated.(Model)
	activeChannel := active.activeTurnChannel
	updated, cmd := active.Update(newSessionResultMsg{generation: generation})
	got := updated.(Model)
	if cmd != nil || !got.running || got.activeTurnChannel != activeChannel || got.newSessionPending || len(got.entries) != 2 || got.entries[len(got.entries)-1].Raw != "newer active prompt" {
		t.Fatalf("/new result reset active turn: cmd=%v running=%v pending=%v entries=%#v", cmd, got.running, got.newSessionPending, got.entries)
	}
}

func TestNewCommandIgnoresStaleResults(t *testing.T) {
	backend := &fakeBackend{
		history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "keep transcript"}}}},
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("/new")
	updated, firstCmd := m.Update(keyPress(tea.KeyEnter))
	first := updated.(Model)
	firstGeneration := first.newSessionGeneration

	updated, _ = first.Update(newSessionResultMsg{generation: firstGeneration + 1})
	stillPending := updated.(Model)
	if !stillPending.newSessionPending || stillPending.editor.Value() != "/new" || !strings.Contains(stillPending.View().Content, "keep transcript") {
		t.Fatalf("mismatched result changed state: pending=%v editor=%q view=%q", stillPending.newSessionPending, stillPending.editor.Value(), stillPending.View().Content)
	}

	updated, _ = stillPending.Update(newSessionResultMsg{generation: firstGeneration, err: errors.New("first failed")})
	failed := updated.(Model)
	if failed.newSessionPending {
		t.Fatal("matching failure left /new pending")
	}
	failed.editor.SetValue("/new")
	updated, secondCmd := failed.Update(keyPress(tea.KeyEnter))
	second := updated.(Model)
	if secondCmd == nil || second.newSessionGeneration <= firstGeneration {
		t.Fatalf("second /new: cmd=%v generation=%d, first=%d", secondCmd, second.newSessionGeneration, firstGeneration)
	}

	second.editor.SetValue("newer draft")
	updated, _ = second.Update(newSessionResultMsg{generation: firstGeneration})
	got := updated.(Model)
	if !got.newSessionPending || got.editor.Value() != "newer draft" || !strings.Contains(got.View().Content, "keep transcript") {
		t.Fatalf("stale success reset newer state: pending=%v editor=%q view=%q", got.newSessionPending, got.editor.Value(), got.View().Content)
	}

	_ = firstCmd
}

func TestExitStillQuitsWhileNewSessionIsPending(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.editor.SetValue("/new")
	updated, newCmd := m.Update(keyPress(tea.KeyEnter))
	pending := updated.(Model)
	if newCmd == nil || !pending.newSessionPending {
		t.Fatalf("/new cmd=%v pending=%v", newCmd, pending.newSessionPending)
	}

	pending.editor.SetValue("/exit")
	_, quitCmd := pending.Update(keyPress(tea.KeyEnter))
	if quitCmd == nil {
		t.Fatal("/exit did not quit while /new was pending")
	}
	if msg := runCommandWithin(t, quitCmd, time.Second); msg == nil {
		t.Fatal("/exit quit message = nil")
	}
}

func TestNewCommandSuccessReplacesHistoryAndUsage(t *testing.T) {
	backend := &fakeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session-old"},
		history: []model.Message{{
			Role:   model.RoleAssistant,
			Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}},
			Usage:  &model.Usage{InputTokens: 1, OutputTokens: 2},
		}},
	}
	backend.newSession = func() error {
		backend.info.SessionID = "session-new"
		backend.history = []model.Message{{
			Role:   model.RoleAssistant,
			Blocks: []model.Block{{Type: model.BlockText, Text: "fresh transcript"}},
			Usage:  &model.Usage{InputTokens: 7, OutputTokens: 9},
		}}
		return nil
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("  /new  ")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	waiting := updated.(Model)
	if cmd == nil || waiting.editor.Value() != "  /new  " || !strings.Contains(waiting.View().Content, "old transcript") {
		t.Fatalf("waiting editor=%q cmd=%v view=%q", waiting.editor.Value(), cmd, waiting.View().Content)
	}

	updated, next := waiting.Update(cmd())
	got := updated.(Model)
	if next != nil {
		t.Fatalf("new session result scheduled unexpected cmd %v", next)
	}
	if got.editor.Value() != "" || got.usage.InputTokens != 7 || got.usage.OutputTokens != 9 {
		t.Fatalf("editor=%q usage=%#v", got.editor.Value(), got.usage)
	}
	if content := got.View().Content; strings.Contains(content, "old transcript") || !strings.Contains(content, "fresh transcript") || !strings.Contains(content, "session-new") {
		t.Fatalf("view = %q", content)
	}
}

func TestNewCommandFailureRetainsDraftAndState(t *testing.T) {
	backend := &fakeBackend{
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
		history: []model.Message{{
			Role:   model.RoleAssistant,
			Blocks: []model.Block{{Type: model.BlockText, Text: "keep me"}},
			Usage:  &model.Usage{InputTokens: 3, OutputTokens: 4},
		}},
		newSession: func() error { return errors.New("replacement failed") },
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue(" /new ")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	waiting := updated.(Model)
	if cmd == nil || waiting.editor.Value() != " /new " {
		t.Fatalf("waiting editor=%q cmd=%v", waiting.editor.Value(), cmd)
	}

	updated, next := waiting.Update(cmd())
	got := updated.(Model)
	if next != nil {
		t.Fatalf("failure scheduled unexpected cmd %v", next)
	}
	if got.editor.Value() != " /new " || got.usage.InputTokens != 3 || got.usage.OutputTokens != 4 {
		t.Fatalf("editor=%q usage=%#v", got.editor.Value(), got.usage)
	}
	if content := got.View().Content; !strings.Contains(content, "keep me") || !strings.Contains(content, "replacement failed") {
		t.Fatalf("view = %q", content)
	}
}

func TestNewCommandIsRejectedWhileTurnIsActive(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	newSessionCalls := 0
	backend := &fakeBackend{
		prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
			close(started)
			<-release
			return nil
		},
		newSession: func() error {
			newSessionCalls++
			return nil
		},
		info: app.Info{Profile: "profile", Model: "model", SessionID: "session"},
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	turnDone := make(chan tea.Msg, 1)
	go func() { turnDone <- cmd() }()
	<-started

	running.editor.SetValue("/new")
	updated, rejectCmd := running.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if rejectCmd != nil || got.editor.Value() != "/new" || !strings.Contains(got.View().Content, app.ErrPromptActive.Error()) {
		t.Fatalf("editor=%q cmd=%v view=%q", got.editor.Value(), rejectCmd, got.View().Content)
	}
	if newSessionCalls != 0 {
		t.Fatalf("new session called %d times while turn active", newSessionCalls)
	}

	close(release)
	select {
	case <-turnDone:
	case <-time.After(time.Second):
		t.Fatal("turn command did not complete")
	}
}

func TestExitCommandOnlyQuitsWhenIdle(t *testing.T) {
	idle := resizeModel(t, newTestModel(t), 80, 12)
	idle.editor.SetValue(" /exit ")
	updated, quitCmd := idle.Update(keyPress(tea.KeyEnter))
	if quitCmd == nil {
		t.Fatal("idle /exit did not return quit command")
	}
	if msg := quitCmd(); msg == nil {
		t.Fatal("idle /exit quit message = nil")
	}
	if updated.(Model).editor.Value() != " /exit " {
		t.Fatalf("idle editor = %q", updated.(Model).editor.Value())
	}

	started := make(chan struct{})
	release := make(chan struct{})
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		close(started)
		<-release
		return nil
	}, info: app.Info{Profile: "profile", Model: "model", SessionID: "session"}}
	runningModel := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	runningModel.editor.SetValue("question")
	updated, cmd := runningModel.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	turnDone := make(chan tea.Msg, 1)
	go func() { turnDone <- cmd() }()
	<-started

	running.editor.SetValue("/exit")
	updated, quitCmd = running.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if quitCmd != nil || got.editor.Value() != "/exit" || !got.running {
		t.Fatalf("running=%v editor=%q cmd=%v", got.running, got.editor.Value(), quitCmd)
	}

	close(release)
	select {
	case <-turnDone:
	case <-time.After(time.Second):
		t.Fatal("turn command did not complete")
	}
}

func TestCtrlCArmsAtZeroTime(t *testing.T) {
	clock := newFakeClock(time.Time{})
	m := resizeModel(t, NewModel(context.Background(), &fakeBackend{}, WithClock(clock), WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	}))), 80, 12)

	updated, armCmd := m.Update(keyPress('c', tea.ModCtrl))
	armed := updated.(Model)
	if armCmd == nil || !armed.ctrlCArmed || !armed.ctrlCArmedAt.IsZero() || armed.ctrlCArmGeneration != 1 {
		t.Fatalf("armed=%v armedAt=%v generation=%d cmd=%v", armed.ctrlCArmed, armed.ctrlCArmedAt, armed.ctrlCArmGeneration, armCmd)
	}
	_, quitCmd := armed.Update(keyPress('c', tea.ModCtrl))
	if quitCmd == nil {
		t.Fatal("second Ctrl+C at zero clock did not quit")
	}
}

func TestCtrlCExpiryUsesGenerationWhenTwoArmsShareClock(t *testing.T) {
	clock := newFakeClock(time.Time{})
	m := resizeModel(t, NewModel(context.Background(), &fakeBackend{}, WithClock(clock), WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	}))), 80, 12)

	updated, _ := m.Update(keyPress('c', tea.ModCtrl))
	first := updated.(Model)
	firstGeneration := first.ctrlCArmGeneration
	updated, _ = first.Update(ctrlCArmExpiredMsg{generation: firstGeneration})
	cleared := updated.(Model)
	if cleared.ctrlCArmed {
		t.Fatal("matching expiry did not clear first arm")
	}

	updated, _ = cleared.Update(keyPress('c', tea.ModCtrl))
	second := updated.(Model)
	if !second.ctrlCArmed || second.ctrlCArmGeneration <= firstGeneration || second.ctrlCArmedAt != first.ctrlCArmedAt {
		t.Fatalf("second arm: armed=%v generation=%d armedAt=%v", second.ctrlCArmed, second.ctrlCArmGeneration, second.ctrlCArmedAt)
	}
	updated, _ = second.Update(ctrlCArmExpiredMsg{generation: firstGeneration})
	if !updated.(Model).ctrlCArmed {
		t.Fatal("stale first expiry cleared second arm at same clock time")
	}
}

func TestCtrlCSecondPressWindowHasStrictDeadline(t *testing.T) {
	tests := []struct {
		name     string
		advance  time.Duration
		wantQuit bool
	}{
		{name: "just below", advance: time.Second - time.Nanosecond, wantQuit: true},
		{name: "exact", advance: time.Second, wantQuit: false},
		{name: "above", advance: time.Second + time.Nanosecond, wantQuit: false},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			clock := newFakeClock(time.Unix(500, 0))
			m := resizeModel(t, NewModel(context.Background(), &fakeBackend{}, WithClock(clock), WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
				return text, nil
			}))), 80, 12)
			updated, _ := m.Update(keyPress('c', tea.ModCtrl))
			first := updated.(Model)
			clock.Advance(tt.advance)

			updated, cmd := first.Update(keyPress('c', tea.ModCtrl))
			got := updated.(Model)
			if tt.wantQuit {
				if cmd == nil || runCommandWithin(t, cmd, time.Second) == nil {
					t.Fatal("second Ctrl+C did not quit strictly before deadline")
				}
				return
			}
			if cmd == nil || !got.ctrlCArmed || got.ctrlCArmGeneration != first.ctrlCArmGeneration+1 {
				t.Fatalf("deadline press did not rearm: cmd=%v armed=%v generation=%d first=%d", cmd, got.ctrlCArmed, got.ctrlCArmGeneration, first.ctrlCArmGeneration)
			}
		})
	}
}

func TestCtrlCRunningCancelsArmsAndSecondQuits(t *testing.T) {
	clock := newFakeClock(time.Unix(100, 0))
	started := make(chan struct{})
	canceled := make(chan struct{})
	release := make(chan struct{})
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		close(started)
		<-ctx.Done()
		close(canceled)
		<-release
		return ctx.Err()
	}}
	m := resizeModel(t, NewModel(context.Background(), backend, WithClock(clock), WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	}))), 80, 12)
	m.editor.SetValue("question")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	turnDone := make(chan tea.Msg, 1)
	go func() { turnDone <- cmd() }()
	<-started

	updated, armCmd := running.Update(keyPress('c', tea.ModCtrl))
	armed := updated.(Model)
	if armCmd == nil || !armed.running || armed.ctrlCArmedAt != clock.Now() {
		t.Fatalf("running=%v armedAt=%v cmd=%v", armed.running, armed.ctrlCArmedAt, armCmd)
	}
	select {
	case <-canceled:
	case <-time.After(time.Second):
		t.Fatal("ctrl+c did not cancel active turn")
	}

	_, quitCmd := armed.Update(keyPress('c', tea.ModCtrl))
	if quitCmd == nil {
		t.Fatal("second ctrl+c did not quit")
	}
	if msg := quitCmd(); msg == nil {
		t.Fatal("second ctrl+c quit message = nil")
	}

	close(release)
	select {
	case <-turnDone:
	case <-time.After(time.Second):
		t.Fatal("canceled turn did not complete")
	}
}

func TestCtrlCIdleClearsEditorArmsAndSecondQuits(t *testing.T) {
	clock := newFakeClock(time.Unix(200, 0))
	m := resizeModel(t, NewModel(context.Background(), &fakeBackend{info: app.Info{Profile: "profile", Model: "model", SessionID: "session"}}, WithClock(clock), WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	}))), 80, 12)
	m.editor.SetValue("line one\nline two")

	updated, armCmd := m.Update(keyPress('c', tea.ModCtrl))
	armed := updated.(Model)
	if armCmd == nil || armed.editor.Value() != "" || armed.ctrlCArmedAt != clock.Now() {
		t.Fatalf("editor=%q armedAt=%v cmd=%v", armed.editor.Value(), armed.ctrlCArmedAt, armCmd)
	}
	if !strings.Contains(armed.View().Content, "press Ctrl+C again to exit") {
		t.Fatalf("view = %q", armed.View().Content)
	}

	_, quitCmd := armed.Update(keyPress('c', tea.ModCtrl))
	if quitCmd == nil {
		t.Fatal("second ctrl+c did not quit")
	}
	if msg := quitCmd(); msg == nil {
		t.Fatal("second ctrl+c quit message = nil")
	}
}

func TestCtrlCArmExpiresAndStaleTickIsIgnored(t *testing.T) {
	clock := newFakeClock(time.Unix(300, 0))
	m := resizeModel(t, NewModel(context.Background(), &fakeBackend{info: app.Info{Profile: "profile", Model: "model", SessionID: "session"}}, WithClock(clock), WithRenderer(rendererFunc(func(text string, _ int) (string, error) {
		return text, nil
	}))), 80, 12)

	updated, armCmd := m.Update(keyPress('c', tea.ModCtrl))
	armed := updated.(Model)
	if armCmd == nil || armed.ctrlCArmedAt.IsZero() {
		t.Fatalf("armedAt=%v cmd=%v", armed.ctrlCArmedAt, armCmd)
	}

	updated, _ = armed.Update(ctrlCArmExpiredMsg{generation: armed.ctrlCArmGeneration - 1})
	if !updated.(Model).ctrlCArmed {
		t.Fatal("stale ctrl+c tick cleared armed state")
	}

	expired := make(chan tea.Msg, 1)
	go func() { expired <- armCmd() }()
	clock.WaitForTimers(t, 1, time.Second)
	clock.Advance(time.Second)
	select {
	case msg := <-expired:
		updated, _ = armed.Update(msg)
		cleared := updated.(Model)
		if cleared.ctrlCArmed {
			t.Fatalf("armed=%v armedAt=%v, want cleared", cleared.ctrlCArmed, cleared.ctrlCArmedAt)
		}
		if strings.Contains(cleared.View().Content, "press Ctrl+C again to exit") {
			t.Fatalf("view = %q", cleared.View().Content)
		}
		updated, rearmCmd := cleared.Update(keyPress('c', tea.ModCtrl))
		if rearmCmd == nil || !updated.(Model).ctrlCArmed {
			t.Fatalf("rearm cmd=%v armed=%v armedAt=%v", rearmCmd, updated.(Model).ctrlCArmed, updated.(Model).ctrlCArmedAt)
		}
	case <-time.After(time.Second):
		t.Fatal("ctrl+c expiry command did not complete")
	}
}

func TestStaleTurnMessagesCannotMutateActiveOrCompletedTurn(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.editor.SetValue("active")
	updated, _ := m.Update(keyPress(tea.KeyEnter))
	active := updated.(Model)

	staleStream := newTurnStream()
	staleStream.channel <- turnEnvelope{done: true}
	close(staleStream.channel)
	staleEvent := agent.Event{Type: agent.EventTextDelta, Text: "stale text"}
	updated, drainCmd := active.Update(turnMsg{
		channel: staleStream.channel,
		stream:  staleStream,
		value:   turnEnvelope{event: &staleEvent},
	})
	unchanged := updated.(Model)
	if drainCmd == nil || len(unchanged.entries) != 1 || unchanged.entries[0].Raw != "active" || !unchanged.running {
		t.Fatalf("stale active envelope mutated state: cmd=%v running=%v entries=%#v", drainCmd, unchanged.running, unchanged.entries)
	}
	staleDone := runCommandWithin(t, drainCmd, time.Second)
	updated, next := unchanged.Update(staleDone)
	if next != nil || len(updated.(Model).entries) != 1 || !updated.(Model).running {
		t.Fatalf("stale drain completion changed active turn: cmd=%v running=%v entries=%#v", next, updated.(Model).running, updated.(Model).entries)
	}

	completedModel := resizeModel(t, newTestModel(t), 80, 12)
	completedModel.editor.SetValue("completed")
	started, startCmd := completedModel.Update(keyPress(tea.KeyEnter))
	doneMsg := runCommandWithin(t, startCmd, time.Second).(turnMsg)
	finished, _ := started.(Model).Update(doneMsg)
	idle := finished.(Model)
	updated, drainCmd = idle.Update(turnMsg{channel: doneMsg.channel, stream: doneMsg.stream, value: turnEnvelope{event: &staleEvent}})
	if drainCmd == nil || updated.(Model).running || len(updated.(Model).entries) != 1 {
		t.Fatalf("completed stale envelope mutated state: cmd=%v running=%v entries=%#v", drainCmd, updated.(Model).running, updated.(Model).entries)
	}
	closedDone := runCommandWithin(t, drainCmd, time.Second)
	updated, next = updated.(Model).Update(closedDone)
	if next != nil || updated.(Model).running || len(updated.(Model).entries) != 1 {
		t.Fatalf("closed stale drain changed completed state: cmd=%v running=%v entries=%#v", next, updated.(Model).running, updated.(Model).entries)
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
	if cap(firstTurn.stream.regularEventSlots) != 63 {
		t.Fatalf("ordinary turn permits = %d, want 63", cap(firstTurn.stream.regularEventSlots))
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

func TestTurnChannelClosesAfterDone(t *testing.T) {
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error { return nil }}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	first := runCommandWithin(t, cmd, time.Second).(turnMsg)
	afterDone, _ := updated.(Model).Update(first)
	if got := afterDone.(Model); got.running || got.cancel != nil {
		t.Fatalf("post-done running=%v cancel=%v", got.running, got.cancel != nil)
	}

	select {
	case _, ok := <-first.channel:
		if ok {
			t.Fatal("turn channel still open after done")
		}
	case <-time.After(time.Second):
		t.Fatal("turn channel did not close after done")
	}
}

func TestWaitTurnClosedBeforeDoneReturnsError(t *testing.T) {
	stream := newTurnStream()
	close(stream.channel)

	msg := runCommandWithin(t, waitTurn(stream), time.Second).(turnMsg)
	if !msg.value.done {
		t.Fatal("closed channel did not report completion")
	}
	if msg.value.err == nil {
		t.Fatal("closed channel returned nil error")
	}
}

func TestToolEventsUpdateTranscript(t *testing.T) {
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventToolCallStarted, ToolName: "read", ToolCallID: "call-1", ToolArgs: `{"path":"README.md"}`})
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
	if started.entries[1].ToolArgs != `{"path":"README.md"}` {
		t.Fatalf("started tool args = %q, want live args carried from event", started.entries[1].ToolArgs)
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

	content := idle.View().Content
	if idle.expandedTools || !strings.Contains(content, "read") || !strings.Contains(content, "README.md") || strings.Contains(content, "full output") {
		t.Fatalf("default tool summary expanded=%v content=%q, want concise argument preview", idle.expandedTools, content)
	}

	expanded, _ := idle.Update(toggleToolsMsg{})
	if content := expanded.View().Content; !strings.Contains(content, "full output") {
		t.Fatalf("expanded tool content = %q, want full output", content)
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
	for len(first.channel) < turnChannelCapacity-2 && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if len(first.channel) < turnChannelCapacity-2 {
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
	case extra, ok := <-first.channel:
		if ok {
			t.Fatalf("unexpected envelope after completion: %#v", extra)
		}
	case <-time.After(time.Second):
		t.Fatal("turn channel did not close after completion")
	}
	select {
	case <-backendFinished:
	case <-time.After(time.Second):
		t.Fatal("backend worker did not exit")
	}
}

func TestCanceledFullTurnChannelReconcilesPersistedToolResultsAfterBaseline(t *testing.T) {
	preexisting := []model.Message{
		{ID: "historical-call", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolName: "historical", ToolCallID: "call-1"}}},
		{ID: "historical-result", Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, ToolName: "historical", ToolCallID: "call-1", Text: "old result", IsError: true}}},
	}
	backendFinished := make(chan struct{})
	backend := &fakeBackend{history: preexisting}
	backend.prompt = func(ctx context.Context, text string, emit func(agent.Event)) error {
		defer close(backendFinished)
		emit(agent.Event{Type: agent.EventToolCallStarted, ToolName: "wrong callback name", ToolCallID: "call-1"})
		for i := 0; i < turnChannelCapacity+16; i++ {
			emit(agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 1}})
		}
		<-ctx.Done()
		for _, event := range []agent.Event{
			{Type: agent.EventToolCallFinished, ToolName: "wrong callback name", ToolCallID: "call-1", ToolResult: tool.Result{Content: "wrong callback output"}},
			{Type: agent.EventToolCallStarted, ToolName: "write", ToolCallID: "call-2"},
			{Type: agent.EventToolCallFinished, ToolName: "write", ToolCallID: "call-2", ToolResult: tool.Result{Content: "wrong second callback output"}},
			{Type: agent.EventToolCallStarted, ToolName: "edit", ToolCallID: "call-3"},
			{Type: agent.EventToolCallFinished, ToolName: "edit", ToolCallID: "call-3", ToolResult: tool.Result{Content: "wrong third callback output"}},
			{Type: agent.EventToolCallFinished, ToolName: "malformed", ToolCallID: "call-malformed", ToolResult: tool.Result{Content: "not persisted", IsError: true}},
		} {
			emit(event)
		}
		backend.history = append(backend.history,
			model.Message{ID: "turn-results-1", Role: model.RoleTool, Blocks: []model.Block{
				{Type: model.BlockToolResult, ToolName: "bash", ToolCallID: "call-1", Text: "first exact persisted result", IsError: true},
				{Type: model.BlockToolResult, ToolName: "write", ToolCallID: "call-2", Text: "second exact persisted result"},
			}},
			model.Message{ID: "turn-results-2", Role: model.RoleTool, Blocks: []model.Block{
				{Type: model.BlockToolResult, ToolName: "edit", ToolCallID: "call-3", Text: "third exact persisted result", IsError: true},
			}},
		)
		return ctx.Err()
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")

	updated, start := m.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	first := runCommandWithin(t, start, time.Second).(turnMsg)
	deadline := time.Now().Add(time.Second)
	for len(first.channel) < turnChannelCapacity-1 && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if len(first.channel) < turnChannelCapacity-1 {
		running.cancel()
		t.Fatalf("turn channel only filled to %d, want 63 ordinary envelopes", len(first.channel))
	}

	updated, _ = running.Update(keyPress(tea.KeyEscape))
	state := updated.(Model)
	select {
	case <-backendFinished:
	case <-time.After(time.Second):
		t.Fatal("canceled callbacks blocked while the UI was not consuming")
	}
	deadline = time.Now().Add(time.Second)
	for len(first.channel) < turnChannelCapacity && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if got := len(first.channel); got != turnChannelCapacity {
		t.Fatalf("full channel with reserved done = %d, want capacity %d", got, turnChannelCapacity)
	}

	doneCount := 0
	msg := tea.Msg(first)
	for state.running {
		if turn, ok := msg.(turnMsg); ok && turn.value.done {
			doneCount++
		}
		updated, next := state.Update(msg)
		state = updated.(Model)
		if !state.running {
			break
		}
		if next == nil {
			t.Fatal("turn stopped scheduling channel reads before completion")
		}
		msg = runCommandWithin(t, next, time.Second)
	}
	if doneCount != 1 {
		t.Fatalf("done envelopes applied = %d, want 1", doneCount)
	}

	var toolEntries []Entry
	for _, entry := range state.entries {
		if entry.Kind == EntryTool {
			toolEntries = append(toolEntries, entry)
		}
	}
	want := []struct {
		id, name, output string
		error            bool
	}{
		{id: "call-1", name: "historical", output: "old result", error: true},
		{id: "call-1", name: "bash", output: "first exact persisted result", error: true},
		{id: "call-2", name: "write", output: "second exact persisted result"},
		{id: "call-3", name: "edit", output: "third exact persisted result", error: true},
	}
	if len(toolEntries) != len(want) {
		t.Fatalf("tool entries = %#v, want historical plus %d persisted results", toolEntries, len(want)-1)
	}
	for index, expected := range want {
		entry := toolEntries[index]
		if entry.ToolCallID != expected.id || entry.ToolName != expected.name || entry.ToolOutput != expected.output || !entry.ToolDone || entry.ToolError != expected.error {
			t.Fatalf("tool entry %d = %#v, want %#v", index, entry, expected)
		}
	}
	select {
	case _, ok := <-first.channel:
		if ok {
			t.Fatal("turn channel contains an envelope after done")
		}
	case <-time.After(time.Second):
		t.Fatal("turn channel was not closed exactly once after done")
	}
}

func TestPersistedToolReconciliationUpdatesDoneEntriesInStoredOrder(t *testing.T) {
	backend := &fakeBackend{history: []model.Message{{
		ID: "before-turn", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "old prompt"}},
	}}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.turnHistoryBaseline = captureTurnHistoryBaseline(backend.History())
	m.turnEntryStart = len(m.entries)
	m.entries = append(m.entries,
		Entry{ID: "later-callback", Kind: EntryTool, ToolCallID: "call-2", ToolName: "wrong-2", ToolOutput: "callback-2", ToolDone: true},
		Entry{ID: "earlier-callback", Kind: EntryTool, ToolCallID: "call-1", ToolName: "wrong-1", ToolOutput: "callback-1", ToolDone: true},
	)
	backend.history = append(backend.history, model.Message{
		ID: "turn-results", Role: model.RoleTool, Blocks: []model.Block{
			{Type: model.BlockToolResult, ToolCallID: "call-1", ToolName: "bash", Text: "stored first", IsError: true},
			{Type: model.BlockToolResult, ToolCallID: "call-2", ToolName: "write", Text: "stored second"},
		},
	})

	if !m.reconcilePersistedToolResults() {
		t.Fatal("reconcilePersistedToolResults() reported no change")
	}
	got := m.entries[m.turnEntryStart:]
	if len(got) != 2 || got[0].ToolCallID != "call-1" || got[0].ToolName != "bash" || got[0].ToolOutput != "stored first" || !got[0].ToolError || !got[0].ToolDone {
		t.Fatalf("first reconciled entry = %#v", got)
	}
	if got[1].ToolCallID != "call-2" || got[1].ToolName != "write" || got[1].ToolOutput != "stored second" || got[1].ToolError || !got[1].ToolDone {
		t.Fatalf("second reconciled entry = %#v", got)
	}
}

func TestLateMalformedFinishCallbackCannotSendOnClosedTurnStream(t *testing.T) {
	releaseLateCallback := make(chan struct{})
	callbackPanic := make(chan any, 1)
	backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
		go func() {
			<-releaseLateCallback
			func() {
				defer func() { callbackPanic <- recover() }()
				emit(agent.Event{
					Type:       agent.EventToolCallFinished,
					ToolName:   "malformed",
					ToolCallID: "late-call",
					ToolResult: tool.Result{Content: "too late", IsError: true},
				})
			}()
		}()
		return nil
	}}
	stream := newTurnStream()
	runTurnWorker(context.Background(), backend, "question", stream)

	close(releaseLateCallback)
	select {
	case panicValue := <-callbackPanic:
		if panicValue != nil {
			t.Fatalf("late malformed callback panicked after stream close: %v", panicValue)
		}
	case <-time.After(time.Second):
		t.Fatal("late malformed callback leaked")
	}

	envelope, ok := <-stream.channel
	if !ok || !envelope.done {
		t.Fatalf("completion envelope = %#v, open=%v, want done before close", envelope, ok)
	}
	if _, ok := <-stream.channel; ok {
		t.Fatal("turn stream remained open after its one completion envelope")
	}
}

func TestFinishTurnReconcilesPendingToolEntries(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.running = true
	m.cancel = func() {}
	m.entries = []Entry{{Kind: EntryTool, ToolCallID: "call-1", ToolName: "bash"}}

	updated, _ := m.finishTurn(context.Canceled)
	got := updated.(Model)
	if !got.entries[0].ToolDone || !got.entries[0].ToolError || !strings.Contains(got.entries[0].ToolOutput, context.Canceled.Error()) {
		t.Fatalf("pending tool after finish = %#v", got.entries[0])
	}
}

func TestFatalPersistenceWithoutStoredToolResultUsesGenericReconciliation(t *testing.T) {
	fatalErr := errors.Join(session.ErrFatalPersistence, errors.New("persist tool result: disk full"))
	backend := &fakeBackend{history: []model.Message{{ID: "assistant", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "bash"}}}}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.running = true
	m.cancel = func() {}
	m.entries = append(m.entries, Entry{Kind: EntryTool, ToolCallID: "call-1", ToolName: "bash"})

	updated, cmd := m.finishTurn(fatalErr)
	got := updated.(Model)
	entry := got.entries[len(got.entries)-2]
	if cmd == nil || !errors.Is(got.fatalErr, session.ErrFatalPersistence) {
		t.Fatalf("finishTurn() cmd=%v fatalErr=%v, want fatal quit", cmd, got.fatalErr)
	}
	if !entry.ToolDone || !entry.ToolError || !strings.Contains(entry.ToolOutput, "disk full") {
		t.Fatalf("pending tool after failed persistence = %#v", entry)
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
