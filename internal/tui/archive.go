package tui

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"time"

	"charm.land/bubbles/v2/key"
	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/render"
	"github.com/baiyuqing/otto/internal/session"
)

type archiveMode uint8

const (
	archiveClosed archiveMode = iota
	archiveLoading
	archiveLoaded
	archiveLoadError
	archiveArchiving
	archiveError
)

type archivePickerState struct {
	mode          archiveMode
	generation    uint64
	sessions      []session.SessionInfo
	skipped       int
	selected      int
	errText       string
	operationPath string
	listPending   bool
	listCancel    context.CancelFunc
}

func (a archivePickerState) active() bool {
	return a.mode != archiveClosed
}

func (a archivePickerState) currentSelection() (session.SessionInfo, bool) {
	if a.selected < 0 || a.selected >= len(a.sessions) {
		return session.SessionInfo{}, false
	}
	return a.sessions[a.selected], true
}

type archiveSessionResultMsg struct {
	generation uint64
	path       string
	result     session.ArchiveResult
	errText    string
}

func sessionArchiverFromBackend(backend app.Backend) (app.SessionArchiver, bool) {
	archiver, ok := backend.(app.SessionArchiver)
	return archiver, ok
}

func runArchiveListCommand(ctx context.Context, archiver app.SessionArchiver, generation uint64) tea.Cmd {
	return func() tea.Msg {
		if archiver == nil {
			return sessionListResultMsg{generation: generation, errText: boundedResumeError(errors.New("session archiver is required"))}
		}
		result, err := archiver.ListSessions(rootContext(ctx), resumeListLimit)
		return sessionListResultMsg{
			generation: generation,
			result:     boundedSessionListResult(result),
			errText:    boundedResumeError(err),
		}
	}
}

func runArchiveSessionCommand(ctx context.Context, archiver app.SessionArchiver, generation uint64, path string) tea.Cmd {
	operationPath := strings.Clone(path)
	return func() tea.Msg {
		if !validResumeOperationPath(operationPath) {
			return archiveSessionResultMsg{generation: generation, errText: "selected session path is invalid"}
		}
		if archiver == nil {
			return archiveSessionResultMsg{generation: generation, path: operationPath, errText: boundedResumeError(errors.New("session archiver is required"))}
		}
		result, err := archiver.ArchiveSession(rootContext(ctx), operationPath)
		return archiveSessionResultMsg{
			generation: generation,
			path:       operationPath,
			result:     result,
			errText:    boundedResumeError(err),
		}
	}
}

func (m Model) handleArchiveCommand() (tea.Model, tea.Cmd) {
	if m.running || m.newSessionPending || m.resume.active() || m.archive.active() || m.archive.listPending {
		m.statusText = app.ErrPromptActive.Error()
		return m, nil
	}
	archiver, ok := sessionArchiverFromBackend(m.backend)
	if !ok {
		m.statusText = app.ErrPersistenceDisabled.Error()
		return m, nil
	}

	m.clearEditor()
	generation := m.archive.generation + 1
	ctx, cancel := context.WithCancel(rootContext(m.rootCtx))
	m.archive = archivePickerState{
		mode:        archiveLoading,
		generation:  generation,
		listPending: true,
		listCancel:  cancel,
	}
	m.statusText = ""
	return m, runArchiveListCommand(ctx, archiver, generation)
}

func (m Model) handleArchiveKeyPress(msg tea.KeyPressMsg) (tea.Model, tea.Cmd, bool) {
	if !m.archive.active() {
		return m, nil, false
	}

	exactMatch := func(binding key.Binding) bool {
		return msg.Key().Mod == 0 && key.Matches(msg, binding)
	}
	if exactMatch(m.keymap.ResumeClose) {
		if m.archive.mode == archiveArchiving {
			m.statusText = "archive in progress"
			return m, nil, true
		}
		if m.archive.mode == archiveLoading && m.archive.listCancel != nil {
			m.archive.listCancel()
		}
		m.closeArchivePicker()
		return m, nil, true
	}
	if m.archive.mode == archiveArchiving {
		return m, nil, true
	}
	if (m.archive.mode == archiveLoaded || m.archive.mode == archiveError) && len(m.archive.sessions) > 0 {
		last := len(m.archive.sessions) - 1
		switch {
		case exactMatch(m.keymap.ResumeUp):
			m.archive.selected = clamp(m.archive.selected-1, 0, last)
			return m, nil, true
		case exactMatch(m.keymap.ResumeDown):
			m.archive.selected = clamp(m.archive.selected+1, 0, last)
			return m, nil, true
		case exactMatch(m.keymap.ResumePageUp):
			m.archive.selected = clamp(m.archive.selected-resumeVisibleRows(m.width, m.height), 0, last)
			return m, nil, true
		case exactMatch(m.keymap.ResumePageDown):
			m.archive.selected = clamp(m.archive.selected+resumeVisibleRows(m.width, m.height), 0, last)
			return m, nil, true
		}
	}
	if !exactMatch(m.keymap.ResumeSelect) || (m.archive.mode != archiveLoaded && m.archive.mode != archiveError) {
		return m, nil, true
	}
	selected, ok := m.archive.currentSelection()
	if !ok {
		return m, nil, true
	}
	archiver, ok := sessionArchiverFromBackend(m.backend)
	if !ok {
		m.statusText = app.ErrPersistenceDisabled.Error()
		return m, nil, true
	}
	if !validResumeOperationPath(selected.Path) {
		m.archive.mode = archiveError
		m.archive.errText = "selected session path is invalid"
		m.statusText = m.archive.errText
		return m, nil, true
	}
	m.archive.mode = archiveArchiving
	m.archive.operationPath = selected.Path
	m.archive.errText = ""
	m.statusText = ""
	return m, runArchiveSessionCommand(m.rootCtx, archiver, m.archive.generation, selected.Path), true
}

func (m Model) applyArchiveListResult(msg sessionListResultMsg) (tea.Model, tea.Cmd) {
	if !m.archive.listPending || msg.generation != m.archive.generation {
		return m, nil
	}
	m.releaseArchiveListOwnership()
	if m.archive.mode != archiveLoading {
		return m, nil
	}

	result := boundedSessionListResult(msg.result)
	errText := boundedResumeErrorText(msg.errText)
	m.archive.sessions = result.Sessions
	m.archive.skipped = result.Skipped
	m.archive.selected = 0
	m.archive.errText = ""
	if errText != "" {
		m.archive.mode = archiveLoadError
		m.archive.errText = errText
		m.statusText = errText
		return m, nil
	}
	m.archive.mode = archiveLoaded
	m.statusText = archiveListStatus(result)
	return m, nil
}

func (m Model) applyArchiveSessionResult(msg archiveSessionResultMsg) (tea.Model, tea.Cmd) {
	if m.archive.mode != archiveArchiving ||
		msg.generation != m.archive.generation ||
		!validResumeOperationPath(msg.path) ||
		msg.path != m.archive.operationPath {
		return m, nil
	}

	errText := boundedResumeErrorText(msg.errText)
	if errText != "" {
		m.archive.mode = archiveError
		m.archive.errText = errText
		m.statusText = errText
		return m, nil
	}

	selectedWasCurrent := false
	if selected, ok := m.archive.currentSelection(); ok {
		selectedWasCurrent = selected.Current
	}
	status := "archived session"
	if msg.result.ID != "" {
		status = "archived session " + msg.result.ID
	}
	if selectedWasCurrent {
		// The controller swapped the current session for a fresh one.
		status += "; started new session"
		m.resetSessionViewFromBackend(status)
	} else {
		m.statusText = status
	}
	m.closeArchivePicker()
	return m, nil
}

func (m *Model) cancelArchiveListWorker() {
	if m.archive.listCancel != nil {
		m.archive.listCancel()
	}
}

func (m *Model) releaseArchiveListOwnership() {
	m.cancelArchiveListWorker()
	m.archive.listCancel = nil
	m.archive.listPending = false
}

func (m *Model) closeArchivePicker() {
	generation := m.archive.generation
	listPending := m.archive.listPending
	listCancel := m.archive.listCancel
	m.archive = archivePickerState{
		generation:  generation,
		listPending: listPending,
		listCancel:  listCancel,
	}
}

func renderArchivePicker(width, height int, state archivePickerState, spinnerText string, now time.Time) string {
	if width <= 0 || height <= 0 {
		return ""
	}
	innerWidth := max(1, width-4)
	selected := 0
	if len(state.sessions) > 0 {
		selected = clamp(state.selected, 0, len(state.sessions)-1)
	}
	title := "Archive Session"
	if len(state.sessions) > 0 {
		title += fmt.Sprintf("  %d/%d", selected+1, len(state.sessions))
		if state.sessions[selected].Current {
			title += " · current"
		}
	}
	switch state.mode {
	case archiveArchiving:
		title += " · Archiving"
	case archiveError:
		if state.errText != "" {
			title += " · Error: " + render.EscapeSingleLineText(state.errText)
		}
	}
	title = clipSingleLineText(title, innerWidth)

	body := make([]string, 0, resumeVisibleRows(width, height))
	help := "Enter archive · Esc close · ↑/↓ PgUp/PgDn"
	switch state.mode {
	case archiveLoading:
		loading := "Loading sessions"
		if safeSpinner := clipSingleLineText(spinnerText, innerWidth); safeSpinner != "" {
			loading = safeSpinner + " " + loading
		}
		body = append(body, clipSingleLineText(loading, innerWidth))
		help = "Esc close"
	case archiveLoadError:
		body = append(body, "Unable to load sessions")
		if state.errText != "" && len(body) < resumeVisibleRows(width, height) {
			body = append(body, clipSingleLineText(state.errText, innerWidth))
		}
		help = "Esc close"
	case archiveLoaded, archiveError, archiveArchiving:
		if len(state.sessions) == 0 {
			empty := "No active sessions"
			if state.skipped > 0 {
				empty += fmt.Sprintf(" (skipped %d)", state.skipped)
			}
			body = append(body, clipSingleLineText(empty, innerWidth))
			help = "Esc close"
		} else {
			start, end := resumeVisibleRange(len(state.sessions), selected, resumeVisibleRows(width, height))
			for index := start; index < end; index++ {
				body = append(body, renderResumeSessionRow(state.sessions[index], index == selected, innerWidth, now))
			}
		}
		if state.mode == archiveArchiving {
			help = "Actions disabled · Esc status"
		}
	}
	if len(body) == 0 {
		body = append(body, "No active sessions")
	}

	lines := make([]string, 0, len(body)+2)
	lines = append(lines, title)
	lines = append(lines, body...)
	lines = append(lines, clipSingleLineText(help, innerWidth))
	return renderOverlay(width, height, strings.Join(lines, "\n"))
}

func archiveListStatus(result session.ListResult) string {
	if len(result.Sessions) > 0 {
		return ""
	}
	status := "no active sessions found"
	if result.Skipped > 0 {
		status = fmt.Sprintf("%s (skipped %d)", status, result.Skipped)
	}
	return status
}
