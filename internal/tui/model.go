package tui

import (
	"context"
	"errors"
	"fmt"
	"strings"
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
	liveEntryIDPrefix    = "live"
)

type Option func(*Model)

func WithRenderer(renderer MarkdownRenderer) Option {
	return func(model *Model) {
		model.renderer = renderer
	}
}

type Model struct {
	rootCtx           context.Context
	backend           app.Backend
	entries           []Entry
	viewport          viewport.Model
	editor            textarea.Model
	spinner           spinner.Model
	keymap            KeyMap
	width             int
	height            int
	usage             otmodel.Usage
	running           bool
	expandedTools     bool
	overlay           overlayKind
	autoFollow        bool
	renderer          MarkdownRenderer
	dirtyStreaming    bool
	renderTickActive  bool
	cancel            context.CancelFunc
	ctrlCArmedAt      time.Time
	activeAssistant   int
	turnErrorSeen     bool
	turnEventErr      error
	fatalErr          error
	turnGeneration    uint64
	liveEntrySequence int
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
	editor.KeyMap.InsertNewline = DefaultKeyMap().InsertNewline
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
		autoFollow:      true,
		renderer:        GlamourRenderer{},
		activeAssistant: -1,
	}
	for _, option := range options {
		option(&model)
	}
	model.rerenderAndRefreshViewportContent(false)
	return model
}

func (m Model) Init() tea.Cmd {
	return nil
}

func (m Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	previousYOffset := m.viewport.YOffset()
	previousEditorHeight := m.editor.Height()
	viewportBefore := m.viewport

	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.width = max(0, msg.Width)
		m.height = max(0, msg.Height)
		m.rerenderAndRefreshViewportContent(!m.autoFollow)
		return m, nil
	case showHelpOverlayMsg:
		m.overlay = overlayHelp
		return m, nil
	case showSessionOverlayMsg:
		m.overlay = overlaySession
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
		if key.Matches(msg, m.keymap.Submit) {
			if m.running {
				return m, nil
			}
			prompt := m.editor.Value()
			if strings.TrimSpace(prompt) == "" {
				return m, nil
			}
			return m.startPrompt(prompt)
		}
		if key.Matches(msg, m.keymap.Cancel) && m.running && m.cancel != nil {
			m.cancel()
			return m, nil
		}
	}

	var cmd tea.Cmd
	m.viewport, _ = m.viewport.Update(msg)
	m.editor, cmd = m.editor.Update(msg)
	m.spinner, _ = m.spinner.Update(msg)
	m.syncAutoFollow(viewportBefore)

	if m.editor.Height() != previousEditorHeight {
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

	layout := calculateLayout(m.width, m.height, m.editor)
	if layout.tooSmall {
		return newRootView(m, smallTerminalView(m.width, m.height))
	}

	transcript := lipgloss.NewStyle().Width(layout.transcriptWidth).Height(layout.transcriptHeight).Render(m.viewport.View())
	editor := lipgloss.NewStyle().Width(m.width).Height(layout.editorHeight).Render(m.editor.View())
	footer := lipgloss.NewStyle().Width(m.width).Render(renderFooter(m.width, infoFromBackend(m.backend), m.usage))
	content := lipgloss.JoinVertical(lipgloss.Left, transcript, editor, footer)
	if m.overlay != overlayNone {
		content = renderOverlay(m.width, m.height, m.overlayContent())
	}
	return newRootView(m, content)
}

func (m Model) overlayContent() string {
	switch m.overlay {
	case overlayHelp:
		return helpOverlayContent()
	case overlaySession:
		return sessionOverlayContent(infoFromBackend(m.backend))
	default:
		return ""
	}
}

func newRootView(m Model, content string) tea.View {
	view := tea.NewView(content)
	view.AltScreen = true
	view.MouseMode = tea.MouseModeCellMotion
	view.KeyboardEnhancements.ReportEventTypes = false
	view.KeyboardEnhancements.ReportAlternateKeys = true
	if cursor := m.editor.Cursor(); cursor != nil {
		view.Cursor = cursor
	}
	return view
}

func (m Model) startPrompt(text string) (tea.Model, tea.Cmd) {
	ctx, cancel := context.WithCancel(rootContext(m.rootCtx))
	stream := newTurnStream()

	m.running = true
	m.cancel = cancel
	m.turnErrorSeen = false
	m.turnEventErr = nil
	m.dirtyStreaming = false
	m.renderTickActive = false
	m.activeAssistant = -1
	m.turnGeneration++
	m.entries = append(m.entries, Entry{ID: m.nextLiveEntryID("user"), Kind: EntryUser, Raw: text})
	m.editor.SetValue("")
	m.rerenderAndRefreshViewportContent(!m.autoFollow)

	return m, startTurnCommand(ctx, m.backend, text, stream)
}

func newTurnStream() *turnStream {
	return &turnStream{
		channel:    make(chan turnEnvelope, turnChannelCapacity),
		eventSlots: make(chan struct{}, turnChannelCapacity-1),
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

	err := backend.Prompt(ctx, text, func(event agent.Event) {
		eventCopy := event
		sendTurnEvent(ctx, stream, turnEnvelope{event: &eventCopy})
	})
	stream.channel <- turnEnvelope{err: err, done: true}
}

func sendTurnEvent(ctx context.Context, stream *turnStream, envelope turnEnvelope) bool {
	select {
	case stream.eventSlots <- struct{}{}:
	case <-ctx.Done():
		return false
	}
	select {
	case stream.channel <- envelope:
		return true
	case <-ctx.Done():
		<-stream.eventSlots
		return false
	}
}

func waitTurn(stream *turnStream) tea.Cmd {
	return func() tea.Msg {
		envelope, ok := <-stream.channel
		if !ok {
			envelope = turnEnvelope{done: true, err: errors.New("turn stream closed before completion")}
		} else if envelope.event != nil {
			<-stream.eventSlots
		}
		return turnMsg{channel: stream.channel, stream: stream, value: envelope}
	}
}

func (m Model) updateTurn(msg turnMsg) (tea.Model, tea.Cmd) {
	if msg.value.event != nil {
		return m.applyTurnEvent(msg.stream, *msg.value.event)
	}
	if msg.value.done {
		return m.finishTurn(msg.value.err)
	}
	return m, waitTurn(msg.stream)
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
		return
	}
	m.entries = append(m.entries, Entry{
		ID:         m.nextLiveEntryID("tool"),
		Kind:       EntryTool,
		ToolCallID: event.ToolCallID,
		ToolName:   event.ToolName,
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

func (m Model) finishTurn(err error) (tea.Model, tea.Cmd) {
	if err == nil {
		err = m.turnEventErr
	}
	m.finalizeStreamingRender()
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
	m.renderTickActive = false
	m.turnErrorSeen = false
	m.turnEventErr = nil
	m.activeAssistant = -1
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
	return max(1, calculateLayout(m.width, m.height, m.editor).transcriptWidth)
}

func (m *Model) refreshViewportContent(preserveOffset bool) {
	previousYOffset := m.viewport.YOffset()
	m.editor.SetWidth(max(0, m.width))
	layout := calculateLayout(m.width, m.height, m.editor)
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
	name := entry.ToolName
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
	if args := strings.TrimSpace(entry.ToolArgs); args != "" {
		parts = append(parts, args)
	}
	parts = append(parts, status)
	summary := strings.Join(parts, " ")
	if !expanded {
		return summary
	}
	lines := []string{summary}
	if output := strings.TrimSpace(entry.ToolOutput); output != "" {
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
	return m.running || m.dirtyStreaming || m.renderTickActive || m.cancel != nil || !m.ctrlCArmedAt.IsZero() || m.fatalErr != nil
}
