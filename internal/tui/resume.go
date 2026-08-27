package tui

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	"charm.land/bubbles/v2/key"
	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/session"
)

const (
	resumeListLimit       = 20
	resumeMaxPathBytes    = 4096
	resumeMaxFieldBytes   = 512
	resumeMaxErrorBytes   = 512
	resumeMaxWarningCount = 8
	resumeMaxWarningBytes = 512
)

var errResumeReconciliationUnsafe = errors.New("session resume completed while newer work is active")

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
	mode          resumeMode
	generation    uint64
	sessions      []session.SessionInfo
	skipped       int
	selected      int
	errText       string
	operationPath string
	listPending   bool
	listCancel    context.CancelFunc
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
	errText    string
}

type resumeCommittedPathState uint8

const (
	resumeCommittedPathUnknown resumeCommittedPathState = iota
	resumeCommittedPathValid
	resumeCommittedPathMissing
	resumeCommittedPathUnusable
)

type sessionResumeResultMsg struct {
	generation         uint64
	path               string
	result             app.ResumeResult
	committedPathState resumeCommittedPathState
	warningsSkipped    int
	errText            string
}

func sessionBrowserFromBackend(backend app.Backend) (app.SessionBrowser, bool) {
	browser, ok := backend.(app.SessionBrowser)
	return browser, ok
}

func runSessionListCommand(ctx context.Context, browser app.SessionBrowser, generation uint64) tea.Cmd {
	return func() tea.Msg {
		if browser == nil {
			return sessionListResultMsg{generation: generation, errText: boundedResumeError(errors.New("session browser is required"))}
		}
		result, err := browser.ListSessions(rootContext(ctx), resumeListLimit)
		return sessionListResultMsg{
			generation: generation,
			result:     boundedSessionListResult(result),
			errText:    boundedResumeError(err),
		}
	}
}

func runSessionResumeCommand(ctx context.Context, browser app.SessionBrowser, generation uint64, path string) tea.Cmd {
	operationPath := strings.Clone(path)
	return func() tea.Msg {
		if !validResumeOperationPath(operationPath) {
			return sessionResumeResultMsg{generation: generation, errText: "selected session path is invalid"}
		}
		if browser == nil {
			return sessionResumeResultMsg{generation: generation, path: operationPath, errText: boundedResumeError(errors.New("session browser is required"))}
		}
		result, err := browser.ResumeSession(rootContext(ctx), operationPath)
		boundedResult, committedPathState, warningsSkipped := boundedResumeResult(result)
		return sessionResumeResultMsg{
			generation:         generation,
			path:               operationPath,
			result:             boundedResult,
			committedPathState: committedPathState,
			warningsSkipped:    warningsSkipped,
			errText:            boundedResumeError(err),
		}
	}
}

func (m Model) handleResumeCommand() (tea.Model, tea.Cmd) {
	if m.running || m.newSessionPending || m.resume.active() || m.resume.listPending {
		m.statusText = app.ErrPromptActive.Error()
		return m, nil
	}
	browser, ok := sessionBrowserFromBackend(m.backend)
	if !ok {
		m.statusText = app.ErrPersistenceDisabled.Error()
		return m, nil
	}

	m.clearEditor()
	generation := m.resume.generation + 1
	ctx, cancel := context.WithCancel(rootContext(m.rootCtx))
	m.resume = resumePickerState{
		mode:        resumeLoading,
		generation:  generation,
		listPending: true,
		listCancel:  cancel,
	}
	m.statusText = ""
	return m, runSessionListCommand(ctx, browser, generation)
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
		if m.resume.mode == resumeLoading && m.resume.listCancel != nil {
			m.resume.listCancel()
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
	if !validResumeOperationPath(selected.Path) {
		m.resume.mode = resumeResumeError
		m.resume.errText = "selected session path is invalid"
		m.statusText = m.resume.errText
		return m, nil, true
	}
	m.resume.mode = resumeResuming
	m.resume.operationPath = selected.Path
	m.resume.errText = ""
	m.statusText = ""
	return m, runSessionResumeCommand(m.rootCtx, browser, m.resume.generation, selected.Path), true
}

func (m Model) applySessionListResult(msg sessionListResultMsg) (tea.Model, tea.Cmd) {
	if !m.resume.listPending || msg.generation != m.resume.generation {
		return m, nil
	}
	m.releaseSessionListOwnership()
	if m.resume.mode != resumeLoading {
		return m, nil
	}

	result := boundedSessionListResult(msg.result)
	errText := boundedResumeErrorText(msg.errText)
	m.resume.sessions = result.Sessions
	m.resume.skipped = result.Skipped
	m.resume.selected = 0
	m.resume.errText = ""
	if errText != "" {
		m.resume.mode = resumeLoadError
		m.resume.errText = errText
		m.statusText = errText
		return m, nil
	}
	m.resume.mode = resumeLoaded
	m.statusText = resumeListStatus(result)
	return m, nil
}

func (m Model) applySessionResumeResult(msg sessionResumeResultMsg) (tea.Model, tea.Cmd) {
	currentOperation := m.resume.mode == resumeResuming &&
		msg.generation == m.resume.generation &&
		validResumeOperationPath(msg.path) &&
		msg.path == m.resume.operationPath
	if !currentOperation {
		return m.reconcileCommittedStaleResume(msg)
	}

	errText := boundedResumeErrorText(msg.errText)
	if errText != "" {
		m.resume.mode = resumeResumeError
		m.resume.errText = errText
		m.statusText = errText
		return m, nil
	}
	result, _, locallySkipped := boundedResumeResult(msg.result)
	warningsSkipped := addBoundedCount(nonnegativeCount(msg.warningsSkipped), locallySkipped)
	status := resumeSuccessStatus(result, warningsSkipped)
	m.resetSessionViewFromBackend(status)
	m.closeResumePicker()
	return m, nil
}

func (m Model) reconcileCommittedStaleResume(msg sessionResumeResultMsg) (tea.Model, tea.Cmd) {
	// Failed workers cannot have committed a successful replacement. Their
	// payload remains guarded by the current generation and mode checks above.
	if boundedResumeErrorText(msg.errText) != "" {
		return m, nil
	}
	result, committedPathState, _ := boundedResumeResult(msg.result)
	if committedPathState != resumeCommittedPathValid {
		return m.failClosedStaleResume()
	}
	currentPath := infoFromBackend(m.backend).SessionPath
	if !validResumeOperationPath(currentPath) || currentPath != result.SessionPath {
		// The committed result was superseded by a different backend session. In
		// particular, never rebuild the UI from this stale result payload.
		return m, nil
	}
	if m.hasNewerWorkThanStaleResume() {
		return m.failClosedStaleResume()
	}

	// ResumeSession has committed and its canonical result path is still the
	// backend's current identity. Rebuild only from current backend state; stale warnings
	// and other result payload are intentionally ignored.
	m.resetSessionViewFromBackend("resumed session")
	m.closeResumePicker()
	return m, nil
}

func (m Model) hasNewerWorkThanStaleResume() bool {
	return m.running || m.newSessionPending || m.resume.listPending || m.resume.active()
}

func (m *Model) cancelSessionListWorker() {
	if m.resume.listCancel != nil {
		m.resume.listCancel()
	}
}

func (m Model) failClosedStaleResume() (tea.Model, tea.Cmd) {
	m.fatalErr = errResumeReconciliationUnsafe
	m.statusText = errResumeReconciliationUnsafe.Error()
	return m.quit()
}

func (m Model) quit() (tea.Model, tea.Cmd) {
	m.cancelSessionListWorker()
	return m, tea.Quit
}

func (m *Model) releaseSessionListOwnership() {
	m.cancelSessionListWorker()
	m.resume.listCancel = nil
	m.resume.listPending = false
}

func (m *Model) closeResumePicker() {
	generation := m.resume.generation
	listPending := m.resume.listPending
	listCancel := m.resume.listCancel
	m.resume = resumePickerState{
		generation:  generation,
		listPending: listPending,
		listCancel:  listCancel,
	}
}

func boundedSessionListResult(result session.ListResult) session.ListResult {
	bounded := session.ListResult{Skipped: nonnegativeCount(result.Skipped)}
	candidateCount := min(resumeListLimit, len(result.Sessions))
	bounded.Skipped = addBoundedCount(bounded.Skipped, len(result.Sessions)-candidateCount)
	bounded.Sessions = make([]session.SessionInfo, 0, candidateCount)
	for _, info := range result.Sessions[:candidateCount] {
		boundedInfo, ok := boundedSessionInfo(info)
		if !ok {
			bounded.Skipped = addBoundedCount(bounded.Skipped, 1)
			continue
		}
		bounded.Sessions = append(bounded.Sessions, boundedInfo)
	}
	return bounded
}

func boundedSessionInfo(info session.SessionInfo) (session.SessionInfo, bool) {
	if !validResumeOperationPath(info.Path) {
		return session.SessionInfo{}, false
	}
	return session.SessionInfo{
		Path:         strings.Clone(info.Path),
		ID:           boundedSingleLineText(info.ID, resumeMaxFieldBytes),
		CWD:          boundedSingleLineText(info.CWD, resumeMaxFieldBytes),
		Name:         boundedSingleLineText(info.Name, resumeMaxFieldBytes),
		Created:      detachedResumeTime(info.Created),
		Modified:     detachedResumeTime(info.Modified),
		MessageCount: max(0, info.MessageCount),
		LastUserText: boundedSingleLineText(info.LastUserText, resumeMaxFieldBytes),
		Profile:      boundedSingleLineText(info.Profile, resumeMaxFieldBytes),
		Provider:     boundedSingleLineText(info.Provider, resumeMaxFieldBytes),
		Model:        boundedSingleLineText(info.Model, resumeMaxFieldBytes),
		Current:      info.Current,
	}, true
}

func detachedResumeTime(value time.Time) time.Time {
	return value.UTC()
}

func boundedResumeResult(result app.ResumeResult) (app.ResumeResult, resumeCommittedPathState, int) {
	count := min(len(result.Warnings), resumeMaxWarningCount)
	bounded := app.ResumeResult{Warnings: make([]session.Warning, count)}
	committedPath, committedPathState := boundedResumeCommittedPath(result.SessionPath)
	bounded.SessionPath = committedPath
	for index := 0; index < count; index++ {
		bounded.Warnings[index].Message = boundedSingleLineText(result.Warnings[index].Message, resumeMaxWarningBytes)
	}
	return bounded, committedPathState, len(result.Warnings) - count
}

func boundedResumeCommittedPath(path string) (string, resumeCommittedPathState) {
	switch {
	case path == "":
		return "", resumeCommittedPathMissing
	case validResumeOperationPath(path):
		return strings.Clone(path), resumeCommittedPathValid
	default:
		return "", resumeCommittedPathUnusable
	}
}

func boundedResumeError(err error) string {
	if err == nil {
		return ""
	}
	return boundedResumeErrorText(err.Error())
}

func boundedResumeErrorText(text string) string {
	if text == "" {
		return ""
	}
	bounded := boundedSingleLineText(text, resumeMaxErrorBytes)
	if strings.TrimSpace(bounded) == "" {
		return "session operation failed"
	}
	return bounded
}

func boundedSingleLineText(text string, maximumBytes int) string {
	if maximumBytes <= 0 || text == "" {
		return ""
	}
	var builder strings.Builder
	builder.Grow(min(len(text), maximumBytes))
	for len(text) > 0 {
		r, size := utf8.DecodeRuneInString(text)
		piece := string(r)
		if unicode.IsControl(r) {
			if r < 0x100 {
				piece = fmt.Sprintf("\\x%02x", r)
			} else {
				piece = fmt.Sprintf("\\u%04x", r)
			}
		}
		if builder.Len()+len(piece) > maximumBytes {
			break
		}
		builder.WriteString(piece)
		text = text[size:]
	}
	return strings.Clone(builder.String())
}

func validResumeOperationPath(path string) bool {
	if path == "" || len(path) > resumeMaxPathBytes || !utf8.ValidString(path) {
		return false
	}
	for _, r := range path {
		if unicode.IsControl(r) {
			return false
		}
	}
	return true
}

func nonnegativeCount(count int) int {
	return max(0, count)
}

func addBoundedCount(left, right int) int {
	left = nonnegativeCount(left)
	right = nonnegativeCount(right)
	maximum := int(^uint(0) >> 1)
	if right > maximum-left {
		return maximum
	}
	return left + right
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

func resumeSuccessStatus(result app.ResumeResult, warningsSkipped int) string {
	totalWarnings := addBoundedCount(len(result.Warnings), warningsSkipped)
	if totalWarnings == 0 {
		return "resumed session"
	}
	message := ""
	if len(result.Warnings) > 0 {
		message = strings.TrimSpace(result.Warnings[0].Message)
	}
	if message == "" {
		message = "resumed session with warnings"
	}
	if totalWarnings == 1 {
		return message
	}
	extra := totalWarnings - 1
	suffix := "warnings"
	if extra == 1 {
		suffix = "warning"
	}
	return fmt.Sprintf("%s (+%d more %s)", message, extra, suffix)
}
