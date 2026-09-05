package tui

import (
	"context"
	"errors"
	"fmt"
	"os/exec"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
)

var tuiOpenBrowser = func(url string) {
	_ = exec.Command("open", url).Start()
}

type loginResultMsg struct {
	generation uint64
	url        string
	err        error
	done       bool
}

func (m Model) handleLoginCommand(argument string) (tea.Model, tea.Cmd) {
	if !app.BackendDynamicContentAvailable(m.backend) {
		m.clearEditor()
		m.statusText = ""
		m.appendLoginEntry(EntrySystem, app.ErrAuthenticationUnavailable.Error())
		return m, nil
	}
	authentication, ok := m.backend.(app.Authentication)
	if !ok {
		m.clearEditor()
		m.statusText = ""
		m.appendLoginEntry(EntrySystem, app.ErrAuthenticationUnavailable.Error())
		return m, nil
	}
	switch argument {
	case "status":
		line, _ := authentication.Status(m.rootCtx)
		m.clearEditor()
		m.statusText = ""
		m.appendLoginEntry(EntrySystem, line)
		return m, nil
	case "":
		if m.running || m.newSessionPending || m.loginPending {
			m.statusText = app.ErrPromptActive.Error()
			return m, nil
		}
		m.clearEditor()
		m.loginGeneration++
		m.loginPending = true
		m.statusText = ""
		channel := make(chan loginResultMsg, 2)
		m.loginChannel = channel
		return m, startLoginCommand(m.rootCtx, authentication, m.loginGeneration, channel)
	default:
		m.statusText = "usage: /login [status]"
		return m, nil
	}
}

func (m Model) handleLogoutCommand() (tea.Model, tea.Cmd) {
	if !app.BackendDynamicContentAvailable(m.backend) {
		m.clearEditor()
		m.statusText = ""
		m.appendLoginEntry(EntrySystem, app.ErrAuthenticationUnavailable.Error())
		return m, nil
	}
	authentication, ok := m.backend.(app.Authentication)
	if !ok {
		m.clearEditor()
		m.statusText = ""
		m.appendLoginEntry(EntrySystem, app.ErrAuthenticationUnavailable.Error())
		return m, nil
	}
	m.clearEditor()
	m.statusText = ""
	removed, err := authentication.Logout(m.rootCtx)
	if err != nil {
		m.statusText = err.Error()
		m.appendLoginEntry(EntryError, err.Error())
		return m, nil
	}
	if !removed {
		m.appendLoginEntry(EntrySystem, "Not signed in to ChatGPT.")
		return m, nil
	}
	m.appendLoginEntry(EntrySystem, "Signed out of ChatGPT.")
	return m, nil
}

func startLoginCommand(ctx context.Context, authentication app.Authentication, generation uint64, channel chan loginResultMsg) tea.Cmd {
	return func() tea.Msg {
		go runLoginWorker(ctx, authentication, generation, channel)
		return waitLogin(generation, channel)()
	}
}

func runLoginWorker(ctx context.Context, authentication app.Authentication, generation uint64, channel chan loginResultMsg) {
	defer close(channel)
	opener := func(url string) error {
		channel <- loginResultMsg{generation: generation, url: url}
		tuiOpenBrowser(url)
		return nil
	}
	err := authentication.Login(rootContext(ctx), opener)
	if err != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			channel <- loginResultMsg{generation: generation, err: ctxErr, done: true}
			return
		}
		channel <- loginResultMsg{generation: generation, err: err, done: true}
		return
	}
	channel <- loginResultMsg{generation: generation, done: true}
}

func waitLogin(generation uint64, channel chan loginResultMsg) tea.Cmd {
	return func() tea.Msg {
		msg, ok := <-channel
		if !ok {
			return loginResultMsg{generation: generation, done: true}
		}
		return msg
	}
}

func (m Model) applyLoginResult(msg loginResultMsg) (tea.Model, tea.Cmd) {
	if !m.loginPending || msg.generation != m.loginGeneration {
		return m, nil
	}
	m.loginPending = !msg.done
	if !msg.done {
		m.appendLoginEntry(EntrySystem, fmt.Sprintf("Open this URL to sign in:\n\n  %s", msg.url))
		return m, waitLogin(m.loginGeneration, m.loginChannel)
	}
	m.loginChannel = nil
	if msg.err != nil {
		if errors.Is(msg.err, app.ErrAuthenticationUnsupported) {
			provider := m.backend.Info().Provider
			m.statusText = ""
			m.appendLoginEntry(EntrySystem, fmt.Sprintf("The %s provider authenticates with an API key from the environment; there is nothing to sign in to.", provider))
			return m, nil
		}
		if errors.Is(msg.err, app.ErrAuthenticationUnavailable) {
			m.statusText = ""
			m.appendLoginEntry(EntrySystem, app.ErrAuthenticationUnavailable.Error())
			return m, nil
		}
		m.statusText = msg.err.Error()
		m.appendLoginEntry(EntryError, fmt.Sprintf("login failed: %v", msg.err))
		return m, nil
	}
	m.statusText = ""
	m.appendLoginEntry(EntrySystem, "Signed in to ChatGPT. Restart Otto to use the new credentials.")
	return m, nil
}

func (m *Model) appendLoginEntry(kind EntryKind, text string) {
	m.entries = append(m.entries, Entry{ID: m.nextLiveEntryID("login"), Kind: kind, Raw: text})
	m.rerenderAndRefreshViewportContent()
}
