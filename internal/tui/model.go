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
	"github.com/baiyuqing/otto/internal/subagent"
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
	rootCtx                 context.Context
	backend                 app.Backend
	entries                 []Entry
	viewport                viewport.Model
	editor                  textarea.Model
	spinner                 spinner.Model
	keymap                  KeyMap
	width                   int
	height                  int
	usage                   otmodel.Usage
	running                 bool
	expandedDetails         bool
	overlay                 overlayKind
	renderer                MarkdownRenderer
	rendererInjected        bool
	darkBackground          bool
	clock                   Clock
	statusText              string
	supportsModifiedEnter   bool
	dirtyStreaming          bool
	renderTickActive        bool
	cancel                  context.CancelFunc
	operationCleanup        *operationCleanup
	activeOperation         *activeOperation
	ctrlCArmed              bool
	ctrlCArmedAt            time.Time
	ctrlCArmGeneration      uint64
	newSessionPending       bool
	newSessionGeneration    uint64
	profileSwitchPending    bool
	profileSwitchGeneration uint64
	pendingCanceledTasks    int
	resume                  resumePickerState
	archive                 archivePickerState
	profilePicker           profilePickerState
	memoryGeneration        uint64
	loginPending            bool
	loginGeneration         uint64
	loginChannel            chan loginResultMsg
	activeTurnStream        *turnStream
	activeTurnChannel       <-chan turnEnvelope
	activeAssistant         int
	turnErrorSeen           bool
	turnEventErr            error
	fatalErr                error
	turnGeneration          uint64
	operationKind           operationKind
	compactionCompleted     bool
	turnHistoryBaseline     turnHistoryBaseline
	turnEntryStart          int
	liveEntrySequence       int
	commandSuggestionIndex  int
	promptHistory           []string
	promptHistoryIndex      int
	promptDraft             string
	turnStartedAt           time.Time
	turnDuration            time.Duration
	committed               int
	committedAssistantTurn  bool
	pendingPrints           []string
	printInFlight           bool
	bannerShown             bool
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
	vp.SoftWrap = true
	// The transcript viewport now renders only the live region (the
	// in-progress turn); it must never consume typing keys. Its default
	// keymap binds space (and f/j/k/u/d/b/h/l) to scrolling, which would fire
	// while the composer is focused because unhandled key presses are
	// forwarded to the viewport before the editor.
	vp.KeyMap = viewport.KeyMap{}

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
	// commitFinalEntries (inside rerenderAndRefreshViewportContent) is gated
	// on m.width > 0, so at construction time (before the first
	// tea.WindowSizeMsg) this call is a no-op for committing; it only sizes
	// the initial viewport content.
	model.rerenderAndRefreshViewportContent()
	return model
}

func (m Model) Init() tea.Cmd {
	return tea.Batch(tea.RequestBackgroundColor, m.spinner.Tick, m.armTaskUpdates())
}

// Update dispatches msg and then, regardless of which branch handled it,
// gives the print-ordering state machine a chance to send the next queued
// scrollback chunk. Bubble Tea dispatches each returned tea.Cmd on its own
// goroutine, so a tea.Println queued by one Update call is not guaranteed to
// reach the terminal before a later one; centralizing the flush here means
// every internal helper that appends to entries can stay a plain void method
// and this is the only place that has to remember to drain pendingPrints.
func (m Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	next, cmd := m.dispatch(msg)
	updated, ok := next.(Model)
	if !ok {
		return next, cmd
	}
	return updated, tea.Batch(cmd, updated.flushNextPrintCmd())
}

func (m Model) dispatch(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.BackgroundColorMsg:
		m.darkBackground = msg.IsDark()
		if !m.rendererInjected {
			m.renderer = newGlamourRenderer(m.darkBackground)
			m.invalidateMarkdownRenders()
			m.rerenderAndRefreshViewportContent()
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
		if len(m.archive.sessions) == 0 {
			m.archive.selected = 0
		} else {
			m.archive.selected = clamp(m.archive.selected, 0, len(m.archive.sessions)-1)
		}
		if len(m.profilePicker.profiles) == 0 {
			m.profilePicker.selected = 0
		} else {
			m.profilePicker.selected = clamp(m.profilePicker.selected, 0, len(m.profilePicker.profiles)-1)
		}
		m.queueBannerIfEmpty()
		m.rerenderAndRefreshViewportContent()
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
		m.refreshViewportContent()
		return m, nil
	case commitFlushedMsg:
		m.printInFlight = false
		return m, nil
	case newSessionResultMsg:
		return m.applyNewSessionResult(msg)
	case profileSwitchResultMsg:
		return m.applyProfileSwitchResult(msg)
	case sessionListResultMsg:
		if m.resume.listPending && msg.generation == m.resume.generation {
			return m.applySessionListResult(msg)
		}
		if m.archive.listPending && msg.generation == m.archive.generation {
			return m.applyArchiveListResult(msg)
		}
		return m, nil
	case sessionResumeResultMsg:
		return m.applySessionResumeResult(msg)
	case archiveSessionResultMsg:
		return m.applyArchiveSessionResult(msg)
	case memoryCommandResultMsg:
		return m.applyMemoryCommandResult(msg)
	case loginResultMsg:
		return m.applyLoginResult(msg)
	case ctrlCArmExpiredMsg:
		if m.ctrlCArmed && msg.generation == m.ctrlCArmGeneration {
			m.clearCtrlCArm()
		}
		return m, nil
	case turnMsg:
		return m.updateTurn(msg)
	case taskUpdateMsg:
		// The sole re-arm point for the task-update wait: every taskUpdateMsg,
		// including a closed:true from a registry swapped out by a session
		// replacement, re-arms here. Session-replacement result handlers
		// (applyNewSessionResult, applyProfileSwitchResult,
		// applySessionResumeResult, reconcileCommittedStaleResume,
		// applyArchiveSessionResult) must not call armTaskUpdates themselves,
		// or every replacement leaves an extra goroutine waiting on the same
		// channel.
		next, cmd := m.maybeWake()
		return next, tea.Batch(cmd, next.armTaskUpdates())
	case renderStreamingMsg:
		if msg.generation != m.turnGeneration || !m.running {
			return m, nil
		}
		m.renderTickActive = false
		if m.dirtyStreaming && m.renderActiveAssistantEntry() {
			m.dirtyStreaming = false
			m.refreshViewportContent()
		}
		return m, nil
	case tea.KeyPressMsg:
		return m.handleKeyPress(msg)
	case tea.PasteMsg:
		if m.resume.active() || m.archive.active() || m.overlay != overlayNone {
			return m, nil
		}
	case tea.MouseMsg:
		if m.resume.active() || m.archive.active() || m.overlay != overlayNone {
			return m, nil
		}
	}

	return m.updateComponents(msg)
}

func (m Model) updateComponents(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd
	previousEditorValue := m.editor.Value()
	previousEditorHeight := m.editor.Height()
	previousSuggestionCount := len(m.commandSuggestions())
	m.viewport, _ = m.viewport.Update(msg)
	m.editor, cmd = m.editor.Update(msg)
	if m.editor.Value() != previousEditorValue {
		m.commandSuggestionIndex = 0
	}
	m.spinner, _ = m.spinner.Update(msg)

	if m.editor.Height() != previousEditorHeight || len(m.commandSuggestions()) != previousSuggestionCount {
		m.rerenderAndRefreshViewportContent()
	}

	return m, cmd
}

func (m Model) View() tea.View {
	_ = m.reservedStateActive()

	layout := calculateLayout(m.width, m.height, m.editor, 0, m.liveLines(), m.taskPanelLines())
	if layout.tooSmall {
		return newRootView(m, smallTerminalView(m.width, m.height))
	}
	if m.resume.active() {
		return newRootView(m, renderResumePicker(m.width, m.height, m.resume, m.spinner.View(), m.now()))
	}
	if m.archive.active() {
		return newRootView(m, renderArchivePicker(m.width, m.height, m.archive, m.spinner.View(), m.now()))
	}
	if m.profilePicker.active() {
		return newRootView(m, renderProfilePicker(m.width, m.height, m.profilePicker, infoFromBackend(m.backend)))
	}

	transcript := lipgloss.NewStyle().Width(layout.transcriptWidth).Height(layout.transcriptHeight).MaxHeight(layout.transcriptHeight).Render(m.viewport.View())
	footer := lipgloss.NewStyle().Width(m.width).Render(renderFooter(m.width, infoFromBackend(m.backend), m.usage, m.footerStatus()))
	parts := []string{transcript}
	if layout.taskLines > 0 {
		parts = append(parts, lipgloss.NewStyle().Width(m.width).Render(taskPanelContent(m.activeTasks(), m.now(), m.spinner.View(), m.width)))
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
		return newRootView(m, content)
	}
	return newRootViewWithOverlay(m, content, m.commandSuggestionOverlay(layout))
}

func (m Model) footerStatus() string {
	if !m.running {
		return m.statusText
	}
	elapsed := m.now().Sub(m.turnStartedAt)
	running := m.spinner.View() + " working"
	if elapsed >= 10*time.Second {
		running += " " + formatTurnSeconds(elapsed)
	}
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
		return helpOverlayContent(m.width, m.height, infoFromBackend(m.backend).Sandbox)
	case overlaySession:
		return sessionOverlayContent(m.width, m.height, infoFromBackend(m.backend))
	default:
		return ""
	}
}

func (m Model) handleKeyPress(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	previousEditorHeight := m.editor.Height()
	if isCtrlCKey(msg) {
		return m.handleCtrlC(previousEditorHeight)
	}
	if updated, cmd, handled := m.handleResumeKeyPress(msg); handled {
		return updated, cmd
	}
	if updated, cmd, handled := m.handleArchiveKeyPress(msg); handled {
		return updated, cmd
	}
	if updated, cmd, handled := m.handleProfilePickerKeyPress(msg); handled {
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
		m.refreshViewportContent()
		return m, nil
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
	return m.updateComponents(msg)
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
		m.rerenderAndRefreshViewportContent()
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
		m.rerenderAndRefreshViewportContent()
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

func (m Model) updateEditorWithKey(msg tea.KeyPressMsg, previousEditorHeight int) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd
	previousEditorValue := m.editor.Value()
	previousSuggestionCount := len(m.commandSuggestions())
	m.editor, cmd = m.editor.Update(msg)
	if m.editor.Value() != previousEditorValue {
		m.commandSuggestionIndex = 0
	}
	if m.editor.Height() != previousEditorHeight || len(m.commandSuggestions()) != previousSuggestionCount {
		m.rerenderAndRefreshViewportContent()
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
	return newRootViewWithOverlay(m, content, "")
}

func newRootViewWithOverlay(m Model, content, overlay string) tea.View {
	content = fitToBounds(content, m.width, m.height)
	if overlay != "" {
		content = lipgloss.NewCompositor(lipgloss.NewLayer(content), lipgloss.NewLayer(overlay).Z(1)).Render()
	}
	view := tea.NewView(content)
	view.AltScreen = false
	view.MouseMode = tea.MouseModeNone
	view.KeyboardEnhancements.ReportEventTypes = false
	view.KeyboardEnhancements.ReportAlternateKeys = true
	layout := calculateLayout(m.width, m.height, m.editor, 0, m.liveLines(), m.taskPanelLines())
	if !layout.tooSmall && !m.resume.active() && !m.archive.active() && !m.profilePicker.active() && m.overlay == overlayNone {
		if cursor := m.editor.Cursor(); cursor != nil {
			cursor.Y += 1 + layout.transcriptHeight + layout.taskLines + layout.editorSpacing
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

func (m Model) commandSuggestionOverlay(layout layoutState) string {
	suggestions := m.commandSuggestions()
	if len(suggestions) == 0 || m.width <= 0 || m.height <= 0 || layout.tooSmall {
		return ""
	}
	availableRows := m.height - layout.footerHeight - layout.inputBoxHeight - layout.editorSpacing
	if layout.taskLines > 0 {
		availableRows -= layout.taskLines
	}
	height := min(len(suggestions), max(0, availableRows))
	if height <= 0 {
		return ""
	}
	panel := renderCommandSuggestions(m.width, suggestions, m.commandSuggestionIndex, height)
	if panel == "" {
		return ""
	}
	return fitToBounds(panel, m.width, m.height)
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
		m.statusText = app.ErrPromptActive.Error()
		return m, nil
	}
	m.addPromptHistory(trimmed)
	return m.startPrompt(prompt)
}

func (m Model) handleCommand(value string) (tea.Model, tea.Cmd) {
	command, argument, ok := parseSlashCommand(value)
	argumentAllowed := command.Kind == slashCommandCompact || command.Kind == slashCommandMemory || command.Kind == slashCommandRemember || command.Kind == slashCommandLogin || command.Kind == slashCommandModel || command.Kind == slashCommandTask
	if !ok || (argument != "" && !argumentAllowed) {
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
		m.pendingCanceledTasks = m.countActiveTasks()
		m.statusText = ""
		return m, runNewSessionCommand(m.backend, m.newSessionGeneration)
	case slashCommandModel:
		return m.handleModelCommand(argument)
	case slashCommandResume:
		return m.handleResumeCommand()
	case slashCommandArchive:
		return m.handleArchiveCommand()
	case slashCommandCompact:
		if m.running || m.newSessionPending {
			m.statusText = app.ErrPromptActive.Error()
			return m, nil
		}
		return m.startCompaction(argument)
	case slashCommandMemory:
		return m.handleMemoryCommand(argument)
	case slashCommandRemember:
		return m.handleRememberCommand(argument)
	case slashCommandLogin:
		return m.handleLoginCommand(argument)
	case slashCommandLogout:
		return m.handleLogoutCommand()
	case slashCommandExit:
		if m.running {
			return m, nil
		}
		return m.quit()
	case slashCommandTasks:
		return m.handleTasksCommand()
	case slashCommandTask:
		return m.handleTaskCommand(argument)
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
	m.noteCanceledTasks(m.pendingCanceledTasks)
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
	m.loginPending = false
	m.loginChannel = nil
	m.profileSwitchPending = false
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
	m.editor.SetValue("")
	m.commandSuggestionIndex = 0
	m.committed = 0
	m.committedAssistantTurn = false
	m.bannerShown = false
	m.queueBannerIfEmpty()
	m.rerenderAndRefreshViewportContent()
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
			m.rerenderAndRefreshViewportContent()
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
		m.rerenderAndRefreshViewportContent()
	}
}

func (m Model) startPrompt(text string) (tea.Model, tea.Cmd) {
	return m.startTurn(text, true)
}

// startTurn sets up turn state and starts the provider call, shared by a
// user-submitted prompt and a sub-agent wake turn. When appendUserEntry is
// false (a wake turn), no EntryUser is appended and the editor is left
// untouched, so text the user was composing survives the turn.
func (m Model) startTurn(text string, appendUserEntry bool) (Model, tea.Cmd) {
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
	if appendUserEntry {
		m.entries = append(m.entries, Entry{ID: m.nextLiveEntryID("user"), Kind: EntryUser, Raw: text})
		m.editor.SetValue("")
		m.commandSuggestionIndex = 0
	}
	m.rerenderAndRefreshViewportContent()

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
	if event.Plan != nil {
		plan := *event.Plan
		cloned.Plan = &plan
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
			m.refreshViewportContent()
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
		m.refreshViewportContent()
		return m, waitTurn(stream)
	case agent.EventToolCallFinished:
		m.finalizeStreamingRender()
		m.activeAssistant = -1
		m.finishToolEntry(event)
		m.refreshViewportContent()
		return m, waitTurn(stream)
	case agent.EventProviderUsage:
		m.usage = addUsageTotals(m.usage, &event.Usage)
		return m, waitTurn(stream)
	case agent.EventNotification:
		m.finalizeStreamingRender()
		m.activeAssistant = -1
		m.entries = append(m.entries, Entry{
			ID:   m.nextLiveEntryID("notification"),
			Kind: EntrySystem,
			Raw:  notificationEntryText(event.TaskID, event.Text),
		})
		m.usage = addUsageTotals(m.usage, &event.Usage)
		m.refreshViewportContent()
		return m, waitTurn(stream)
	case agent.EventAgentError:
		m.recordTurnError(event.Err)
		return m, waitTurn(stream)
	case agent.EventCompactionStarted, agent.EventCompactionPlanned, agent.EventCompactionCompleted, agent.EventCompactionWarning:
		if m.applyCompactionEvent(event, envelope.aggregateUsage, envelope.aggregateUsagePresent) {
			m.refreshViewportContent()
		}
		return m, waitTurn(stream)
	case agent.EventMemoryWarning:
		if event.Err != nil {
			m.statusText = event.Err.Error()
		} else {
			m.statusText = "memory recall warning"
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
	if m.operationKind == operationCompact && !m.compactionCompleted && envelope.compactionResult != nil && (envelope.compactionResult.Noop || envelope.compactionResult.CheckpointID != "" || err == nil) {
		m.applyCompactionResult(*envelope.compactionResult, envelope.aggregateUsage, envelope.aggregateUsagePresent)
	}
	if err == nil {
		err = m.turnEventErr
	}
	m.finalizeStreamingRender()
	m.reconcilePendingTools(err)
	if err != nil && !m.turnErrorSeen {
		m.recordTurnError(err)
	}
	m.completeTurnState()
	m.refreshViewportContent()
	if errors.Is(err, session.ErrFatalPersistence) {
		m.fatalErr = err
		return m.quit()
	}
	return m.maybeWake()
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
	m.rerenderAndRefreshViewportContent()
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
	m.refreshViewportContent()
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

func (m *Model) rerenderAndRefreshViewportContent() {
	m.renderEntries(m.transcriptWidth())
	m.refreshViewportContent()
}

func (m Model) transcriptWidth() int {
	return max(1, calculateLayout(m.width, m.height, m.editor, 0, 0, 0).transcriptWidth)
}

func (m *Model) refreshViewportContent() {
	width := m.transcriptWidth()
	m.commitFinalEntries(width)
	content, _ := renderTranscript(m.entries[m.committed:], m.committedAssistantTurn, width, m.expandedDetails, m.darkBackground)
	layout := calculateLayout(m.width, m.height, m.editor, 0, lineCount(content), m.taskPanelLines())
	editorWidth := m.width
	if layout.inputBoxed {
		editorWidth = max(1, m.width-inputBoxPadding*2-inputBoxBorder)
	}
	m.editor.SetWidth(max(0, editorWidth))
	m.editor.SetHeight(layout.editorHeight)
	m.viewport.SetWidth(layout.transcriptWidth)
	m.viewport.SetHeight(max(1, layout.transcriptHeight))
	m.viewport.SetContent(content)
	m.viewport.GotoBottom()
}

// commitFinalEntries advances m.committed past every entry that has become
// final since the last commit, queuing the newly-final block for printing to
// the terminal's native scrollback via the print-ordering state machine, then
// always attempts to flush pendingPrints (so an externally queued item, like
// the empty-session banner, is not stranded).
//
// ponytail: gated on m.width > 0 so a commit can never fire before the first
// WindowSizeMsg sets a usable width; upgrade only if Otto ever needs to print
// before terminal size is known.
func (m *Model) commitFinalEntries(width int) {
	if m.width > 0 {
		end := m.committed
		for end < len(m.entries) && isEntryFinal(m.entries, end, m.activeAssistant, m.running) {
			end++
		}
		if end != m.committed {
			chunk, nextAssistantTurn := renderTranscript(m.entries[m.committed:end], m.committedAssistantTurn, width, m.expandedDetails, m.darkBackground)
			m.committed = end
			m.committedAssistantTurn = nextAssistantTurn
			m.queuePrint(chunk)
		}
	}
}

// queuePrint appends chunk to pendingPrints with a trailing newline so
// tea.Println leaves one blank line after it, matching the "\n\n" block
// separator used inside a chunk.
func (m *Model) queuePrint(chunk string) {
	if chunk != "" {
		m.pendingPrints = append(m.pendingPrints, chunk+"\n")
	}
}

// flushNextPrintCmd sends the next queued scrollback chunk, if one is queued
// and none is currently in flight. Called unconditionally by Update after
// every dispatch, so every internal helper that appends to pendingPrints can
// stay a plain void method.
func (m *Model) flushNextPrintCmd() tea.Cmd {
	if m.printInFlight || len(m.pendingPrints) == 0 {
		return nil
	}
	chunk := m.pendingPrints[0]
	m.pendingPrints = m.pendingPrints[1:]
	m.printInFlight = true
	return tea.Sequence(tea.Println(chunk), func() tea.Msg { return commitFlushedMsg{} })
}

func (m *Model) renderEntries(width int) {
	for i := m.committed; i < len(m.entries); i++ {
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

// transcriptContent renders the full entry list at once; kept for tests that
// exercise rendering independent of the commit/live split.
func (m Model) transcriptContent(width int) string {
	if len(m.entries) == 0 && !m.running {
		return emptyTranscriptHint(width)
	}
	content, _ := renderTranscript(m.entries, false, width, m.expandedDetails, m.darkBackground)
	return content
}

// renderTranscript renders a slice of entries into the block-joined
// transcript text shared by the committed (printed to scrollback) and live
// (viewport) regions. assistantTurn carries the "Otto" header grouping state
// across a commit boundary; the returned bool is that state after processing
// entries.
func renderTranscript(entries []Entry, assistantTurn bool, width int, expandedDetails, darkBackground bool) (string, bool) {
	blocks := make([]string, 0, len(entries))
	for _, entry := range entries {
		if entry.Kind == EntryUser {
			assistantTurn = false
			blocks = append(blocks, renderUserBlock(entry.Rendered, width, darkBackground))
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
				blocks = append(blocks, indentToolBlock(renderToolBlock(entry, width, expandedDetails, darkBackground), width))
			} else {
				blocks = append(blocks, entry.Rendered)
			}
			continue
		}
		assistantTurn = false
		blocks = append(blocks, renderEntry(entry, width, expandedDetails, darkBackground))
	}
	return strings.Join(blocks, "\n\n"), assistantTurn
}

// isEntryFinal reports whether entries[index] is done changing and may be
// committed (printed to scrollback): User/System/Error entries always are;
// Tool entries once ToolDone; Assistant entries once no longer the streaming
// entry; Compaction entries once the turn has stopped running.
func isEntryFinal(entries []Entry, index int, activeAssistant int, running bool) bool {
	entry := entries[index]
	switch entry.Kind {
	case EntryUser, EntrySystem, EntryError:
		return true
	case EntryTool:
		return entry.ToolDone
	case EntryAssistant:
		return index != activeAssistant
	case EntryCompaction:
		return !running
	default:
		return true
	}
}

func lineCount(s string) int {
	if s == "" {
		return 0
	}
	return strings.Count(s, "\n") + 1
}

// liveLines reports the rendered line count of the not-yet-committed tail of
// m.entries, i.e. how tall the live region needs to be. Idle or fully
// committed sessions have no live region.
func (m Model) liveLines() int {
	if m.committed >= len(m.entries) {
		return 0
	}
	content, _ := renderTranscript(m.entries[m.committed:], m.committedAssistantTurn, m.transcriptWidth(), m.expandedDetails, m.darkBackground)
	return lineCount(content)
}

// queueBannerIfEmpty queues the empty-session logo/hint text directly into
// pendingPrints (bypassing the live region) the first time the transcript is
// observed empty. bannerShown latches so the banner is only queued once per
// session; resetSessionViewFromBackend clears the latch so /new re-shows it.
func (m *Model) queueBannerIfEmpty() {
	if m.bannerShown || len(m.entries) != 0 {
		return
	}
	m.bannerShown = true
	m.queuePrint(emptyTranscriptHint(m.transcriptWidth()))
}

const logo = "     ____  __  __\n    / __ \\/ /_/ /____\n   / /_/ / __/ __/ __ \\\n   \\____/\\__/\\__/\\____/"
const emptyTranscriptHintText = "Ask Otto anything. Type /help for commands or /resume to continue a session."

func emptyTranscriptHint(width int) string {
	var sb strings.Builder
	for _, line := range strings.Split(logo, "\n") {
		sb.WriteString(clipSingleLineText(line, width))
		sb.WriteByte('\n')
	}
	sb.WriteString(clipSingleLineText(emptyTranscriptHintText, width))
	return sb.String()
}

func renderEntry(entry Entry, width int, expandedDetails, darkBackground bool) string {
	switch entry.Kind {
	case EntryUser:
		return renderMessageBlock("You", entry.Rendered)
	case EntryAssistant:
		return renderMessageBlock("Otto", entry.Rendered)
	case EntryTool:
		return renderToolBlock(entry, width, expandedDetails, darkBackground)
	case EntryCompaction:
		return renderCompactionBlock(entry, width, expandedDetails)
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
			lines = append(lines, ansi.Truncate("│ "+part, max(0, width), ""))
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

func renderToolBlock(entry Entry, width int, expanded bool, dark bool) string {
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
	summary := renderToolSummary(name, preview, status, max(1, min(118, width-2)), dark)
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
	var fields struct {
		Command     string `json:"command"`
		Path        string `json:"path"`
		FilePath    string `json:"file_path"`
		Pattern     string `json:"pattern"`
		Glob        string `json:"glob"`
		Query       string `json:"query"`
		OldString   string `json:"old_string"`
		NewString   string `json:"new_string"`
		Prompt      string `json:"prompt"`
		Description string `json:"description"`
		Name        string `json:"name"`
		TaskID      string `json:"task_id"`
	}
	if json.Unmarshal([]byte(raw), &fields) != nil {
		return escapePlainText(raw)
	}
	path := fields.Path
	if path == "" {
		path = fields.FilePath
	}

	switch name {
	case "bash":
		if fields.Command != "" {
			return escapePlainText(fields.Command)
		}
	case "read", "write", "ls":
		if path != "" {
			return escapePlainText(path)
		}
	case "grep":
		parts := make([]string, 0, 3)
		if fields.Pattern != "" {
			parts = append(parts, fields.Pattern)
		}
		if path != "" && path != "." {
			parts = append(parts, path)
		}
		if fields.Glob != "" {
			parts = append(parts, fields.Glob)
		}
		if len(parts) > 0 {
			return escapePlainText(strings.Join(parts, " "))
		}
	case "find":
		parts := make([]string, 0, 2)
		if fields.Pattern != "" {
			parts = append(parts, fields.Pattern)
		}
		if path != "" && path != "." {
			parts = append(parts, "in "+path)
		}
		if len(parts) > 0 {
			return escapePlainText(strings.Join(parts, " "))
		}
	case "edit":
		parts := make([]string, 0, 2)
		if path != "" {
			parts = append(parts, path)
		}
		if fields.OldString != "" {
			preview := strings.Join(strings.Fields(fields.OldString), " ")
			if len(preview) > 40 {
				preview = preview[:40] + "…"
			}
			parts = append(parts, preview+" → …")
		}
		if len(parts) > 0 {
			return escapePlainText(strings.Join(parts, " "))
		}
	case "memory_search":
		if fields.Query != "" {
			return escapePlainText(fields.Query)
		}
	case "agent":
		return escapePlainText(subagent.TaskLabel(agent.Task{Name: fields.Name, Description: fields.Description, Prompt: fields.Prompt}))
	case "agent_wait":
		if fields.TaskID != "" {
			return escapePlainText(fields.TaskID)
		}
		return escapePlainText("all")
	case "agent_send":
		if fields.TaskID != "" {
			return escapePlainText(fields.TaskID)
		}
	}
	return escapePlainText(raw)
}

func renderToolSummary(name, args, status string, width int, dark bool) string {
	if width <= 0 {
		return ""
	}
	marker := "…"
	switch status {
	case "complete":
		marker = "✓"
	case "error":
		marker = "✗"
	}

	prefix := marker + " "
	prefixWidth := 2 // marker + space
	if prefixWidth+ansi.StringWidth(name) > width {
		nameWidth := max(1, width-prefixWidth)
		name = ansi.Truncate(name, nameWidth, "…")
	}

	base := prefix + name
	baseWidth := prefixWidth + ansi.StringWidth(name)
	preview := strings.Join(strings.Fields(args), " ")
	if preview != "" {
		remaining := width - baseWidth - 2 // "  " separator
		if remaining > 1 {
			base += "  " + ansi.Truncate(preview, remaining, "…")
		}
	}

	bg := "254"
	if dark {
		bg = "237"
	}
	lineStyle := lipgloss.NewStyle().Background(lipgloss.Color(bg))
	if status == "running" {
		lineStyle = lineStyle.Faint(true)
	}
	return lineStyle.Width(width).MaxWidth(width).MaxHeight(1).Render(base)
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
	return m.running || m.newSessionPending || m.resume.active() || m.resume.listPending || m.archive.active() || m.archive.listPending || m.dirtyStreaming || m.renderTickActive || m.cancel != nil || m.activeTurnStream != nil || m.ctrlCArmed || m.fatalErr != nil
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
