package repl

import (
	"context"
	"fmt"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/render"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/charmbracelet/x/ansi"
)

const pickerListLimit = 20

type pickerAction uint8

const (
	pickerResume pickerAction = iota + 1
	pickerArchive
)

type pickerPhase uint8

const (
	pickerLoading pickerPhase = iota + 1
	pickerReady
	pickerExecuting
)

type inlinePicker struct {
	action   pickerAction
	phase    pickerPhase
	sessions []session.SessionInfo
	skipped  int
	selected int
	errText  string
	cancel   context.CancelFunc
}

func (p *inlinePicker) active() bool {
	return p != nil && p.phase != 0
}

type pickerListMsg struct {
	result session.ListResult
	err    error
}

type pickerDoneMsg struct {
	text string
	err  error
}

func (m inlineModel) startPicker(action pickerAction) (tea.Model, tea.Cmd) {
	if m.running {
		return m, tea.Println("error: prompt active")
	}

	var lister interface {
		ListSessions(context.Context, int) (session.ListResult, error)
	}
	switch action {
	case pickerResume:
		browser, ok := m.backend.(app.SessionBrowser)
		if !ok {
			return m, tea.Println("error: " + app.ErrPersistenceDisabled.Error())
		}
		lister = browser
	case pickerArchive:
		archiver, ok := m.backend.(app.SessionArchiver)
		if !ok {
			return m, tea.Println("error: " + app.ErrPersistenceDisabled.Error())
		}
		lister = archiver
	}

	ctx, cancel := context.WithCancel(m.rootCtx)
	m.picker = &inlinePicker{
		action: action,
		phase:  pickerLoading,
		cancel: cancel,
	}

	return m, func() tea.Msg {
		result, err := lister.ListSessions(ctx, pickerListLimit)
		return pickerListMsg{result: result, err: err}
	}
}

func (m inlineModel) handlePickerList(msg pickerListMsg) (tea.Model, tea.Cmd) {
	if !m.picker.active() {
		return m, nil
	}
	if msg.err != nil {
		m.closePicker()
		return m, tea.Println("error: " + msg.err.Error())
	}
	if len(msg.result.Sessions) == 0 {
		m.closePicker()
		text := "no sessions found"
		if msg.result.Skipped > 0 {
			text += fmt.Sprintf(" (skipped %d)", msg.result.Skipped)
		}
		return m, tea.Println(text)
	}
	m.picker.sessions = msg.result.Sessions
	m.picker.skipped = msg.result.Skipped
	m.picker.selected = 0
	m.picker.phase = pickerReady
	return m, nil
}

func (m inlineModel) handlePickerKey(msg tea.KeyPressMsg) (inlineModel, tea.Cmd, bool) {
	if !m.picker.active() {
		return m, nil, false
	}

	if m.picker.phase == pickerLoading || m.picker.phase == pickerExecuting {
		if msg.String() == "ctrl+c" || msg.Key().Code == tea.KeyEscape {
			if m.picker.cancel != nil {
				m.picker.cancel()
			}
			m.closePicker()
			return m, nil, true
		}
		return m, nil, true
	}

	last := len(m.picker.sessions) - 1
	switch msg.Key().Code {
	case tea.KeyUp:
		if m.picker.selected > 0 {
			m.picker.selected--
		}
		return m, nil, true
	case tea.KeyDown:
		if m.picker.selected < last {
			m.picker.selected++
		}
		return m, nil, true
	case tea.KeyEscape:
		m.closePicker()
		return m, nil, true
	case tea.KeyEnter:
		if msg.Key().Mod != 0 {
			return m, nil, true
		}
		return m.executePickerSelection()
	}
	return m, nil, true
}

func (m inlineModel) executePickerSelection() (inlineModel, tea.Cmd, bool) {
	if m.picker.selected < 0 || m.picker.selected >= len(m.picker.sessions) {
		return m, nil, true
	}
	sel := m.picker.sessions[m.picker.selected]

	switch m.picker.action {
	case pickerResume:
		if sel.Current {
			m.closePicker()
			return m, tea.Println("already the current session"), true
		}
		m.picker.phase = pickerExecuting
		browser := m.backend.(app.SessionBrowser)
		path := strings.Clone(sel.Path)
		return m, func() tea.Msg {
			result, err := browser.ResumeSession(context.Background(), path)
			if err != nil {
				return pickerDoneMsg{err: err}
			}
			text := "Resumed session: " + result.SessionPath
			if len(result.Warnings) > 0 {
				text += fmt.Sprintf(" (%d warnings)", len(result.Warnings))
			}
			return pickerDoneMsg{text: text}
		}, true

	case pickerArchive:
		m.picker.phase = pickerExecuting
		archiver := m.backend.(app.SessionArchiver)
		path := strings.Clone(sel.Path)
		return m, func() tea.Msg {
			result, err := archiver.ArchiveSession(context.Background(), path)
			if err != nil {
				return pickerDoneMsg{err: err}
			}
			return pickerDoneMsg{text: "Archived: " + result.Path}
		}, true
	}

	return m, nil, true
}

func (m inlineModel) handlePickerDone(msg pickerDoneMsg) (tea.Model, tea.Cmd) {
	m.closePicker()
	if msg.err != nil {
		return m, tea.Println("error: " + msg.err.Error())
	}
	var cmds []tea.Cmd
	cmds = append(cmds, tea.Println(msg.text))
	if info := m.backend.Info(); info.SessionID != "" {
		cmds = append(cmds, tea.Println("Session: "+info.SessionID))
	}
	return m, tea.Batch(cmds...)
}

func (m *inlineModel) closePicker() {
	if m.picker != nil && m.picker.cancel != nil {
		m.picker.cancel()
	}
	m.picker = nil
}

func renderPickerView(p *inlinePicker, width int) string {
	if p == nil {
		return ""
	}

	title := "Resume Session"
	if p.action == pickerArchive {
		title = "Archive Session"
	}

	switch p.phase {
	case pickerLoading:
		return title + "\n  Loading...\n  Esc cancel"
	case pickerExecuting:
		verb := "Resuming..."
		if p.action == pickerArchive {
			verb = "Archiving..."
		}
		return title + "\n  " + verb + "\n  Esc cancel"
	}

	if len(p.sessions) == 0 {
		return title + "\n  (no sessions)\n  Esc close"
	}

	innerWidth := max(1, width-4)
	now := time.Now()

	lines := make([]string, 0, len(p.sessions)+2)
	lines = append(lines, title)
	for i, s := range p.sessions {
		lines = append(lines, renderPickerRow(s, i == p.selected, innerWidth, now))
	}
	if p.skipped > 0 {
		lines = append(lines, fmt.Sprintf("  (%d more not shown)", p.skipped))
	}
	lines = append(lines, "  Enter select · Esc cancel · ↑/↓ navigate")
	return strings.Join(lines, "\n")
}

func renderPickerRow(info session.SessionInfo, selected bool, width int, now time.Time) string {
	cursor := "  "
	if selected {
		cursor = "> "
	}
	current := ""
	if info.Current {
		current = "* "
	}

	age := formatPickerAge(now, info.Modified)
	if age == "" {
		age = formatPickerAge(now, info.Created)
	}
	if age == "" {
		age = "-"
	}

	prefix := cursor + current + fmt.Sprintf("%-4s ", age)
	remaining := max(1, width-ansi.StringWidth(prefix))

	title := info.Name
	if title == "" {
		title = info.LastUserText
	}
	if title == "" {
		title = info.ID
	}
	if title == "" {
		title = "(unnamed)"
	}
	title = render.EscapeSingleLineText(title)

	if info.Model != "" && remaining > 30 {
		model := ansi.Truncate(render.EscapeSingleLineText(info.Model), 18, "…")
		suffix := " · " + model
		remaining -= ansi.StringWidth(suffix)
		return prefix + ansi.Truncate(title, max(1, remaining), "…") + suffix
	}
	return prefix + ansi.Truncate(title, remaining, "…")
}

func formatPickerAge(now time.Time, at time.Time) string {
	if at.IsZero() {
		return ""
	}
	age := now.Sub(at)
	if age < time.Minute {
		return "now"
	}
	switch {
	case age < time.Hour:
		return fmt.Sprintf("%dm", int(age/time.Minute))
	case age < 24*time.Hour:
		return fmt.Sprintf("%dh", int(age/time.Hour))
	case age < 7*24*time.Hour:
		return fmt.Sprintf("%dd", int(age/(24*time.Hour)))
	default:
		return fmt.Sprintf("%dw", int(age/(7*24*time.Hour)))
	}
}
