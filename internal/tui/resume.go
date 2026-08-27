package tui

import (
	"context"
	"errors"
	"fmt"
	"strings"

	"charm.land/bubbles/v2/key"
	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/session"
)

const resumeListLimit = 20

type resumeMode uint8

const (
	resumeClosed resumeMode = iota
	resumeLoading
	resumeLoaded
	resumeLoadError
	resumeResuming
	resumeResumeError
)

type resumePickerState struct {
	mode       resumeMode
	generation uint64
	sessions   []session.SessionInfo
	skipped    int
	selected   int
	errText    string
}

func (r resumePickerState) active() bool {
	return r.mode != resumeClosed
}

func (r resumePickerState) currentSelection() (session.SessionInfo, bool) {
	if r.selected < 0 || r.selected >= len(r.sessions) {
		return session.SessionInfo{}, false
	}
	return r.sessions[r.selected], true
}

type sessionListResultMsg struct {
	generation uint64
	result     session.ListResult
	err        error
}

type sessionResumeResultMsg struct {
	generation uint64
	path       string
	result     app.ResumeResult
	err        error
}

func sessionBrowserFromBackend(backend app.Backend) (app.SessionBrowser, bool) {
	browser, ok := backend.(app.SessionBrowser)
	return browser, ok
}

func runSessionListCommand(ctx context.Context, browser app.SessionBrowser, generation uint64) tea.Cmd {
	return func() tea.Msg {
		if browser == nil {
			return sessionListResultMsg{generation: generation, err: errors.New("session browser is required")}
		}
		result, err := browser.ListSessions(rootContext(ctx), resumeListLimit)
		return sessionListResultMsg{generation: generation, result: cloneSessionListResult(result), err: err}
	}
}

func runSessionResumeCommand(ctx context.Context, browser app.SessionBrowser, generation uint64, path string) tea.Cmd {
	return func() tea.Msg {
		if browser == nil {
			return sessionResumeResultMsg{generation: generation, path: path, err: errors.New("session browser is required")}
		}
		result, err := browser.ResumeSession(rootContext(ctx), path)
		return sessionResumeResultMsg{generation: generation, path: path, result: cloneResumeResult(result), err: err}
	}
}

func cloneSessionListResult(result session.ListResult) session.ListResult {
	result.Sessions = append([]session.SessionInfo(nil), result.Sessions...)
	return result
}

func cloneResumeResult(result app.ResumeResult) app.ResumeResult {
	result.Warnings = append([]session.Warning(nil), result.Warnings...)
	return result
}

func (m Model) handleResumeCommand() (tea.Model, tea.Cmd) {
	if m.running || m.newSessionPending || m.resume.active() {
		m.statusText = app.ErrPromptActive.Error()
		return m, nil
	}
	browser, ok := sessionBrowserFromBackend(m.backend)
	if !ok {
		m.statusText = app.ErrPersistenceDisabled.Error()
		return m, nil
	}
	m.clearEditor()
	m.resume.generation++
	m.resume.mode = resumeLoading
	m.resume.sessions = nil
	m.resume.skipped = 0
	m.resume.selected = 0
	m.resume.errText = ""
	m.statusText = ""
	return m, runSessionListCommand(m.rootCtx, browser, m.resume.generation)
}

func (m Model) handleResumeKeyPress(msg tea.KeyPressMsg) (tea.Model, tea.Cmd, bool) {
	if !m.resume.active() {
		return m, nil, false
	}
	if isEscapeKey(msg) {
		if m.resume.mode == resumeResuming {
			m.statusText = "resume in progress"
			return m, nil, true
		}
		m.closeResumePicker()
		return m, nil, true
	}
	if !key.Matches(msg, m.keymap.Submit) {
		return m, nil, true
	}
	if m.resume.mode != resumeLoaded && m.resume.mode != resumeResumeError {
		return m, nil, true
	}
	selected, ok := m.resume.currentSelection()
	if !ok {
		return m, nil, true
	}
	if selected.Current {
		m.closeResumePicker()
		return m, nil, true
	}
	browser, ok := sessionBrowserFromBackend(m.backend)
	if !ok {
		m.statusText = app.ErrPersistenceDisabled.Error()
		return m, nil, true
	}
	m.resume.mode = resumeResuming
	m.resume.errText = ""
	m.statusText = ""
	return m, runSessionResumeCommand(m.rootCtx, browser, m.resume.generation, selected.Path), true
}

func (m Model) applySessionListResult(msg sessionListResultMsg) (tea.Model, tea.Cmd) {
	if m.resume.mode != resumeLoading || msg.generation != m.resume.generation {
		return m, nil
	}
	m.resume.sessions = cloneSessionInfos(msg.result.Sessions)
	m.resume.skipped = msg.result.Skipped
	m.resume.selected = 0
	m.resume.errText = ""
	if msg.err != nil {
		m.resume.mode = resumeLoadError
		m.resume.errText = msg.err.Error()
		m.statusText = m.resume.errText
		return m, nil
	}
	m.resume.mode = resumeLoaded
	m.statusText = resumeListStatus(msg.result)
	return m, nil
}

func (m Model) applySessionResumeResult(msg sessionResumeResultMsg) (tea.Model, tea.Cmd) {
	if m.resume.mode != resumeResuming || msg.generation != m.resume.generation {
		return m, nil
	}
	if msg.err != nil {
		m.resume.mode = resumeResumeError
		m.resume.errText = msg.err.Error()
		m.statusText = m.resume.errText
		return m, nil
	}
	status := resumeSuccessStatus(msg.result)
	m.resetSessionViewFromBackend(status)
	m.closeResumePicker()
	return m, nil
}

func (m *Model) closeResumePicker() {
	generation := m.resume.generation
	m.resume = resumePickerState{generation: generation}
}

func cloneSessionInfos(infos []session.SessionInfo) []session.SessionInfo {
	if infos == nil {
		return nil
	}
	return append([]session.SessionInfo(nil), infos...)
}

func resumeListStatus(result session.ListResult) string {
	if len(result.Sessions) > 0 {
		return ""
	}
	status := "no resumable sessions found"
	if result.Skipped > 0 {
		status = fmt.Sprintf("%s (skipped %d)", status, result.Skipped)
	}
	return status
}

func resumeSuccessStatus(result app.ResumeResult) string {
	if len(result.Warnings) == 0 {
		return "resumed session"
	}
	message := strings.TrimSpace(result.Warnings[0].Message)
	if message == "" {
		message = "resumed session with warnings"
	}
	if len(result.Warnings) == 1 {
		return message
	}
	extra := len(result.Warnings) - 1
	suffix := "warnings"
	if extra == 1 {
		suffix = "warning"
	}
	return fmt.Sprintf("%s (+%d more %s)", message, extra, suffix)
}
