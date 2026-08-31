package repl

import (
	"context"
	"errors"
	"fmt"
	"io"
	"strings"
	"sync"

	"charm.land/bubbles/v2/key"
	"charm.land/bubbles/v2/textarea"
	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/command"
	"github.com/baiyuqing/otto/internal/render"
	"github.com/baiyuqing/otto/internal/session"
)

const inlineTurnChannelCap = 256

type inlineModel struct {
	rootCtx  context.Context
	backend  app.Backend
	editor   textarea.Model
	renderer render.MarkdownRenderer
	width    int

	running   bool
	cancel    context.CancelFunc
	turnCh    <-chan inlineTurnEnvelope
	streamBuf string
	turnErr   error

	history      []string
	historyIndex int
	historyDraft string

	suggestions     []command.Command
	suggestionIndex int

	fatalErr error
	exitReq  bool
}

type inlineTurnEnvelope struct {
	event *agent.Event
	err   error
	done  bool
}

type inlineTurnMsg inlineTurnEnvelope

func newInlineModel(ctx context.Context, backend app.Backend) inlineModel {
	editor := textarea.New()
	editor.Prompt = "> "
	editor.ShowLineNumbers = false
	editor.CharLimit = maxInputBytes
	editor.SetHeight(1)
	editor.MinHeight = 1
	editor.MaxHeight = 10
	editor.DynamicHeight = true
	editor.KeyMap.InsertNewline = key.NewBinding(key.WithKeys("alt+enter"))
	_ = editor.Focus()

	return inlineModel{
		rootCtx:      ctx,
		backend:      backend,
		editor:       editor,
		renderer:     render.NewGlamourRenderer(true),
		historyIndex: -1,
	}
}

func (m inlineModel) Init() tea.Cmd {
	cmds := []tea.Cmd{tea.Println(strings.TrimRight(logo, "\n"))}
	if info := m.backend.Info(); info.SessionID != "" {
		cmds = append(cmds, tea.Println("Session: "+info.SessionID))
	}
	return tea.Batch(cmds...)
}

func (m inlineModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.width = msg.Width
		m.editor.SetWidth(msg.Width)
		return m, nil

	case tea.KeyPressMsg:
		return m.handleKey(msg)

	case inlineTurnMsg:
		return m.handleTurnEvent(msg)
	}

	if !m.running {
		var cmd tea.Cmd
		m.editor, cmd = m.editor.Update(msg)
		return m, cmd
	}
	return m, nil
}

func (m inlineModel) handleKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	if msg.String() == "ctrl+c" {
		if m.running && m.cancel != nil {
			m.cancel()
			return m, nil
		}
		m.exitReq = true
		return m, tea.Quit
	}

	if m.running {
		return m, nil
	}

	if len(m.suggestions) > 0 {
		switch msg.Key().Code {
		case tea.KeyUp:
			m.suggestionIndex = (m.suggestionIndex - 1 + len(m.suggestions)) % len(m.suggestions)
			return m, nil
		case tea.KeyDown:
			m.suggestionIndex = (m.suggestionIndex + 1) % len(m.suggestions)
			return m, nil
		case tea.KeyEscape:
			m.suggestions = nil
			m.suggestionIndex = 0
			return m, nil
		case tea.KeyTab, tea.KeyEnter:
			if msg.Key().Mod == 0 {
				return m.acceptSuggestion()
			}
		}
	}

	if msg.Key().Code == tea.KeyEnter && msg.Key().Mod == 0 {
		return m.handleSubmit()
	}

	switch msg.Key().Code {
	case tea.KeyUp:
		if next, handled := m.handleHistoryUp(); handled {
			return next, nil
		}
	case tea.KeyDown:
		if next, handled := m.handleHistoryDown(); handled {
			return next, nil
		}
	}

	var cmd tea.Cmd
	m.editor, cmd = m.editor.Update(msg)
	m.refreshSuggestions()
	return m, cmd
}

// acceptSuggestion completes the editor value to the selected command,
// appending a trailing space for commands that take an argument.
func (m inlineModel) acceptSuggestion() (tea.Model, tea.Cmd) {
	selected := m.suggestions[m.suggestionIndex]
	value := selected.Name
	if commandTakesArgs(selected.Kind) {
		value += " "
	}
	m.editor.SetValue(value)
	m.suggestions = nil
	m.suggestionIndex = 0
	return m, nil
}

func commandTakesArgs(kind command.Kind) bool {
	switch kind {
	case command.KindResume, command.KindCompact, command.KindMemory, command.KindRemember:
		return true
	default:
		return false
	}
}

// refreshSuggestions recomputes the visible suggestion list from the current
// editor value, hiding it once the value is already an exact command match.
func (m *inlineModel) refreshSuggestions() {
	matches := command.Match(m.editor.Value())
	if len(matches) == 1 && matches[0].Name == m.editor.Value() {
		matches = nil
	}
	m.suggestions = matches
	if m.suggestionIndex >= len(m.suggestions) {
		m.suggestionIndex = 0
	}
}

// handleHistoryUp recalls the previous history entry when the cursor is on
// the editor's first line. It returns handled=false so the key falls through
// to normal cursor movement otherwise (e.g. multi-line editing, empty history).
func (m inlineModel) handleHistoryUp() (inlineModel, bool) {
	if len(m.history) == 0 || m.editor.Line() != 0 {
		return m, false
	}
	if m.historyIndex == -1 {
		m.historyDraft = m.editor.Value()
		m.historyIndex = len(m.history) - 1
	} else if m.historyIndex > 0 {
		m.historyIndex--
	}
	m.editor.SetValue(m.history[m.historyIndex])
	return m, true
}

// handleHistoryDown steps forward through history, or restores the saved
// draft once the newest entry is passed. It is a no-op when not browsing.
func (m inlineModel) handleHistoryDown() (inlineModel, bool) {
	if m.historyIndex == -1 {
		return m, false
	}
	if m.historyIndex < len(m.history)-1 {
		m.historyIndex++
		m.editor.SetValue(m.history[m.historyIndex])
	} else {
		m.historyIndex = -1
		m.editor.SetValue(m.historyDraft)
	}
	return m, true
}

func (m inlineModel) handleSubmit() (tea.Model, tea.Cmd) {
	value := strings.TrimSpace(m.editor.Value())
	if value == "" {
		return m, nil
	}
	m.editor.SetValue("")
	m.suggestions = nil
	m.suggestionIndex = 0

	if strings.HasPrefix(value, "/") {
		return m.handleCommand(value)
	}

	m.history = append(m.history, value)
	m.historyIndex = -1
	return m.startTurn(value)
}

func (m inlineModel) startTurn(text string) (tea.Model, tea.Cmd) {
	ctx, cancel := context.WithCancel(m.rootCtx)
	ch := make(chan inlineTurnEnvelope, inlineTurnChannelCap)
	m.running = true
	m.cancel = cancel
	m.turnCh = ch
	m.streamBuf = ""
	m.turnErr = nil

	go runInlineTurnWorker(ctx, m.backend, text, ch)

	return m, waitInlineTurn(ch)
}

func runInlineTurnWorker(ctx context.Context, backend app.Backend, text string, ch chan<- inlineTurnEnvelope) {
	defer close(ch)
	if backend == nil {
		ch <- inlineTurnEnvelope{err: errors.New("backend is required"), done: true}
		return
	}
	var mu sync.Mutex
	accepting := true
	err := backend.Prompt(ctx, text, func(event agent.Event) {
		mu.Lock()
		defer mu.Unlock()
		if !accepting {
			return
		}
		e := event
		ch <- inlineTurnEnvelope{event: &e}
	})
	mu.Lock()
	accepting = false
	mu.Unlock()
	ch <- inlineTurnEnvelope{err: err, done: true}
}

func waitInlineTurn(ch <-chan inlineTurnEnvelope) tea.Cmd {
	return func() tea.Msg {
		envelope, ok := <-ch
		if !ok {
			return inlineTurnMsg{done: true}
		}
		return inlineTurnMsg(envelope)
	}
}

func (m inlineModel) handleTurnEvent(msg inlineTurnMsg) (tea.Model, tea.Cmd) {
	if msg.done {
		return m.finishTurn(msg.err)
	}
	if msg.event == nil {
		return m, waitInlineTurn(m.turnCh)
	}

	event := *msg.event
	var cmds []tea.Cmd

	switch event.Type {
	case agent.EventTextDelta:
		m.streamBuf += event.Text
		cmds = append(cmds, m.flushCompleteLines()...)

	case agent.EventToolCallStarted:
		cmds = append(cmds, tea.Println(fmt.Sprintf("\n[tool] %s (%s)", event.ToolName, event.ToolCallID)))

	case agent.EventToolCallFinished:
		cmds = append(cmds, tea.Println(fmt.Sprintf("[tool result] %s", firstLine(event.ToolResult.Content))))

	case agent.EventCompactionCompleted:
		if event.Compaction != nil {
			cmds = append(cmds, tea.Println(compactionLine(agent.CompactionResult{
				CheckpointID:         event.Compaction.CheckpointID,
				TokensBefore:         event.Compaction.TokensBefore,
				EstimatedTokensAfter: event.Compaction.EstimatedTokensAfter,
				Noop:                 event.Compaction.Noop,
			})))
		}

	case agent.EventCompactionWarning:
		if event.Err != nil {
			cmds = append(cmds, tea.Println(event.Err.Error()))
		}

	case agent.EventMemoryWarning:
		if event.Err != nil {
			cmds = append(cmds, tea.Println(event.Err.Error()))
		}

	case agent.EventAgentError:
		if event.Err != nil {
			cmds = append(cmds, tea.Println(event.Err.Error()))
			m.turnErr = event.Err
		}
	}

	cmds = append(cmds, waitInlineTurn(m.turnCh))
	return m, tea.Batch(cmds...)
}

func (m *inlineModel) flushCompleteLines() []tea.Cmd {
	var cmds []tea.Cmd
	for {
		idx := strings.IndexByte(m.streamBuf, '\n')
		if idx < 0 {
			break
		}
		cmds = append(cmds, tea.Println(m.streamBuf[:idx]))
		m.streamBuf = m.streamBuf[idx+1:]
	}
	return cmds
}

func (m inlineModel) finishTurn(err error) (tea.Model, tea.Cmd) {
	m.running = false
	if m.cancel != nil {
		m.cancel()
		m.cancel = nil
	}

	var cmds []tea.Cmd

	if m.streamBuf != "" {
		cmds = append(cmds, tea.Println(m.streamBuf))
		m.streamBuf = ""
	}

	if err != nil && m.turnErr == nil {
		cmds = append(cmds, tea.Println(err.Error()))
	}

	cmds = append(cmds, tea.Println(""))

	if err != nil && errors.Is(err, session.ErrFatalPersistence) {
		m.fatalErr = err
		cmds = append(cmds, tea.Quit)
	}

	return m, tea.Batch(cmds...)
}

func (m inlineModel) handleCommand(input string) (tea.Model, tea.Cmd) {
	name, args, ok := splitCommand(input)
	if !ok {
		return m, tea.Println("unknown command: " + input)
	}

	switch name {
	case "exit":
		m.exitReq = true
		return m, tea.Quit

	case "help":
		return m, tea.Println(
			"/help     show commands\n" +
				"/exit     exit Otto\n" +
				"/new      start a new session\n" +
				"/session  show session details\n" +
				"/archive  archive current session and start a new one\n" +
				"/compact [focus] compact context\n" +
				"/memory search <query> | /memory forget <id> | /memory review <id> accept|reject\n" +
				"/remember [--scope user|workspace] [--kind K] [--key K] <text>")

	case "session":
		if args != "" {
			break
		}
		info := m.backend.Info()
		return m, tea.Println(fmt.Sprintf("ID: %s\nPath: %s\nProvider: %s\nModel: %s",
			info.SessionID, info.SessionPath, info.Provider, info.Model))

	case "new":
		if args != "" {
			break
		}
		if err := m.backend.NewSession(); err != nil {
			return m, tea.Println("error: " + err.Error())
		}
		if info := m.backend.Info(); info.SessionID != "" {
			return m, tea.Println("Session: " + info.SessionID)
		}
		return m, nil

	case "archive":
		if args != "" {
			break
		}
		return m.handleArchiveCommand()

	case "compact":
		return m.handleCompactCommand(strings.TrimSpace(args))

	case "memory":
		return m.handleMemoryCommand(args)

	case "remember":
		return m.handleRememberCommand(args)
	}

	return m, tea.Println("unknown command: " + input)
}

func (m inlineModel) handleArchiveCommand() (tea.Model, tea.Cmd) {
	archiver, ok := m.backend.(app.SessionArchiver)
	if !ok {
		return m, tea.Println("error: " + app.ErrPersistenceDisabled.Error())
	}
	result, err := archiver.ArchiveCurrentSession(m.rootCtx)
	if err != nil {
		return m, tea.Println("error: " + err.Error())
	}
	var cmds []tea.Cmd
	cmds = append(cmds, tea.Println("Archived: "+result.Path))
	if info := m.backend.Info(); info.SessionID != "" {
		cmds = append(cmds, tea.Println("Session: "+info.SessionID))
	}
	return m, tea.Batch(cmds...)
}

func (m inlineModel) handleCompactCommand(focus string) (tea.Model, tea.Cmd) {
	renderedIDs := make(map[string]struct{})
	renderedNoopEmpty := false
	var cmds []tea.Cmd

	result, err := m.backend.Compact(m.rootCtx, focus, func(event agent.Event) {
		if event.Type != agent.EventCompactionCompleted || event.Compaction == nil {
			return
		}
		c := event.Compaction
		switch {
		case c.Noop && c.CheckpointID == "":
			if renderedNoopEmpty {
				return
			}
			renderedNoopEmpty = true
		case c.CheckpointID != "":
			if _, ok := renderedIDs[c.CheckpointID]; ok {
				return
			}
			renderedIDs[c.CheckpointID] = struct{}{}
		}
		cmds = append(cmds, tea.Println(compactionLine(agent.CompactionResult{
			CheckpointID:         c.CheckpointID,
			TokensBefore:         c.TokensBefore,
			EstimatedTokensAfter: c.EstimatedTokensAfter,
			Noop:                 c.Noop,
		})))
	})
	if err != nil {
		if errors.Is(err, context.Canceled) && !errors.Is(err, session.ErrFatalPersistence) {
			return m, tea.Batch(cmds...)
		}
		cmds = append(cmds, tea.Println("error: "+err.Error()))
		return m, tea.Batch(cmds...)
	}

	shouldRender := true
	if result.CheckpointID != "" {
		if _, ok := renderedIDs[result.CheckpointID]; ok {
			shouldRender = false
		}
	}
	if result.Noop && result.CheckpointID == "" {
		shouldRender = !renderedNoopEmpty
	}
	if shouldRender {
		cmds = append(cmds, tea.Println(compactionLine(result)))
	}

	return m, tea.Batch(cmds...)
}

func (m inlineModel) handleMemoryCommand(args string) (tea.Model, tea.Cmd) {
	r := &REPL{backend: m.backend}
	var stdout, stderr strings.Builder
	r.stdout = &stdout
	r.stderr = &stderr
	_, err := r.memoryCommand(m.rootCtx, args)
	var cmds []tea.Cmd
	if s := stdout.String(); s != "" {
		cmds = append(cmds, tea.Println(strings.TrimRight(s, "\n")))
	}
	if s := stderr.String(); s != "" {
		cmds = append(cmds, tea.Println(strings.TrimRight(s, "\n")))
	}
	if err != nil {
		cmds = append(cmds, tea.Println("error: "+err.Error()))
	}
	return m, tea.Batch(cmds...)
}

func (m inlineModel) handleRememberCommand(args string) (tea.Model, tea.Cmd) {
	r := &REPL{backend: m.backend}
	var stdout, stderr strings.Builder
	r.stdout = &stdout
	r.stderr = &stderr
	_, err := r.rememberCommand(m.rootCtx, args)
	var cmds []tea.Cmd
	if s := stdout.String(); s != "" {
		cmds = append(cmds, tea.Println(strings.TrimRight(s, "\n")))
	}
	if s := stderr.String(); s != "" {
		cmds = append(cmds, tea.Println(strings.TrimRight(s, "\n")))
	}
	if err != nil {
		cmds = append(cmds, tea.Println("error: "+err.Error()))
	}
	return m, tea.Batch(cmds...)
}

func (m inlineModel) View() tea.View {
	if m.running && m.streamBuf != "" {
		return tea.NewView(m.streamBuf)
	}
	if m.running {
		return tea.NewView("")
	}
	content := m.editor.View()
	if len(m.suggestions) > 0 {
		content += "\n" + renderInlineSuggestions(m.suggestions, m.suggestionIndex)
	}
	return tea.NewView(content)
}

func renderInlineSuggestions(suggestions []command.Command, selected int) string {
	lines := make([]string, 0, len(suggestions))
	for i, c := range suggestions {
		if i == selected {
			lines = append(lines, fmt.Sprintf("\x1b[1m> %-10s %s\x1b[0m", c.Name, c.Description))
			continue
		}
		lines = append(lines, fmt.Sprintf("  %-10s %s", c.Name, c.Description))
	}
	return strings.Join(lines, "\n")
}

// RunInline starts the interactive inline REPL using Bubble Tea without alt screen.
func RunInline(ctx context.Context, input io.Reader, output io.Writer, backend app.Backend) error {
	model := newInlineModel(ctx, backend)
	program := tea.NewProgram(
		model,
		tea.WithContext(ctx),
		tea.WithInput(input),
		tea.WithOutput(output),
		tea.WithoutSignalHandler(),
	)
	finalModel, programErr := program.Run()
	if fatalErr := fatalErrFromInlineModel(finalModel); fatalErr != nil {
		if programErr != nil {
			return errors.Join(fatalErr, programErr)
		}
		return fatalErr
	}
	return programErr
}

func fatalErrFromInlineModel(m tea.Model) error {
	switch v := m.(type) {
	case inlineModel:
		return v.fatalErr
	case *inlineModel:
		if v != nil {
			return v.fatalErr
		}
	}
	return nil
}
