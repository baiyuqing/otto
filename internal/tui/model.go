package tui

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"sort"
	"strings"
	"sync"
	"time"

	"charm.land/bubbles/v2/key"
	"charm.land/bubbles/v2/spinner"
	"charm.land/bubbles/v2/textarea"
	"charm.land/bubbles/v2/viewport"
	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	otmodel "github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

const (
	turnChannelCapacity  = 64
	streamRenderInterval = 50 * time.Millisecond
	ctrlCArmWindow       = time.Second
	liveEntryIDPrefix    = "live"
	ctrlCExitStatus      = "press Ctrl+C again to exit"
)

type Clock interface {
	Now() time.Time
	After(time.Duration) <-chan time.Time
}

type realClock struct{}

func (realClock) Now() time.Time {
	return time.Now()
}

func (realClock) After(d time.Duration) <-chan time.Time {
	return time.After(d)
}

type Option func(*Model)

func WithRenderer(renderer MarkdownRenderer) Option {
	return func(model *Model) {
		model.renderer = renderer
		model.rendererInjected = true
	}
}

func WithClock(clock Clock) Option {
	return func(model *Model) {
		if clock != nil {
			model.clock = clock
		}
	}
}

type turnHistoryBaseline struct {
	messageCount int
	digest       [sha256.Size]byte
	valid        bool
}

type Model struct {
	rootCtx                context.Context
	backend                app.Backend
	entries                []Entry
	viewport               viewport.Model
	editor                 textarea.Model
	spinner                spinner.Model
	keymap                 KeyMap
	width                  int
	height                 int
	usage                  otmodel.Usage
	running                bool
	expandedTools          bool
	overlay                overlayKind
	autoFollow             bool
	renderer               MarkdownRenderer
	rendererInjected       bool
	darkBackground         bool
	clock                  Clock
	statusText             string
	supportsModifiedEnter  bool
	dirtyStreaming         bool
	renderTickActive       bool
	cancel                 context.CancelFunc
	ctrlCArmed             bool
	ctrlCArmedAt           time.Time
	ctrlCArmGeneration     uint64
	newSessionPending      bool
	newSessionGeneration   uint64
	resume                 resumePickerState
	activeTurnChannel      <-chan turnEnvelope
	activeAssistant        int
	turnErrorSeen          bool
	turnEventErr           error
	fatalErr               error
	turnGeneration         uint64
	turnHistoryBaseline    turnHistoryBaseline
	turnEntryStart         int
	liveEntrySequence      int
	commandSuggestionIndex int
}

func NewModel(ctx context.Context, backend app.Backend, options ...Option) Model {
	entries, usage := EntriesFromHistory(historyFromBackend(backend))
	editor := textarea.New()
	editor.ShowLineNumbers = false
	editor.Prompt = "> "
	editor.Placeholder = "Ask Otto"
	editor.MinHeight = minEditorHeight
	editor.MaxHeight = maxEditorHeight
	editor.DynamicHeight = true
	editor.SetHeight(minEditorHeight)
	editor.SetWidth(0)
	editor.KeyMap.InsertNewline = key.NewBinding(key.WithKeys("alt+enter"), key.WithHelp("alt+enter", "insert newline"))
	_ = editor.Focus()

	vp := viewport.New()
	vp.MouseWheelEnabled = true
	vp.SoftWrap = true

	model := Model{
		rootCtx:         ctx,
		backend:         backend,
		entries:         entries,
		viewport:        vp,
		editor:          editor,
		spinner:         spinner.New(spinner.WithSpinner(spinner.MiniDot)),
		keymap:          DefaultKeyMap(),
		usage:           usage,
		expandedTools:   true,
		autoFollow:      true,
		darkBackground:  true,
		renderer:        newGlamourRenderer(true),
		clock:           realClock{},
		activeAssistant: -1,
	}
	for _, option := range options {
		option(&model)
	}
	model.rerenderAndRefreshViewportContent(false)
	return model
}

func (m Model) Init() tea.Cmd {
	return tea.Batch(tea.RequestBackgroundColor, m.spinner.Tick)
}

func (m Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	previousYOffset := m.viewport.YOffset()
	previousEditorHeight := m.editor.Height()
	viewportBefore := m.viewport

	switch msg := msg.(type) {
	case tea.BackgroundColorMsg:
		m.darkBackground = msg.IsDark()
		if !m.rendererInjected {
			m.renderer = newGlamourRenderer(m.darkBackground)
			m.invalidateAssistantRenders()
			m.rerenderAndRefreshViewportContent(!m.autoFollow)
		}
		return m, nil
	case spinner.TickMsg:
		var cmd tea.Cmd
		m.spinner, cmd = m.spinner.Update(msg)
		return m, cmd
	case tea.WindowSizeMsg:
		m.width = max(0, msg.Width)
		m.height = max(0, msg.Height)
		m.rerenderAndRefreshViewportContent(!m.autoFollow)
		return m, nil
	case tea.KeyboardEnhancementsMsg:
		m.supportsModifiedEnter = msg.SupportsKeyDisambiguation()
		return m, nil
	case showHelpOverlayMsg:
		m.overlay = overlayHelp
		m.statusText = ""
		return m, nil
	case showSessionOverlayMsg:
		m.overlay = overlaySession
		m.statusText = ""
		return m, nil
	case hideOverlayMsg:
		m.overlay = overlayNone
		return m, nil
	case toggleToolsMsg:
		m.expandedTools = !m.expandedTools
		m.refreshViewportContent(!m.autoFollow)
		return m, nil
	case scrollViewportMsg:
		m.scrollViewport(msg.Delta)
		return m, nil
	case newSessionResultMsg:
		return m.applyNewSessionResult(msg)
	case sessionListResultMsg:
		return m.applySessionListResult(msg)
	case sessionResumeResultMsg:
		return m.applySessionResumeResult(msg)
	case ctrlCArmExpiredMsg:
		if m.ctrlCArmed && msg.generation == m.ctrlCArmGeneration {
			m.clearCtrlCArm()
		}
		return m, nil
	case turnMsg:
		return m.updateTurn(msg)
	case renderStreamingMsg:
		if msg.generation != m.turnGeneration || !m.running {
			return m, nil
		}
		m.renderTickActive = false
		if m.dirtyStreaming && m.renderActiveAssistantEntry() {
			m.dirtyStreaming = false
			m.refreshViewportContent(!m.autoFollow)
		}
		return m, nil
	case tea.KeyPressMsg:
		return m.handleKeyPress(msg, previousYOffset, previousEditorHeight, viewportBefore)
	case tea.PasteMsg:
		if m.overlay != overlayNone {
			return m, nil
		}
	case tea.MouseMsg:
		if m.overlay != overlayNone {
			return m, nil
		}
	}

	return m.updateComponents(msg, previousYOffset, previousEditorHeight, viewportBefore)
}

func (m Model) updateComponents(msg tea.Msg, previousYOffset, previousEditorHeight int, viewportBefore viewport.Model) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd
	previousEditorValue := m.editor.Value()
	previousSuggestionCount := len(m.commandSuggestions())
	m.viewport, _ = m.viewport.Update(msg)
	m.editor, cmd = m.editor.Update(msg)
	if m.editor.Value() != previousEditorValue {
		m.commandSuggestionIndex = 0
	}
	m.spinner, _ = m.spinner.Update(msg)
	m.syncAutoFollow(viewportBefore)

	if m.editor.Height() != previousEditorHeight || len(m.commandSuggestions()) != previousSuggestionCount {
		m.rerenderAndRefreshViewportContent(!m.autoFollow)
	} else if m.viewport.YOffset() != previousYOffset && !m.viewport.AtBottom() {
		m.autoFollow = false
	} else if m.viewport.AtBottom() {
		m.autoFollow = true
	}

	return m, cmd
}

func (m Model) View() tea.View {
	_ = m.reservedStateActive()

	suggestions := m.commandSuggestions()
	layout := calculateLayout(m.width, m.height, m.editor, len(suggestions))
	if layout.tooSmall {
		return newRootView(m, smallTerminalView(m.width, m.height))
	}

	transcript := lipgloss.NewStyle().Width(layout.transcriptWidth).Height(layout.transcriptHeight).MaxHeight(layout.transcriptHeight).Render(m.viewport.View())
	editor := lipgloss.NewStyle().Width(m.width).Height(layout.editorHeight).Render(m.editor.View())
	footer := lipgloss.NewStyle().Width(m.width).Render(renderFooter(m.width, infoFromBackend(m.backend), m.usage, m.footerStatus()))
	parts := []string{transcript}
	if layout.suggestionHeight > 0 {
		parts = append(parts, renderCommandSuggestions(m.width, suggestions, m.commandSuggestionIndex, layout.suggestionHeight))
	}
	parts = append(parts, editor, footer)
	content := lipgloss.JoinVertical(lipgloss.Left, parts...)
	if m.overlay != overlayNone {
		content = renderOverlay(m.width, m.height, m.overlayContent())
	}
	return newRootView(m, content)
}

func (m Model) footerStatus() string {
	if !m.running {
		return m.statusText
	}
	running := m.spinner.View() + " working"
	if m.statusText == "" {
		return running
	}
	return running + " · " + m.statusText
}

func (m Model) overlayContent() string {
	switch m.overlay {
	case overlayHelp:
		return helpOverlayContent(m.width, m.height)
	case overlaySession:
		return sessionOverlayContent(infoFromBackend(m.backend))
	default:
		return ""
	}
}

func (m Model) handleKeyPress(msg tea.KeyPressMsg, previousYOffset, previousEditorHeight int, viewportBefore viewport.Model) (tea.Model, tea.Cmd) {
	if m.overlay != overlayNone {
		if isEscapeKey(msg) {
			m.overlay = overlayNone
			return m, nil
		}
		if isCtrlCKey(msg) {
			return m.handleCtrlC(previousEditorHeight)
		}
		return m, nil
	}

	if isCtrlCKey(msg) {
		return m.handleCtrlC(previousEditorHeight)
	}
	if updated, cmd, handled := m.handleResumeKeyPress(msg); handled {
		return updated, cmd
	}
	if isEscapeKey(msg) {
		if m.running && m.cancel != nil {
			m.cancel()
		}
		return m, nil
	}
	if key.Matches(msg, m.keymap.ToggleTools) {
		m.expandedTools = !m.expandedTools
		m.refreshViewportContent(!m.autoFollow)
		return m, nil
	}
	if key.Matches(msg, m.keymap.PageUp) {
		m.viewport.PageUp()
		if !m.viewport.AtBottom() {
			m.autoFollow = false
		}
		return m, nil
	}
	if key.Matches(msg, m.keymap.PageDown) {
		m.viewport.PageDown()
		m.autoFollow = m.viewport.AtBottom()
		return m, nil
	}
	if key.Matches(msg, m.keymap.Home) || key.Matches(msg, m.keymap.End) {
		return m.handleHomeOrEnd(msg, previousEditorHeight)
	}
	if updated, cmd, handled := m.handleCommandSuggestionKey(msg); handled {
		return updated, cmd
	}
	if m.shouldInsertNewline(msg) {
		return m.updateEditorWithKey(normalizeNewlineKey(msg), previousEditorHeight)
	}
	if key.Matches(msg, m.keymap.Help) && m.editor.Value() == "" {
		m.overlay = overlayHelp
		m.statusText = ""
		return m, nil
	}
	if key.Matches(msg, m.keymap.Submit) {
		return m.handleSubmit()
	}
	return m.updateComponents(msg, previousYOffset, previousEditorHeight, viewportBefore)
}

func (m Model) commandSuggestions() []slashCommand {
	if m.overlay != overlayNone {
		return nil
	}
	return matchingSlashCommands(m.editor.Value())
}

func (m Model) handleCommandSuggestionKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd, bool) {
	suggestions := m.commandSuggestions()
	if len(suggestions) == 0 {
		return m, nil, false
	}
	selected := clamp(m.commandSuggestionIndex, 0, len(suggestions)-1)
	switch {
	case key.Matches(msg, m.keymap.SuggestionUp):
		m.commandSuggestionIndex = (selected - 1 + len(suggestions)) % len(suggestions)
		return m, nil, true
	case key.Matches(msg, m.keymap.SuggestionDown):
		m.commandSuggestionIndex = (selected + 1) % len(suggestions)
		return m, nil, true
	case key.Matches(msg, m.keymap.Complete):
		m.editor.SetValue(suggestions[selected].Name)
		m.commandSuggestionIndex = 0
		m.rerenderAndRefreshViewportContent(!m.autoFollow)
		return m, nil, true
	default:
		return m, nil, false
	}
}

func (m Model) handleHomeOrEnd(msg tea.KeyPressMsg, previousEditorHeight int) (tea.Model, tea.Cmd) {
	beforeLine, beforeColumn := m.editor.Line(), m.editor.Column()
	beforeScroll := m.editor.ScrollYOffset()
	beforeStart, beforeEnd, beforeSelection := m.editor.Selection()
	updatedEditor, cmd := m.editor.Update(msg)
	afterStart, afterEnd, afterSelection := updatedEditor.Selection()
	consumed := updatedEditor.Line() != beforeLine || updatedEditor.Column() != beforeColumn || updatedEditor.ScrollYOffset() != beforeScroll || afterSelection != beforeSelection || afterStart != beforeStart || afterEnd != beforeEnd
	if consumed {
		m.editor = updatedEditor
		if m.editor.Height() != previousEditorHeight {
			m.rerenderAndRefreshViewportContent(!m.autoFollow)
		}
		return m, cmd
	}
	if key.Matches(msg, m.keymap.Home) {
		m.viewport.GotoTop()
		m.autoFollow = false
		return m, nil
	}
	m.viewport.GotoBottom()
	m.autoFollow = true
	return m, nil
}

func (m Model) updateEditorWithKey(msg tea.KeyPressMsg, previousEditorHeight int) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd
	previousEditorValue := m.editor.Value()
	previousSuggestionCount := len(m.commandSuggestions())
	m.editor, cmd = m.editor.Update(msg)
	if m.editor.Value() != previousEditorValue {
		m.commandSuggestionIndex = 0
	}
	if m.editor.Height() != previousEditorHeight || len(m.commandSuggestions()) != previousSuggestionCount {
		m.rerenderAndRefreshViewportContent(!m.autoFollow)
	}
	return m, cmd
}

func (m Model) shouldInsertNewline(msg tea.KeyPressMsg) bool {
	return isAltEnterKey(msg) || (isShiftEnterKey(msg) && m.supportsModifiedEnter)
}

func normalizeNewlineKey(msg tea.KeyPressMsg) tea.KeyPressMsg {
	if isShiftEnterKey(msg) {
		return tea.KeyPressMsg(tea.Key{Code: tea.KeyEnter, Mod: tea.ModAlt})
	}
	return msg
}

func isEscapeKey(msg tea.KeyPressMsg) bool {
	return msg.Key().Code == tea.KeyEscape || msg.Key().Code == tea.KeyEsc
}

func isCtrlCKey(msg tea.KeyPressMsg) bool {
	return msg.String() == "ctrl+c"
}

func isAltEnterKey(msg tea.KeyPressMsg) bool {
	return msg.Key().Code == tea.KeyEnter && msg.Key().Mod == tea.ModAlt
}

func isShiftEnterKey(msg tea.KeyPressMsg) bool {
	return msg.Key().Code == tea.KeyEnter && msg.Key().Mod == tea.ModShift
}

func newRootView(m Model, content string) tea.View {
	view := tea.NewView(fitToBounds(content, m.width, m.height))
	view.AltScreen = true
	view.MouseMode = tea.MouseModeCellMotion
	view.KeyboardEnhancements.ReportEventTypes = false
	view.KeyboardEnhancements.ReportAlternateKeys = true
	if cursor := m.editor.Cursor(); cursor != nil {
		view.Cursor = cursor
	}
	return view
}

func (m Model) handleSubmit() (tea.Model, tea.Cmd) {
	prompt := m.editor.Value()
	trimmed := strings.TrimSpace(prompt)
	if trimmed == "" {
		return m, nil
	}
	if strings.HasPrefix(trimmed, "/") {
		return m.handleCommand(trimmed)
	}
	if m.running || m.newSessionPending {
		return m, nil
	}
	return m.startPrompt(prompt)
}

func (m Model) handleCommand(name string) (tea.Model, tea.Cmd) {
	command, ok := findSlashCommand(name)
	if !ok {
		m.statusText = fmt.Sprintf("unknown command: %s", name)
		return m, nil
	}
	switch command.Kind {
	case slashCommandHelp:
		m.clearEditor()
		m.overlay = overlayHelp
		m.statusText = ""
		return m, nil
	case slashCommandSession:
		m.clearEditor()
		m.overlay = overlaySession
		m.statusText = ""
		return m, nil
	case slashCommandNew:
		if m.running {
			m.statusText = app.ErrPromptActive.Error()
			return m, nil
		}
		if m.newSessionPending {
			return m, nil
		}
		m.newSessionGeneration++
		m.newSessionPending = true
		m.statusText = ""
		return m, runNewSessionCommand(m.backend, m.newSessionGeneration)
	case slashCommandResume:
		return m.handleResumeCommand()
	case slashCommandExit:
		if m.running {
			return m, nil
		}
		return m, tea.Quit
	default:
		m.statusText = fmt.Sprintf("unknown command: %s", name)
		return m, nil
	}
}

func runNewSessionCommand(backend app.Backend, generation uint64) tea.Cmd {
	return func() tea.Msg {
		if backend == nil {
			return newSessionResultMsg{generation: generation, err: errors.New("backend is required")}
		}
		return newSessionResultMsg{generation: generation, err: backend.NewSession()}
	}
}

func (m Model) applyNewSessionResult(msg newSessionResultMsg) (tea.Model, tea.Cmd) {
	if !m.newSessionPending || msg.generation != m.newSessionGeneration {
		return m, nil
	}
	m.newSessionPending = false
	if msg.err != nil {
		m.statusText = msg.err.Error()
		return m, nil
	}
	if m.running {
		return m, nil
	}
	m.resetSessionViewFromBackend("")
	return m, nil
}

func (m *Model) resetSessionViewFromBackend(status string) {
	m.entries, m.usage = EntriesFromHistory(historyFromBackend(m.backend))
	m.overlay = overlayNone
	m.statusText = status
	if m.cancel != nil {
		m.cancel()
		m.cancel = nil
	}
	m.running = false
	m.clearCtrlCArm()
	m.dirtyStreaming = false
	m.renderTickActive = false
	m.activeTurnChannel = nil
	m.activeAssistant = -1
	m.turnErrorSeen = false
	m.turnEventErr = nil
	m.fatalErr = nil
	m.turnHistoryBaseline = turnHistoryBaseline{}
	m.turnEntryStart = 0
	m.liveEntrySequence = 0
	m.autoFollow = true
	m.editor.SetValue("")
	m.commandSuggestionIndex = 0
	m.rerenderAndRefreshViewportContent(false)
}

func (m Model) handleCtrlC(previousEditorHeight int) (tea.Model, tea.Cmd) {
	now := m.now()
	if m.ctrlCArmed {
		if now.Before(m.ctrlCArmedAt.Add(ctrlCArmWindow)) {
			if m.cancel != nil {
				m.cancel()
			}
			return m, tea.Quit
		}
		m.clearCtrlCArm()
	}
	if m.running && m.cancel != nil {
		m.cancel()
		return m.armCtrlC(now)
	}
	if m.editor.Value() != "" {
		hadSuggestions := len(m.commandSuggestions()) > 0
		m.editor.SetValue("")
		m.commandSuggestionIndex = 0
		if m.editor.Height() != previousEditorHeight || hadSuggestions {
			m.rerenderAndRefreshViewportContent(!m.autoFollow)
		}
	}
	return m.armCtrlC(now)
}

func (m Model) armCtrlC(now time.Time) (tea.Model, tea.Cmd) {
	m.ctrlCArmed = true
	m.ctrlCArmedAt = now
	m.ctrlCArmGeneration++
	m.statusText = ctrlCExitStatus
	return m, waitCtrlCArmExpiry(m.clock, m.ctrlCArmGeneration)
}

func (m *Model) clearCtrlCArm() {
	m.ctrlCArmed = false
	m.ctrlCArmedAt = time.Time{}
	if m.statusText == ctrlCExitStatus {
		m.statusText = ""
	}
}

func waitCtrlCArmExpiry(clock Clock, generation uint64) tea.Cmd {
	return func() tea.Msg {
		if clock == nil {
			clock = realClock{}
		}
		<-clock.After(ctrlCArmWindow)
		return ctrlCArmExpiredMsg{generation: generation}
	}
}

func (m Model) now() time.Time {
	if m.clock == nil {
		return time.Now()
	}
	return m.clock.Now()
}

func (m *Model) clearEditor() {
	previousEditorHeight := m.editor.Height()
	hadSuggestions := len(m.commandSuggestions()) > 0
	m.editor.SetValue("")
	m.commandSuggestionIndex = 0
	if m.editor.Height() != previousEditorHeight || hadSuggestions {
		m.rerenderAndRefreshViewportContent(!m.autoFollow)
	}
}

func (m Model) startPrompt(text string) (tea.Model, tea.Cmd) {
	ctx, cancel := context.WithCancel(rootContext(m.rootCtx))
	stream := newTurnStream()

	m.running = true
	m.cancel = cancel
	m.turnErrorSeen = false
	m.turnEventErr = nil
	m.statusText = ""
	m.dirtyStreaming = false
	m.renderTickActive = false
	m.activeAssistant = -1
	m.turnGeneration++
	m.activeTurnChannel = stream.channel
	m.turnHistoryBaseline = captureTurnHistoryBaseline(historyFromBackend(m.backend))
	m.turnEntryStart = len(m.entries)
	m.entries = append(m.entries, Entry{ID: m.nextLiveEntryID("user"), Kind: EntryUser, Raw: text})
	m.editor.SetValue("")
	m.commandSuggestionIndex = 0
	m.rerenderAndRefreshViewportContent(!m.autoFollow)

	return m, startTurnCommand(ctx, m.backend, text, stream)
}

func newTurnStream() *turnStream {
	return &turnStream{
		channel:           make(chan turnEnvelope, turnChannelCapacity),
		regularEventSlots: make(chan struct{}, turnChannelCapacity-1),
	}
}

func startTurnCommand(ctx context.Context, backend app.Backend, text string, stream *turnStream) tea.Cmd {
	return func() tea.Msg {
		go runTurnWorker(ctx, backend, text, stream)
		return waitTurn(stream)()
	}
}

func runTurnWorker(ctx context.Context, backend app.Backend, text string, stream *turnStream) {
	defer close(stream.channel)

	if backend == nil {
		stream.channel <- turnEnvelope{err: errors.New("backend is required"), done: true}
		return
	}

	var eventMu sync.Mutex
	acceptingEvents := true
	err := backend.Prompt(ctx, text, func(event agent.Event) {
		eventMu.Lock()
		defer eventMu.Unlock()
		if !acceptingEvents {
			return
		}
		eventCopy := event
		sendTurnEvent(ctx, stream, turnEnvelope{event: &eventCopy})
	})
	eventMu.Lock()
	acceptingEvents = false
	eventMu.Unlock()
	stream.channel <- turnEnvelope{err: err, done: true}
}

func sendTurnEvent(ctx context.Context, stream *turnStream, envelope turnEnvelope) bool {
	return sendRegularTurnEvent(ctx, stream, envelope)
}

func sendRegularTurnEvent(ctx context.Context, stream *turnStream, envelope turnEnvelope) bool {
	select {
	case stream.regularEventSlots <- struct{}{}:
	case <-ctx.Done():
		return false
	}
	envelope.usesRegularEventSlot = true
	select {
	case stream.channel <- envelope:
		return true
	case <-ctx.Done():
		<-stream.regularEventSlots
		return false
	}
}

func waitTurn(stream *turnStream) tea.Cmd {
	return func() tea.Msg {
		envelope, ok := <-stream.channel
		if !ok {
			envelope = turnEnvelope{done: true, err: errors.New("turn stream closed before completion")}
		} else if envelope.usesRegularEventSlot {
			<-stream.regularEventSlots
		}
		return turnMsg{channel: stream.channel, stream: stream, value: envelope}
	}
}

func (m Model) updateTurn(msg turnMsg) (tea.Model, tea.Cmd) {
	if !m.isActiveTurnMessage(msg) {
		if msg.value.done || msg.stream == nil {
			return m, nil
		}
		return m, waitTurn(msg.stream)
	}
	if msg.value.event != nil {
		return m.applyTurnEvent(msg.stream, *msg.value.event)
	}
	if msg.value.done {
		if m.reconcilePersistedToolResults() {
			m.refreshViewportContent(!m.autoFollow)
		}
		return m.finishTurn(msg.value.err)
	}
	return m, waitTurn(msg.stream)
}

func (m Model) isActiveTurnMessage(msg turnMsg) bool {
	return m.running && m.activeTurnChannel != nil && msg.channel == m.activeTurnChannel && msg.stream != nil && msg.stream.channel == m.activeTurnChannel
}

func (m Model) applyTurnEvent(stream *turnStream, event agent.Event) (tea.Model, tea.Cmd) {
	switch event.Type {
	case agent.EventTextDelta:
		m.applyTextDelta(event.Text)
		return m, tea.Batch(waitTurn(stream), m.scheduleRenderTick())
	case agent.EventToolCallStarted:
		m.finalizeStreamingRender()
		m.activeAssistant = -1
		m.entries = append(m.entries, Entry{
			ID:         m.nextLiveEntryID("tool"),
			Kind:       EntryTool,
			ToolCallID: event.ToolCallID,
			ToolName:   event.ToolName,
			ToolArgs:   event.ToolArgs,
		})
		m.refreshViewportContent(!m.autoFollow)
		return m, waitTurn(stream)
	case agent.EventToolCallFinished:
		m.finalizeStreamingRender()
		m.activeAssistant = -1
		m.finishToolEntry(event)
		m.refreshViewportContent(!m.autoFollow)
		return m, waitTurn(stream)
	case agent.EventProviderUsage:
		m.usage = addUsageTotals(m.usage, &event.Usage)
		return m, waitTurn(stream)
	case agent.EventAgentError:
		m.recordTurnError(event.Err)
		return m, waitTurn(stream)
	case agent.EventAgentFinished, agent.EventAgentStarted:
		return m, waitTurn(stream)
	default:
		return m, waitTurn(stream)
	}
}

func (m *Model) applyTextDelta(text string) {
	index := m.ensureActiveAssistantEntry()
	m.entries[index].Raw += text
	m.entries[index].RenderWidth = 0
	m.dirtyStreaming = true
}

func (m *Model) ensureActiveAssistantEntry() int {
	if m.activeAssistant >= 0 && m.activeAssistant < len(m.entries) && m.entries[m.activeAssistant].Kind == EntryAssistant {
		return m.activeAssistant
	}
	m.entries = append(m.entries, Entry{ID: m.nextLiveEntryID("assistant"), Kind: EntryAssistant})
	m.activeAssistant = len(m.entries) - 1
	return m.activeAssistant
}

func (m *Model) finishToolEntry(event agent.Event) {
	if index := m.findPendingToolEntry(event.ToolCallID); index >= 0 {
		m.entries[index].ToolDone = true
		m.entries[index].ToolError = event.ToolResult.IsError
		m.entries[index].ToolOutput = event.ToolResult.Content
		if m.entries[index].ToolName == "" {
			m.entries[index].ToolName = event.ToolName
		}
		if m.entries[index].ToolArgs == "" {
			m.entries[index].ToolArgs = event.ToolArgs
		}
		return
	}
	m.entries = append(m.entries, Entry{
		ID:         m.nextLiveEntryID("tool"),
		Kind:       EntryTool,
		ToolCallID: event.ToolCallID,
		ToolName:   event.ToolName,
		ToolArgs:   event.ToolArgs,
		ToolOutput: event.ToolResult.Content,
		ToolError:  event.ToolResult.IsError,
		ToolDone:   true,
	})
}

func (m Model) findPendingToolEntry(toolCallID string) int {
	for i := len(m.entries) - 1; i >= 0; i-- {
		entry := m.entries[i]
		if entry.Kind == EntryTool && !entry.ToolDone && entry.ToolCallID == toolCallID {
			return i
		}
	}
	return -1
}

func captureTurnHistoryBaseline(history []otmodel.Message) turnHistoryBaseline {
	digest, valid := historyDigest(history)
	return turnHistoryBaseline{messageCount: len(history), digest: digest, valid: valid}
}

func historyDigest(history []otmodel.Message) ([sha256.Size]byte, bool) {
	hasher := sha256.New()
	_, _ = fmt.Fprintf(hasher, "%d\n", len(history))
	encoder := json.NewEncoder(hasher)
	for _, message := range history {
		if err := encoder.Encode(message); err != nil {
			return [sha256.Size]byte{}, false
		}
	}
	var digest [sha256.Size]byte
	copy(digest[:], hasher.Sum(nil))
	return digest, true
}

func (m Model) persistedTurnToolResults() []otmodel.Block {
	baseline := m.turnHistoryBaseline
	if !baseline.valid {
		return nil
	}
	history := historyFromBackend(m.backend)
	if len(history) < baseline.messageCount {
		return nil
	}
	digest, valid := historyDigest(history[:baseline.messageCount])
	if !valid || digest != baseline.digest {
		return nil
	}

	var results []otmodel.Block
	for _, message := range history[baseline.messageCount:] {
		for _, block := range message.Blocks {
			if block.Type == otmodel.BlockToolResult {
				results = append(results, block)
			}
		}
	}
	return results
}

func (m *Model) reconcilePersistedToolResults() bool {
	results := m.persistedTurnToolResults()
	if len(results) == 0 {
		return false
	}
	m.finalizeStreamingRender()
	m.activeAssistant = -1

	usedEntries := make(map[int]struct{}, len(results))
	slots := make([]int, 0, len(results))
	reconciled := make([]Entry, 0, len(results))
	changed := false
	for _, result := range results {
		entryIndex := m.findTurnToolEntry(result.ToolCallID, usedEntries)
		var entry Entry
		if entryIndex >= 0 {
			entry = m.entries[entryIndex]
		} else {
			entry = Entry{ID: m.nextLiveEntryID("tool"), Kind: EntryTool}
			m.entries = append(m.entries, entry)
			entryIndex = len(m.entries) - 1
			changed = true
		}
		usedEntries[entryIndex] = struct{}{}
		slots = append(slots, entryIndex)
		entry.Kind = EntryTool
		entry.ToolCallID = result.ToolCallID
		entry.ToolName = result.ToolName
		entry.ToolOutput = result.Text
		entry.ToolError = result.IsError
		entry.ToolDone = true
		reconciled = append(reconciled, entry)
	}

	sort.Ints(slots)
	for index, slot := range slots {
		if m.entries[slot] != reconciled[index] {
			changed = true
		}
		m.entries[slot] = reconciled[index]
	}
	return changed
}

func (m Model) findTurnToolEntry(toolCallID string, used map[int]struct{}) int {
	if toolCallID == "" {
		return -1
	}
	start := max(0, min(m.turnEntryStart, len(m.entries)))
	for index := start; index < len(m.entries); index++ {
		entry := m.entries[index]
		if entry.Kind != EntryTool || entry.ToolCallID != toolCallID {
			continue
		}
		if _, alreadyUsed := used[index]; !alreadyUsed {
			return index
		}
	}
	return -1
}

func (m Model) finishTurn(err error) (tea.Model, tea.Cmd) {
	if err == nil {
		err = m.turnEventErr
	}
	m.finalizeStreamingRender()
	if m.reconcilePendingTools(err) {
		m.refreshViewportContent(!m.autoFollow)
	}
	if err != nil && !m.turnErrorSeen {
		m.recordTurnError(err)
	}
	m.completeTurnState()
	if errors.Is(err, session.ErrFatalPersistence) {
		m.fatalErr = err
		return m, tea.Quit
	}
	return m, nil
}

func (m *Model) reconcilePendingTools(err error) bool {
	changed := false
	result := "tool completion was not received"
	if err != nil {
		result = err.Error()
	}
	for i := range m.entries {
		entry := &m.entries[i]
		if entry.Kind != EntryTool || entry.ToolDone {
			continue
		}
		entry.ToolDone = true
		entry.ToolError = true
		if entry.ToolOutput == "" {
			entry.ToolOutput = result
		}
		changed = true
	}
	return changed
}

func (m *Model) recordTurnError(err error) {
	if err == nil || m.turnErrorSeen {
		return
	}
	m.turnErrorSeen = true
	m.turnEventErr = err
	m.entries = append(m.entries, Entry{ID: m.nextLiveEntryID("error"), Kind: EntryError, Raw: err.Error()})
	m.rerenderAndRefreshViewportContent(!m.autoFollow)
}

func (m *Model) finalizeStreamingRender() {
	if m.activeAssistant < 0 || m.activeAssistant >= len(m.entries) {
		return
	}
	m.entries[m.activeAssistant].RenderWidth = 0
	if !m.renderActiveAssistantEntry() {
		return
	}
	m.dirtyStreaming = false
	m.refreshViewportContent(!m.autoFollow)
}

func (m *Model) renderActiveAssistantEntry() bool {
	if m.activeAssistant < 0 || m.activeAssistant >= len(m.entries) {
		return false
	}
	m.renderEntryAt(m.activeAssistant, m.transcriptWidth())
	return true
}

func (m *Model) completeTurnState() {
	if m.cancel != nil {
		m.cancel()
		m.cancel = nil
	}
	m.running = false
	m.activeTurnChannel = nil
	m.renderTickActive = false
	m.turnErrorSeen = false
	m.turnEventErr = nil
	m.activeAssistant = -1
	m.turnHistoryBaseline = turnHistoryBaseline{}
	m.turnEntryStart = 0
}

func (m *Model) scheduleRenderTick() tea.Cmd {
	if !m.dirtyStreaming || m.renderTickActive {
		return nil
	}
	m.renderTickActive = true
	generation := m.turnGeneration
	return tea.Tick(streamRenderInterval, func(time.Time) tea.Msg {
		return renderStreamingMsg{generation: generation}
	})
}

func (m *Model) rerenderAndRefreshViewportContent(preserveOffset bool) {
	m.renderEntries(m.transcriptWidth())
	m.refreshViewportContent(preserveOffset)
}

func (m Model) transcriptWidth() int {
	return max(1, calculateLayout(m.width, m.height, m.editor, len(m.commandSuggestions())).transcriptWidth)
}

func (m *Model) refreshViewportContent(preserveOffset bool) {
	previousYOffset := m.viewport.YOffset()
	m.editor.SetWidth(max(0, m.width))
	layout := calculateLayout(m.width, m.height, m.editor, len(m.commandSuggestions()))
	m.editor.SetHeight(layout.editorHeight)
	m.viewport.SetWidth(layout.transcriptWidth)
	m.viewport.SetHeight(max(1, layout.transcriptHeight))

	transcriptWidth := max(1, layout.transcriptWidth)
	content := m.transcriptContent(transcriptWidth)
	m.viewport.SetContent(content)
	if m.autoFollow && !preserveOffset {
		m.viewport.GotoBottom()
		return
	}
	m.viewport.SetYOffset(previousYOffset)
}

func (m *Model) renderEntries(width int) {
	for i := range m.entries {
		m.renderEntryAt(i, width)
	}
}

func (m *Model) invalidateAssistantRenders() {
	for i := range m.entries {
		if m.entries[i].Kind == EntryAssistant {
			m.entries[i].RenderWidth = 0
			m.entries[i].Rendered = ""
		}
	}
}

func (m *Model) renderEntryAt(index int, width int) {
	if index < 0 || index >= len(m.entries) {
		return
	}
	entry := &m.entries[index]
	switch entry.Kind {
	case EntryAssistant:
		if entry.RenderWidth == width && (entry.Rendered != "" || entry.Raw == "") {
			return
		}
		rendered, _ := renderMarkdown(m.renderer, entry.Raw, width)
		entry.Rendered = rendered
		entry.RenderWidth = width
	case EntryTool:
		entry.RenderWidth = width
	default:
		if entry.RenderWidth == width && (entry.Rendered != "" || entry.Raw == "") {
			return
		}
		entry.Rendered = escapePlainText(entry.Raw)
		entry.RenderWidth = width
	}
}

func (m Model) transcriptContent(width int) string {
	blocks := make([]string, 0, len(m.entries))
	for _, entry := range m.entries {
		blocks = append(blocks, m.renderEntry(entry, width))
	}
	return strings.Join(blocks, "\n\n")
}

func (m Model) renderEntry(entry Entry, width int) string {
	switch entry.Kind {
	case EntryUser:
		return renderMessageBlock("You", entry.Rendered)
	case EntryAssistant:
		return renderMessageBlock("Otto", entry.Rendered)
	case EntryTool:
		return renderToolBlock(entry, width, m.expandedTools)
	case EntryError:
		return renderMessageBlock("Error", entry.Rendered)
	default:
		return renderMessageBlock("System", entry.Rendered)
	}
}

func renderMessageBlock(title, body string) string {
	title = lipgloss.NewStyle().Bold(true).Render(title)
	if body == "" {
		return title
	}
	return title + "\n" + body
}

func renderToolBlock(entry Entry, width int, expanded bool) string {
	_ = width
	name := escapeSingleLineText(entry.ToolName)
	if name == "" {
		name = "tool"
	}
	status := "running"
	if entry.ToolDone {
		status = "complete"
	}
	if entry.ToolError {
		status = "error"
	}
	parts := []string{">", name}
	if args := strings.TrimSpace(escapePlainText(entry.ToolArgs)); args != "" {
		parts = append(parts, args)
	}
	parts = append(parts, status)
	summary := strings.Join(parts, " ")
	if !expanded {
		return summary
	}
	lines := []string{summary}
	if output := strings.TrimSpace(escapePlainText(entry.ToolOutput)); output != "" {
		lines = append(lines, "", output)
	}
	return strings.Join(lines, "\n")
}

func (m *Model) scrollViewport(delta int) {
	if delta < 0 {
		m.viewport.ScrollUp(-delta)
		if !m.viewport.AtBottom() {
			m.autoFollow = false
		}
		return
	}
	if delta > 0 {
		m.viewport.ScrollDown(delta)
	}
	m.autoFollow = m.viewport.AtBottom()
}

func (m *Model) syncAutoFollow(before viewport.Model) {
	if m.viewport.AtBottom() {
		m.autoFollow = true
		return
	}
	if m.viewport.YOffset() < before.YOffset() {
		m.autoFollow = false
	}
}

func historyFromBackend(backend app.Backend) []otmodel.Message {
	if backend == nil {
		return nil
	}
	return backend.History()
}

func infoFromBackend(backend app.Backend) app.Info {
	if backend == nil {
		return app.Info{}
	}
	return backend.Info()
}

func rootContext(ctx context.Context) context.Context {
	if ctx == nil {
		return context.Background()
	}
	return ctx
}

func (m *Model) nextLiveEntryID(kind string) string {
	id := fmt.Sprintf("%s-%d", kind, m.liveEntrySequence)
	m.liveEntrySequence++
	return liveEntryIDPrefix + "-" + id
}

func (m Model) reservedStateActive() bool {
	return m.running || m.newSessionPending || m.resume.active() || m.resume.listPending || m.dirtyStreaming || m.renderTickActive || m.cancel != nil || m.ctrlCArmed || m.fatalErr != nil
}
