package tui

import (
	"context"
	"strings"
	"time"

	"charm.land/bubbles/v2/spinner"
	"charm.land/bubbles/v2/textarea"
	"charm.land/bubbles/v2/viewport"
	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
	"github.com/baiyuqing/otto/internal/app"
	otmodel "github.com/baiyuqing/otto/internal/model"
)

type Option func(*Model)

func WithRenderer(renderer MarkdownRenderer) Option {
	return func(model *Model) {
		model.renderer = renderer
	}
}

type Model struct {
	rootCtx        context.Context
	backend        app.Backend
	entries        []Entry
	viewport       viewport.Model
	editor         textarea.Model
	spinner        spinner.Model
	keymap         KeyMap
	width          int
	height         int
	usage          otmodel.Usage
	running        bool
	expandedTools  bool
	overlay        overlayKind
	autoFollow     bool
	renderer       MarkdownRenderer
	dirtyStreaming bool
	cancel         context.CancelFunc
	ctrlCArmedAt   time.Time
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
		rootCtx:    ctx,
		backend:    backend,
		entries:    entries,
		viewport:   vp,
		editor:     editor,
		spinner:    spinner.New(spinner.WithSpinner(spinner.MiniDot)),
		keymap:     DefaultKeyMap(),
		usage:      usage,
		autoFollow: true,
		renderer:   GlamourRenderer{},
	}
	for _, option := range options {
		option(&model)
	}
	model.refreshViewportContent(false)
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
		m.refreshViewportContent(!m.autoFollow)
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
	case tea.KeyPressMsg:
		if msg.String() == "enter" {
			return m, nil
		}
	}

	var cmd tea.Cmd
	m.viewport, _ = m.viewport.Update(msg)
	m.editor, cmd = m.editor.Update(msg)
	m.spinner, _ = m.spinner.Update(msg)
	m.syncAutoFollow(viewportBefore)

	if m.editor.Height() != previousEditorHeight {
		m.refreshViewportContent(!m.autoFollow)
	} else if m.viewport.YOffset() != previousYOffset && !m.viewport.AtBottom() {
		m.autoFollow = false
	} else if m.viewport.AtBottom() {
		m.autoFollow = true
	}

	return m, cmd
}

func (m Model) View() tea.View {
	// Reserved state is wired in later tasks; keep it referenced so static
	// analysis matches the staged implementation plan.
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

func (m *Model) refreshViewportContent(preserveOffset bool) {
	m.editor.SetWidth(max(0, m.width))
	layout := calculateLayout(m.width, m.height, m.editor)
	m.editor.SetHeight(layout.editorHeight)
	m.viewport.SetWidth(layout.transcriptWidth)
	m.viewport.SetHeight(max(1, layout.transcriptHeight))

	transcriptWidth := max(1, layout.transcriptWidth)
	m.renderEntries(transcriptWidth)
	content := m.transcriptContent(transcriptWidth)
	previousYOffset := m.viewport.YOffset()
	m.viewport.SetContent(content)
	if m.autoFollow && !preserveOffset {
		m.viewport.GotoBottom()
		return
	}
	m.viewport.SetYOffset(previousYOffset)
}

func (m *Model) renderEntries(width int) {
	for i := range m.entries {
		entry := &m.entries[i]
		switch entry.Kind {
		case EntryAssistant:
			if entry.RenderWidth == width && (entry.Rendered != "" || entry.Raw == "") {
				continue
			}
			rendered, _ := renderMarkdown(m.renderer, entry.Raw, width)
			entry.Rendered = rendered
			entry.RenderWidth = width
		case EntryTool:
			entry.RenderWidth = width
		default:
			if entry.RenderWidth == width && (entry.Rendered != "" || entry.Raw == "") {
				continue
			}
			entry.Rendered = escapePlainText(entry.Raw)
			entry.RenderWidth = width
		}
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

func (m Model) reservedStateActive() bool {
	return m.running || m.dirtyStreaming || m.cancel != nil || !m.ctrlCArmedAt.IsZero()
}
