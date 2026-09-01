package tui

import (
	"context"
	"errors"
	"fmt"
	"strings"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
)

// profileSwitchResultMsg carries the outcome of an async /model profile switch.
type profileSwitchResultMsg struct {
	generation uint64
	err        error
}

func profileSwitcherFromBackend(backend app.Backend) (app.ProfileSwitcher, bool) {
	switcher, ok := backend.(app.ProfileSwitcher)
	return switcher, ok
}

// handleModelCommand runs "/model" and "/model <profile>". Bare shows the
// current profile/provider/model and lists configured profiles; a named profile
// starts a fresh session on it, reusing the /new session-replacement machinery.
func (m Model) handleModelCommand(argument string) (tea.Model, tea.Cmd) {
	switcher, ok := profileSwitcherFromBackend(m.backend)
	if !ok {
		m.statusText = app.ErrProfileSwitchUnavailable.Error()
		return m, nil
	}
	if argument == "" {
		info := m.backend.Info()
		m.clearEditor()
		m.statusText = ""
		text := fmt.Sprintf("Current: profile %s (provider %s, model %s)", info.Profile, info.Provider, info.Model)
		if profiles := switcher.Profiles(); len(profiles) > 0 {
			text += "\nProfiles: " + strings.Join(profiles, ", ")
		} else {
			text += "\nNo profiles configured."
		}
		m.appendLoginEntry(EntrySystem, text)
		return m, nil
	}
	if m.running || m.newSessionPending || m.profileSwitchPending {
		m.statusText = app.ErrPromptActive.Error()
		return m, nil
	}
	m.clearEditor()
	m.profileSwitchGeneration++
	m.profileSwitchPending = true
	m.statusText = ""
	return m, runProfileSwitchCommand(m.rootCtx, switcher, m.profileSwitchGeneration, argument)
}

func runProfileSwitchCommand(ctx context.Context, switcher app.ProfileSwitcher, generation uint64, profile string) tea.Cmd {
	name := strings.Clone(profile)
	return func() tea.Msg {
		if switcher == nil {
			return profileSwitchResultMsg{generation: generation, err: errors.New("profile switcher is required")}
		}
		_, err := switcher.SwitchProfile(rootContext(ctx), name)
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
	status := fmt.Sprintf("switched to profile %s (provider %s, model %s)", info.Profile, info.Provider, info.Model)
	m.resetSessionViewFromBackend(status)
	return m, nil
}
