package tui

import (
	"context"
	"errors"
	"fmt"
	"strings"

	"charm.land/bubbles/v2/key"
	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/charmbracelet/x/ansi"
)

type profilePickerState struct {
	profiles []string
	selected int
}

func (p profilePickerState) active() bool {
	return p.profiles != nil
}

func (p profilePickerState) currentSelection() (string, bool) {
	if p.selected < 0 || p.selected >= len(p.profiles) {
		return "", false
	}
	return p.profiles[p.selected], true
}

// profileSwitchResultMsg carries the outcome of an async /model profile switch.
type profileSwitchResultMsg struct {
	generation uint64
	err        error
}

func profileSwitcherFromBackend(backend app.Backend) (app.ProfileSwitcher, bool) {
	switcher, ok := backend.(app.ProfileSwitcher)
	return switcher, ok
}

// handleModelCommand runs "/model" and "/model <profile>". Bare opens a
// profile picker in the TUI; a named profile starts a fresh session on it,
// reusing the /new session-replacement machinery.
func (m Model) handleModelCommand(argument string) (tea.Model, tea.Cmd) {
	if !app.BackendDynamicContentAvailable(m.backend) {
		m.statusText = app.ErrProfileSwitchUnavailable.Error()
		return m, nil
	}
	switcher, ok := profileSwitcherFromBackend(m.backend)
	if !ok {
		m.clearEditor()
		m.statusText = app.ErrProfileSwitchUnavailable.Error()
		return m, nil
	}
	if argument == "" {
		m.clearEditor()
		m.statusText = ""
		m.profilePicker = profilePickerState{profiles: switcher.Profiles()}
		if len(m.profilePicker.profiles) == 0 {
			m.statusText = "no profiles configured"
		}
		return m, nil
	}
	return m.startProfileSwitch(switcher, argument)
}

func (m Model) handleProfilePickerKeyPress(msg tea.KeyPressMsg) (tea.Model, tea.Cmd, bool) {
	if !m.profilePicker.active() {
		return m, nil, false
	}
	exactMatch := func(binding key.Binding) bool {
		return msg.Key().Mod == 0 && key.Matches(msg, binding)
	}
	if exactMatch(m.keymap.ResumeClose) {
		m.profilePicker = profilePickerState{}
		return m, nil, true
	}
	if len(m.profilePicker.profiles) > 0 {
		last := len(m.profilePicker.profiles) - 1
		switch {
		case exactMatch(m.keymap.ResumeUp):
			m.profilePicker.selected = clamp(m.profilePicker.selected-1, 0, last)
			return m, nil, true
		case exactMatch(m.keymap.ResumeDown):
			m.profilePicker.selected = clamp(m.profilePicker.selected+1, 0, last)
			return m, nil, true
		case exactMatch(m.keymap.ResumePageUp):
			m.profilePicker.selected = clamp(m.profilePicker.selected-resumeVisibleRows(m.width, m.height), 0, last)
			return m, nil, true
		case exactMatch(m.keymap.ResumePageDown):
			m.profilePicker.selected = clamp(m.profilePicker.selected+resumeVisibleRows(m.width, m.height), 0, last)
			return m, nil, true
		}
	}
	if !exactMatch(m.keymap.ResumeSelect) {
		return m, nil, true
	}
	profile, ok := m.profilePicker.currentSelection()
	if !ok {
		return m, nil, true
	}
	switcher, ok := profileSwitcherFromBackend(m.backend)
	if !ok {
		m.statusText = app.ErrProfileSwitchUnavailable.Error()
		return m, nil, true
	}
	updated, cmd := m.startProfileSwitch(switcher, profile)
	return updated, cmd, true
}

func (m Model) startProfileSwitch(switcher app.ProfileSwitcher, profile string) (tea.Model, tea.Cmd) {
	if m.running || m.newSessionPending || m.profileSwitchPending {
		m.statusText = app.ErrPromptActive.Error()
		return m, nil
	}
	m.clearEditor()
	m.profilePicker = profilePickerState{}
	m.profileSwitchGeneration++
	m.profileSwitchPending = true
	m.pendingCanceledTasks = m.countActiveTasks()
	m.statusText = ""
	return m, runProfileSwitchCommand(m.rootCtx, switcher, m.profileSwitchGeneration, profile)
}

func runProfileSwitchCommand(ctx context.Context, switcher app.ProfileSwitcher, generation uint64, profile string) tea.Cmd {
	name := strings.Clone(profile)
	return func() tea.Msg {
		if switcher == nil {
			return profileSwitchResultMsg{generation: generation, err: errors.New("profile switcher is required")}
		}
		_, err := switcher.SwitchProfile(rootContext(ctx), name)
		if err != nil {
			return profileSwitchResultMsg{generation: generation, err: err}
		}
		err = switcher.SetDefaultProfile(rootContext(ctx), name)
		return profileSwitchResultMsg{generation: generation, err: err}
	}
}

func (m Model) applyProfileSwitchResult(msg profileSwitchResultMsg) (tea.Model, tea.Cmd) {
	if !m.profileSwitchPending || msg.generation != m.profileSwitchGeneration {
		return m, nil
	}
	m.profileSwitchPending = false
	if msg.err != nil {
		m.statusText = msg.err.Error()
		return m, nil
	}
	if m.running {
		return m, nil
	}
	info := m.backend.Info()
	status := fmt.Sprintf("switched to profile %s (provider %s, model %s); set as default", info.Profile, info.Provider, info.Model)
	m.resetSessionViewFromBackend(status)
	m.noteCanceledTasks(m.pendingCanceledTasks)
	return m, nil
}

func renderProfilePicker(width, height int, state profilePickerState, info app.Info) string {
	if width <= 0 || height <= 0 {
		return ""
	}
	innerWidth := max(1, width-4)
	selected := 0
	if len(state.profiles) > 0 {
		selected = clamp(state.selected, 0, len(state.profiles)-1)
	}
	title := "Select Model Profile"
	if len(state.profiles) > 0 {
		title += fmt.Sprintf("  %d/%d", selected+1, len(state.profiles))
	}
	title = clipSingleLineText(title, innerWidth)

	body := make([]string, 0, resumeVisibleRows(width, height))
	current := fmt.Sprintf("Current: profile %s (provider %s, model %s)", info.Profile, info.Provider, info.Model)
	body = append(body, clipSingleLineText(current, innerWidth))
	if len(state.profiles) == 0 {
		body = append(body, "No profiles configured")
	} else {
		start, end := resumeVisibleRange(len(state.profiles), selected, max(1, resumeVisibleRows(width, height)-1))
		for index := start; index < end; index++ {
			body = append(body, renderProfileRow(state.profiles[index], state.profiles[index] == info.Profile, index == selected, innerWidth))
		}
	}
	lines := make([]string, 0, len(body)+2)
	lines = append(lines, title)
	lines = append(lines, body...)
	lines = append(lines, clipSingleLineText("Enter switch · Esc close · ↑/↓ PgUp/PgDn", innerWidth))
	return renderOverlay(width, height, strings.Join(lines, "\n"))
}

func renderProfileRow(profile string, current, selected bool, width int) string {
	cursor := " "
	if selected {
		cursor = ">"
	}
	marker := " "
	if current {
		marker = "*"
	}
	text := cursor + marker + " " + escapeSingleLineText(profile)
	return ansi.Truncate(text, max(1, width), "…")
}
