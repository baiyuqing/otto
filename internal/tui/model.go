package tui

import (
	"context"
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
	"github.com/charmbracelet/x/ansi"
)

const (
	turnChannelCapacity  = 64
	streamRenderInterval = 50 * time.Millisecond
	ctrlCArmWindow       = time.Second
	liveEntryIDPrefix    = "live"
	ctrlCExitStatus      = "press Ctrl+C again to exit"
	promptHistoryLimit   = 100
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

type operationKind uint8

const (
	operationNone operationKind = iota
	operationPrompt
	operationCompact
)

type turnHistoryBaseline struct {
	idsJSON string
	valid   bool
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
	expandedDetails        bool
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
	operationCleanup       *operationCleanup
	activeOperation        *activeOperation
	ctrlCArmed             bool
	ctrlCArmedAt           time.Time
	ctrlCArmGeneration     uint64
	newSessionPending      bool
	newSessionGeneration   uint64
	resume                 resumePickerState
	activeTurnStream       *turnStream
	activeTurnChannel      <-chan turnEnvelope
	activeAssistant        int
	turnErrorSeen          bool
	turnEventErr           error
	fatalErr               error
	turnGeneration         uint64
	operationKind          operationKind
	compactionCompleted    bool
	turnHistoryBaseline    turnHistoryBaseline
	turnEntryStart         int
	liveEntrySequence      int
	commandSuggestionIndex int
	promptHistory          []string
	promptHistoryIndex     int
	promptDraft            string
	turnStartedAt          time.Time
	turnDuration           time.Duration
}

func NewModel(ctx context.Context, backend app.Backend, options ...Option) Model {
	entries, usage := entriesAndUsageFromBackend(backend)
	editor := textarea.New()
	editor.ShowLineNumbers = false
	editor.Prompt = "> "
	editor.Placeholder = "Ask Otto"
	editor.MinHeight = minEditorHeight
	editor.MaxHeight = maxEditorHeight
	editor.DynamicHeight = true
	editor.SetHeight(minEditorHeight)
	editor.SetWidth(0)
	editor.SetVirtualCursor(false)
	editor.KeyMap.InsertNewline = key.NewBinding(key.WithKeys("alt+enter"), key.WithHelp("alt+enter", "insert newline"))
	_ = editor.Focus()

	vp := viewport.New()
	vp.MouseWheelEnabled = true
	vp.SoftWrap = true

	model := Model{
		rootCtx:            ctx,
		backend:            backend,
		entries:            entries,
		viewport:           vp,
		editor:             editor,
		spinner:            spinner.New(spinner.WithSpinner(spinner.MiniDot)),
		keymap:             DefaultKeyMap(),
		usage:              usage,
		expandedDetails:    false,
		autoFollow:         true,
		darkBackground:     true,
		renderer:           newGlamourRenderer(true),
		clock:              realClock{},
		operationCleanup:   newOperationCleanup(),
		activeAssistant:    -1,
		promptHistoryIndex: -1,
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
			m.invalidateMarkdownRenders()
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
		if len(m.resume.sessions) == 0 {
			m.resume.selected = 0
		} else {
			m.resume.selected = clamp(m.resume.selected, 0, len(m.resume.sessions)-1)
		}
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
	case toggleDetailsMsg:
		m.expandedDetails = !m.expandedDetails
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
		if m.resume.active() || m.overlay != overlayNone {
			return m, nil
		}
	case tea.MouseMsg:
		if m.resume.active() || m.overlay != overlayNone {
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
	if m.resume.active() {
		return newRootView(m, renderResumePicker(m.width, m.height, m.resume, m.spinner.View(), m.now()))
	}

	transcript := lipgloss.NewStyle().Width(layout.transcriptWidth).Height(layout.transcriptHeight).MaxHeight(layout.transcriptHeight).Render(m.viewport.View())
	footer := lipgloss.NewStyle().Width(m.width).Render(renderFooter(m.width, infoFromBackend(m.backend), m.usage, m.footerStatus()))
	parts := []string{transcript}
	if layout.suggestionHeight > 0 {
		parts = append(parts, renderCommandSuggestions(m.width, suggestions, m.commandSuggestionIndex, layout.suggestionHeight))
	}
	if layout.editorSpacing > 0 {
		parts = append(parts, lipgloss.NewStyle().Width(m.width).Height(layout.editorSpacing).MaxHeight(layout.editorSpacing).Render(""))
	}
	if layout.inputBoxed {
		parts = append(parts, boxedInput(m.width, layout.editorHeight, m.editor.View()))
	} else {
		parts = append(parts, lipgloss.NewStyle().Width(m.width).Height(layout.editorHeight).Render(m.editor.View()))
	}
	parts = append(parts, footer)
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
	running := m.spinner.View() + " working " + formatTurnSeconds(m.now().Sub(m.turnStartedAt))
	if m.statusText == "" {
		return running
	}
	return running + " · " + m.statusText
}

func formatTurnSeconds(duration time.Duration) string {
	if duration < time.Second {
		return "0s"
	}
	seconds := int64(duration / time.Second)
	if seconds < 60 {
		return fmt.Sprintf("%ds", seconds)
	}
	minutes := seconds / 60
	remSeconds := seconds % 60
	if minutes < 60 {
		if remSeconds == 0 {
			return fmt.Sprintf("%dm", minutes)
		}
		return fmt.Sprintf("%dm %ds", minutes, remSeconds)
	}
	hours := minutes / 60
	remMinutes := minutes % 60
	if remMinutes == 0 {
		return fmt.Sprintf("%dh", hours)
	}
	return fmt.Sprintf("%dh %dm", hours, remMinutes)
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
	if isCtrlCKey(msg) {
		return m.handleCtrlC(previousEditorHeight)
	}
	if updated, cmd, handled := m.handleResumeKeyPress(msg); handled {
		return updated, cmd
	}
	if m.overlay != overlayNone {
		if isEscapeKey(msg) {
			m.overlay = overlayNone
			return m, nil
		}
		return m, nil
	}
	if isEscapeKey(msg) {
		if m.running && m.cancel != nil {
			m.cancel()
		}
		return m, nil
	}
	if key.Matches(msg, m.keymap.ToggleDetails) {
		m.expandedDetails = !m.expandedDetails
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
	if updated, cmd, handled := m.handlePromptHistoryKey(msg); handled {
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

func (m Model) handlePromptHistoryKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd, bool) {
	switch {
	case key.Matches(msg, m.keymap.SuggestionUp):
		if m.promptHistoryIndex < 0 {
			if m.editor.Value() != "" || len(m.promptHistory) == 0 {
				return m, nil, false
			}
			m.promptDraft = m.editor.Value()
			m.promptHistoryIndex = len(m.promptHistory) - 1
			m.setPromptHistoryEditorValue(m.promptHistory[m.promptHistoryIndex])
			return m, nil, true
		}
		if m.promptHistoryIndex > 0 {
			m.promptHistoryIndex--
			m.setPromptHistoryEditorValue(m.promptHistory[m.promptHistoryIndex])
		}
		return m, nil, true
	case key.Matches(msg, m.keymap.SuggestionDown):
		if m.promptHistoryIndex < 0 {
			return m, nil, false
		}
		if m.promptHistoryIndex < len(m.promptHistory)-1 {
			m.promptHistoryIndex++
			m.setPromptHistoryEditorValue(m.promptHistory[m.promptHistoryIndex])
		} else {
			m.setPromptHistoryEditorValue(m.promptDraft)
			m.promptHistoryIndex = -1
		}
		return m, nil, true
	default:
		return m, nil, false
	}
}

func (m *Model) setPromptHistoryEditorValue(value string) {
	previousEditorHeight := m.editor.Height()
	hadSuggestions := len(m.commandSuggestions()) > 0
	m.editor.SetValue(value)
	m.commandSuggestionIndex = 0
	if m.editor.Height() != previousEditorHeight || hadSuggestions {
		m.rerenderAndRefreshViewportContent(!m.autoFollow)
	}
}

func (m *Model) addPromptHistory(trimmed string) {
	if trimmed == "" {
		return
	}
	if last := len(m.promptHistory) - 1; last < 0 || m.promptHistory[last] != trimmed {
		m.promptHistory = append(m.promptHistory, trimmed)
		if len(m.promptHistory) > promptHistoryLimit {
			m.promptHistory = m.promptHistory[len(m.promptHistory)-promptHistoryLimit:]
		}
	}
	m.promptHistoryIndex = -1
	m.promptDraft = ""
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
	layout := calculateLayout(m.width, m.height, m.editor, len(m.commandSuggestions()))
	if !layout.tooSmall && !m.resume.active() && m.overlay == overlayNone {
		if cursor := m.editor.Cursor(); cursor != nil {
			cursor.Y += layout.transcriptHeight + layout.suggestionHeight + layout.editorSpacing
			if layout.inputBoxed {
				// The textarea sits below the top border and the label row.
				cursor.Y += 1 + inputBoxLabel
				cursor.X += 1 + inputBoxPadding
			}
			view.Cursor = cursor
		}
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
	m.addPromptHistory(trimmed)
	return m.startPrompt(prompt)
}

func (m Model) handleCommand(value string) (tea.Model, tea.Cmd) {
	command, argument, ok := parseSlashCommand(value)
	if !ok || (argument != "" && command.Kind != slashCommandCompact) {
		m.statusText = fmt.Sprintf("unknown command: %s", value)
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
	case slashCommandCompact:
		if m.running || m.newSessionPending {
			m.statusText = app.ErrPromptActive.Error()
			return m, nil
		}
		return m.startCompaction(argument)
	case slashCommandExit:
		if m.running {
			return m, nil
		}
		return m.quit()
	default:
		m.statusText = fmt.Sprintf("unknown command: %s", value)
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
	m.abandonActiveTurn()
	m.entries, m.usage = entriesAndUsageFromBackend(m.backend)
	m.overlay = overlayNone
	m.statusText = status
	m.cancel = nil
	m.running = false
	m.clearCtrlCArm()
	m.dirtyStreaming = false
	m.renderTickActive = false
	m.activeTurnStream = nil
	m.activeTurnChannel = nil
	m.activeOperation = nil
	m.activeAssistant = -1
	m.turnErrorSeen = false
	m.turnEventErr = nil
	m.fatalErr = nil
	m.operationKind = operationNone
	m.compactionCompleted = false
	m.turnHistoryBaseline = turnHistoryBaseline{}
	m.turnEntryStart = 0
	m.liveEntrySequence = 0
	m.turnStartedAt = time.Time{}
	m.turnDuration = 0
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
			return m.quit()
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
	m.clearCtrlCArm()
	m.turnErrorSeen = false
	m.turnEventErr = nil
	m.statusText = ""
	m.turnStartedAt = m.now()
	m.turnDuration = 0
	m.dirtyStreaming = false
	m.renderTickActive = false
	m.activeAssistant = -1
	m.turnGeneration++
	m.operationKind = operationPrompt
	m.compactionCompleted = false
	stream.generation = m.turnGeneration
	m.activeTurnStream = stream
	m.activeTurnChannel = stream.channel
	m.registerActiveOperation(stream, cancel)
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
		abandonSignal:     make(chan struct{}),
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
		sendDurableTurnEnvelope(stream, turnEnvelope{err: errors.New("backend is required"), done: true})
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
		eventCopy := cloneAgentEvent(event)
		envelope := turnEnvelope{event: &eventCopy}
		if isCommittedCompactionCompletion(eventCopy) {
			envelope.aggregateUsage, envelope.aggregateUsagePresent = aggregateUsageSnapshot(backend)
		}
		sendTurnEvent(ctx, stream, envelope)
	})
	eventMu.Lock()
	acceptingEvents = false
	eventMu.Unlock()
	sendDurableTurnEnvelope(stream, turnEnvelope{err: err, done: true})
}

func cloneAgentEvent(event agent.Event) agent.Event {
	cloned := event
	if event.Compaction != nil {
		compaction := *event.Compaction
		cloned.Compaction = &compaction
	}
	return cloned
}

func aggregateUsageSnapshot(backend app.Backend) (otmodel.Usage, bool) {
	info := infoFromBackend(backend)
	return info.Usage, info.UsagePresent
}

func isCommittedCompactionCompletion(event agent.Event) bool {
	return event.Type == agent.EventCompactionCompleted && event.Compaction != nil
}

func sendTurnEvent(ctx context.Context, stream *turnStream, envelope turnEnvelope) bool {
	if envelope.event != nil && isCommittedCompactionCompletion(*envelope.event) {
		envelope.applicationAck = newTurnApplicationAck()
		if !sendDurableTurnEnvelope(stream, envelope) {
			return false
		}
		select {
		case <-envelope.applicationAck.done:
			return true
		case <-stream.abandonSignal:
			return false
		}
	}
	return sendRegularTurnEvent(ctx, stream, envelope)
}

func sendDurableTurnEnvelope(stream *turnStream, envelope turnEnvelope) bool {
	if stream == nil {
		return false
	}
	select {
	case <-stream.abandonSignal:
		return false
	default:
	}
	select {
	case stream.channel <- envelope:
		return true
	case <-stream.abandonSignal:
		return false
	}
}

func sendRegularTurnEvent(ctx context.Context, stream *turnStream, envelope turnEnvelope) bool {
	if stream == nil {
		return false
	}
	select {
	case <-stream.abandonSignal:
		return false
	default:
	}
	select {
	case stream.regularEventSlots <- struct{}{}:
	case <-ctx.Done():
		return false
	case <-stream.abandonSignal:
		return false
	}
	envelope.usesRegularEventSlot = true
	select {
	case <-stream.abandonSignal:
		<-stream.regularEventSlots
		return false
	default:
	}
	select {
	case stream.channel <- envelope:
		return true
	case <-ctx.Done():
		<-stream.regularEventSlots
		return false
	case <-stream.abandonSignal:
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
		return turnMsg{channel: stream.channel, stream: stream, generation: stream.generation, value: envelope}
	}
}

func (m Model) updateTurn(msg turnMsg) (tea.Model, tea.Cmd) {
	if msg.value.applicationAck != nil {
		defer msg.value.applicationAck.acknowledge()
	}
	if !m.isActiveTurnMessage(msg) {
		if msg.value.done || msg.stream == nil {
			return m, nil
		}
		if m.running && msg.channel == m.activeTurnChannel {
			return m, nil
		}
		return m, waitTurn(msg.stream)
	}
	if msg.value.event != nil {
		return m.applyTurnEvent(msg.stream, msg.value)
	}
	if msg.value.done {
		if m.reconcilePersistedToolResults() {
			m.refreshViewportContent(!m.autoFollow)
		}
		return m.finishTurn(msg.value)
	}
	return m, waitTurn(msg.stream)
}

func (m Model) isActiveTurnMessage(msg turnMsg) bool {
	return m.running && m.activeTurnStream != nil && msg.generation == m.turnGeneration && msg.stream == m.activeTurnStream && msg.channel == m.activeTurnStream.channel && msg.stream.generation == m.turnGeneration && m.activeTurnChannel == m.activeTurnStream.channel
}

func (m Model) applyTurnEvent(stream *turnStream, envelope turnEnvelope) (tea.Model, tea.Cmd) {
	event := *envelope.event
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
	case agent.EventCompactionStarted, agent.EventCompactionCompleted, agent.EventCompactionWarning:
		if m.applyCompactionEvent(event, envelope.aggregateUsage, envelope.aggregateUsagePresent) {
			m.refreshViewportContent(!m.autoFollow)
		}
		return m, waitTurn(stream)
	case agent.EventAgentFinished:
		m.turnDuration = m.now().Sub(m.turnStartedAt)
		if m.statusText == "" {
			m.statusText = "completed in " + formatTurnSeconds(m.turnDuration)
		}
		return m, waitTurn(stream)
	case agent.EventAgentStarted:
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
	ids := make([]string, 0, len(history))
	seen := make(map[string]struct{}, len(history))
	for _, message := range history {
		if message.ID == "" {
			continue
		}
		if _, ok := seen[message.ID]; ok {
			continue
		}
		seen[message.ID] = struct{}{}
		ids = append(ids, message.ID)
	}
	sort.Strings(ids)
	encoded, err := json.Marshal(ids)
	if err != nil {
		return turnHistoryBaseline{}
	}
	return turnHistoryBaseline{idsJSON: string(encoded), valid: true}
}

func (b turnHistoryBaseline) idSet() map[string]struct{} {
	if !b.valid {
		return nil
	}
	var ids []string
	if err := json.Unmarshal([]byte(b.idsJSON), &ids); err != nil {
		return nil
	}
	set := make(map[string]struct{}, len(ids))
	for _, id := range ids {
		if id == "" {
			continue
		}
		set[id] = struct{}{}
	}
	return set
}

func (m Model) persistedTurnMessages() []otmodel.Message {
	baseline := m.turnHistoryBaseline
	if !baseline.valid {
		return nil
	}
	knownIDs := baseline.idSet()
	if knownIDs == nil {
		return nil
	}
	messages := make([]otmodel.Message, 0)
	for _, message := range historyFromBackend(m.backend) {
		if message.ID == "" {
			continue
		}
		if _, seen := knownIDs[message.ID]; seen {
			continue
		}
		messages = append(messages, message)
	}
	return messages
}

func (m Model) persistedTurnToolResults() []otmodel.Block {
	messages := m.persistedTurnMessages()
	if len(messages) == 0 {
		return nil
	}
	results := make([]otmodel.Block, 0)
	for _, message := range messages {
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

func (m *Model) reconcilePersistedCheckpoint(checkpointID string, tokensAfter int) bool {
	if checkpointID == "" {
		return false
	}
	message, ok := m.persistedCompactionMessage(checkpointID)
	if !ok {
		return m.updateCompactionEntryTokens(checkpointID, tokensAfter)
	}
	entry, ok := compactionEntryFromMessage(message, 0)
	if !ok {
		return false
	}
	entry.TokensAfter = max(0, tokensAfter)
	return m.upsertCompactionEntry(entry)
}

func (m Model) persistedCompactionMessage(checkpointID string) (otmodel.Message, bool) {
	for _, message := range m.persistedTurnMessages() {
		if message.ID != checkpointID {
			continue
		}
		if message.Role == otmodel.RoleContext && message.ContextType == "compaction" && message.Display {
			return message, true
		}
		return otmodel.Message{}, false
	}
	return otmodel.Message{}, false
}

func (m *Model) upsertCompactionEntry(entry Entry) bool {
	if entry.Kind != EntryCompaction {
		return false
	}
	for index := range m.entries {
		if m.entries[index].Kind != EntryCompaction || m.entries[index].CheckpointID != entry.CheckpointID {
			continue
		}
		updated := m.entries[index]
		if entry.Raw != "" {
			updated.Raw = entry.Raw
		}
		updated.TokensBefore = max(updated.TokensBefore, entry.TokensBefore)
		updated.TokensAfter = max(updated.TokensAfter, entry.TokensAfter)
		if updated == m.entries[index] {
			return false
		}
		updated.RenderWidth = 0
		m.entries[index] = updated
		m.renderEntryAt(index, m.transcriptWidth())
		return true
	}
	m.entries = append(m.entries, entry)
	m.renderEntryAt(len(m.entries)-1, m.transcriptWidth())
	return true
}

func (m *Model) updateCompactionEntryTokens(checkpointID string, tokensAfter int) bool {
	if checkpointID == "" || tokensAfter <= 0 {
		return false
	}
	for index := range m.entries {
		entry := m.entries[index]
		if entry.Kind != EntryCompaction || entry.CheckpointID != checkpointID || entry.TokensAfter == tokensAfter {
			continue
		}
		entry.TokensAfter = tokensAfter
		entry.RenderWidth = 0
		m.entries[index] = entry
		m.renderEntryAt(index, m.transcriptWidth())
		return true
	}
	return false
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

func (m Model) finishTurn(envelope turnEnvelope) (tea.Model, tea.Cmd) {
	err := envelope.err
	changed := false
	if m.operationKind == operationCompact && !m.compactionCompleted && envelope.compactionResult != nil && (envelope.compactionResult.Noop || envelope.compactionResult.CheckpointID != "" || err == nil) {
		changed = m.applyCompactionResult(*envelope.compactionResult, envelope.aggregateUsage, envelope.aggregateUsagePresent) || changed
	}
	if err == nil {
		err = m.turnEventErr
	}
	m.finalizeStreamingRender()
	if m.reconcilePendingTools(err) {
		changed = true
	}
	if changed {
		m.refreshViewportContent(!m.autoFollow)
	}
	if err != nil && !m.turnErrorSeen {
		m.recordTurnError(err)
	}
	m.completeTurnState()
	if errors.Is(err, session.ErrFatalPersistence) {
		m.fatalErr = err
		return m.quit()
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
	if m.activeOperation != nil {
		m.operationCleanup.finish(m.activeOperation)
	} else if m.cancel != nil {
		m.cancel()
	}
	m.cancel = nil
	m.activeOperation = nil
	m.running = false
	m.activeTurnStream = nil
	m.activeTurnChannel = nil
	m.renderTickActive = false
	m.turnErrorSeen = false
	m.turnEventErr = nil
	m.activeAssistant = -1
	m.operationKind = operationNone
	m.compactionCompleted = false
	m.turnHistoryBaseline = turnHistoryBaseline{}
	m.turnEntryStart = 0
	m.turnStartedAt = time.Time{}
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
	layout := calculateLayout(m.width, m.height, m.editor, len(m.commandSuggestions()))
	editorWidth := m.width
	if layout.inputBoxed {
		editorWidth = max(1, m.width-inputBoxPadding*2-inputBoxBorder)
	}
	m.editor.SetWidth(max(0, editorWidth))
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

func (m *Model) invalidateMarkdownRenders() {
	for i := range m.entries {
		if m.entries[i].Kind == EntryAssistant || m.entries[i].Kind == EntryCompaction {
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
	case EntryAssistant, EntryCompaction:
		renderWidth := width
		if entry.Kind == EntryAssistant {
			renderWidth = proseWidth(width)
		}
		if entry.RenderWidth == renderWidth && (entry.Rendered != "" || entry.Raw == "") {
			return
		}
		rendered, _ := renderMarkdown(m.renderer, entry.Raw, renderWidth)
		entry.Rendered = rendered
		entry.RenderWidth = renderWidth
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
	if len(m.entries) == 0 && !m.running {
		return emptyTranscriptHint(width)
	}
	blocks := make([]string, 0, len(m.entries))
	assistantTurn := false
	for _, entry := range m.entries {
		if entry.Kind == EntryUser {
			assistantTurn = false
			blocks = append(blocks, renderUserBlock(entry.Rendered, width, m.darkBackground))
			continue
		}
		if entry.Kind == EntryAssistant || entry.Kind == EntryTool {
			if !assistantTurn {
				assistantTurn = true
				if entry.Kind == EntryAssistant {
					blocks = append(blocks, renderMessageBlock("Otto", entry.Rendered))
					continue
				}
				blocks = append(blocks, renderMessageBlock("Otto", ""))
			}
			if entry.Kind == EntryTool {
				blocks = append(blocks, indentToolBlock(renderToolBlock(entry, width, m.expandedDetails), width))
			} else {
				blocks = append(blocks, entry.Rendered)
			}
			continue
		}
		assistantTurn = false
		blocks = append(blocks, m.renderEntry(entry, width))
	}
	return strings.Join(blocks, "\n\n")
}

const emptyTranscriptHintText = "Ask Otto anything. Type /help for commands or /resume to continue a session."

func emptyTranscriptHint(width int) string {
	return clipSingleLineText(emptyTranscriptHintText, width)
}

func (m Model) renderEntry(entry Entry, width int) string {
	switch entry.Kind {
	case EntryUser:
		return renderMessageBlock("You", entry.Rendered)
	case EntryAssistant:
		return renderMessageBlock("Otto", entry.Rendered)
	case EntryTool:
		return renderToolBlock(entry, width, m.expandedDetails)
	case EntryCompaction:
		return renderCompactionBlock(entry, width, m.expandedDetails)
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

func proseWidth(width int) int { return min(100, max(1, width)) }

func renderUserBlock(body string, width int, dark bool) string {
	width = proseWidth(width)
	content := []string{"│ " + lipgloss.NewStyle().Bold(true).Render("You")}
	if body != "" {
		for _, line := range strings.Split(ansi.Wrap(body, max(1, width-2), ""), "\n") {
			content = append(content, "│ "+line)
		}
	}
	background := "254"
	if dark {
		background = "236"
	}
	return lipgloss.NewStyle().Width(width).Background(lipgloss.Color(background)).Render(strings.Join(content, "\n"))
}

func indentToolBlock(block string, width int) string {
	lines := make([]string, 0)
	for _, line := range strings.Split(block, "\n") {
		wrapped := ansi.Wrap(line, max(1, width-2), "")
		for _, part := range strings.Split(wrapped, "\n") {
			lines = append(lines, ansi.Truncate("  "+part, max(0, width), ""))
		}
	}
	return strings.Join(lines, "\n")
}

func renderCompactionBlock(entry Entry, width int, expanded bool) string {
	summary := ansi.Truncate(compactionSummaryLabel(entry.TokensBefore, entry.TokensAfter), max(0, width), "")
	if !expanded || entry.Rendered == "" {
		return summary
	}
	return summary + "\n\n" + entry.Rendered
}

func renderToolBlock(entry Entry, width int, expanded bool) string {
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
	args := escapePlainText(entry.ToolArgs)
	preview := toolArgumentPreview(entry.ToolName, entry.ToolArgs)
	if entry.ToolError && entry.ToolOutput != "" {
		preview += " — " + strings.Join(strings.Fields(escapePlainText(entry.ToolOutput)), " ")
	}
	summary := renderToolSummary(name, preview, status, max(1, min(118, width-2)))
	if !expanded {
		return summary
	}
	lines := []string{summary}
	if args != "" {
		lines = append(lines, "", "Arguments:", args)
	}
	if output := escapePlainText(entry.ToolOutput); output != "" {
		lines = append(lines, "", "Output:", output)
	}
	return strings.Join(lines, "\n")
}

func toolArgumentPreview(name, raw string) string {
	if name == "bash" {
		var args struct {
			Command string `json:"command"`
		}
		if json.Unmarshal([]byte(raw), &args) == nil && args.Command != "" {
			return escapePlainText(args.Command)
		}
	}
	return escapePlainText(raw)
}

func renderToolSummary(name, args, status string, width int) string {
	if width <= 0 {
		return ""
	}
	marker := "…"
	if status == "complete" {
		marker = "✓"
	} else if status == "error" {
		marker = "✗"
	}
	prefix := marker + " "
	minimum := prefix + name
	if ansi.StringWidth(minimum) > width {
		nameWidth := max(1, width-ansi.StringWidth(prefix))
		name = ansi.Truncate(name, nameWidth, "…")
	}

	base := prefix + name + " "
	remaining := width - ansi.StringWidth(base)
	preview := strings.Join(strings.Fields(args), " ")
	if preview == "" {
		return strings.TrimRight(base, " ")
	}
	if remaining > 1 {
		base += " " + ansi.Truncate(preview, remaining-1, "…")
	}
	return ansi.Truncate(base, width, "")
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

func entriesAndUsageFromBackend(backend app.Backend) ([]Entry, otmodel.Usage) {
	entries, fallback := EntriesFromHistory(historyFromBackend(backend))
	info := infoFromBackend(backend)
	if !info.UsagePresent {
		return entries, fallback
	}
	aggregate := addUsageTotals(otmodel.Usage{}, &info.Usage)
	return entries, aggregate
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

func (m *Model) registerActiveOperation(stream *turnStream, cancel context.CancelFunc) {
	if m.operationCleanup == nil {
		m.operationCleanup = newOperationCleanup()
	}
	m.activeOperation = m.operationCleanup.register(stream, cancel)
}

func (m *Model) abandonActiveTurn() {
	if m.activeOperation != nil {
		m.operationCleanup.abandon(m.activeOperation)
		return
	}
	if m.cancel != nil {
		m.cancel()
	}
	if m.activeTurnStream != nil {
		m.activeTurnStream.abandon()
	}
}

func (m Model) reservedStateActive() bool {
	return m.running || m.newSessionPending || m.resume.active() || m.resume.listPending || m.dirtyStreaming || m.renderTickActive || m.cancel != nil || m.activeTurnStream != nil || m.ctrlCArmed || m.fatalErr != nil
}

// boxedInput renders the composer as a bordered panel that anchors the screen
// as its primary area. The bold label row and rounded border distinguish the
// input from the transcript and the footer below it.
func boxedInput(width, editorHeight int, editorView string) string {
	label := lipgloss.NewStyle().Bold(true).Render("Ask Otto")
	body := label + "\n" + editorView
	return lipgloss.NewStyle().
		Width(width).
		Height(editorHeight+inputBoxBorder+inputBoxLabel).
		Border(lipgloss.RoundedBorder()).
		Padding(0, inputBoxPadding).
		Render(body)
}
