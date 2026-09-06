package tui

import (
	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
)

// handleSandboxCommand shows the current sandbox state, or re-reads the
// [sandbox] configuration and applies it to the running process. A reload
// replaces the executor a running bash command holds, so it is refused while a
// turn is in flight.
func (m Model) handleSandboxCommand(argument string) (tea.Model, tea.Cmd) {
	switch argument {
	case "":
		m.clearEditor()
		m.statusText = ""
		m.appendSandboxEntry(EntrySystem, sandboxSummaryText(m.backend.Info().Sandbox))
		return m, nil
	case "reload":
		if m.running || m.newSessionPending {
			m.statusText = app.ErrPromptActive.Error()
			return m, nil
		}
		m.clearEditor()
		m.statusText = ""
		reloader, ok := m.backend.(app.SandboxReloader)
		if !ok {
			m.appendSandboxEntry(EntrySystem, app.ErrSandboxReloadUnavailable.Error())
			return m, nil
		}
		info, err := reloader.ReloadSandbox(m.rootCtx)
		if err != nil {
			m.statusText = err.Error()
			m.appendSandboxEntry(EntryError, err.Error())
			return m, nil
		}
		m.appendSandboxEntry(EntrySystem, sandboxSummaryText(info))
		return m, nil
	default:
		m.statusText = "usage: /sandbox [reload]"
		return m, nil
	}
}

func sandboxSummaryText(info app.SandboxInfo) string {
	text := "Sandbox: " + info.Summary()
	if reason := info.ReasonCode(); reason != "" {
		text += "\nSandbox reason: " + reason
	}
	return text
}

func (m *Model) appendSandboxEntry(kind EntryKind, text string) {
	m.entries = append(m.entries, Entry{ID: m.nextLiveEntryID("sandbox"), Kind: kind, Raw: text})
	m.rerenderAndRefreshViewportContent()
}
