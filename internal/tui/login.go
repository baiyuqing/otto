package tui

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/auth"
	"github.com/baiyuqing/otto/internal/config"
)

// Seams so tests can supply credentials and a temp path without a browser,
// network, or the real home directory.
var (
	authLoginFn        = auth.Login
	authPathFn         = auth.DefaultPath
	errTUILoginFailed  = errors.New("chatgpt sign-in failed")
	errTUILogoutFailed = errors.New("stored chatgpt credentials could not be removed")
)

// loginResultMsg carries one step of the async login flow: an authorization URL
// notice (done=false) or the final outcome (done=true).
type loginResultMsg struct {
	generation uint64
	url        string
	err        error
	done       bool
}

// handleLoginCommand runs "/login" and "/login status". Login is asynchronous:
// auth.Login blocks on the browser callback, so it runs in a worker goroutine
// that streams the URL notice and the final result back as messages. On success
// it only saves credentials; using the subscription still requires starting a
// session on the chatgpt provider.
func (m Model) handleLoginCommand(argument string) (tea.Model, tea.Cmd) {
	switch argument {
	case "status":
		path, err := authPathFn()
		if err != nil {
			m.statusText = auth.ErrCredentialsUnavailable.Error()
			return m, nil
		}
		line, _ := auth.StatusLine(path)
		m.clearEditor()
		m.statusText = ""
		m.appendLoginEntry(EntrySystem, line)
		return m, nil
	case "":
		if provider := m.backend.Info().Provider; provider != config.ProviderChatGPT {
			m.clearEditor()
			m.statusText = ""
			m.appendLoginEntry(EntrySystem, fmt.Sprintf("The %s provider authenticates with an API key from the environment; there is nothing to sign in to.", provider))
			return m, nil
		}
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
		return m, startLoginCommand(m.rootCtx, m.loginGeneration, channel)
	default:
		m.statusText = "usage: /login [status]"
		return m, nil
	}
}

func (m Model) handleLogoutCommand() (tea.Model, tea.Cmd) {
	path, err := authPathFn()
	if err != nil {
		m.statusText = auth.ErrCredentialsUnavailable.Error()
		return m, nil
	}
	m.clearEditor()
	m.statusText = ""
	if err := os.Remove(path); err != nil {
		if errors.Is(err, os.ErrNotExist) {
			m.appendLoginEntry(EntrySystem, "Not signed in to ChatGPT.")
			return m, nil
		}
		m.statusText = errTUILogoutFailed.Error()
		m.appendLoginEntry(EntryError, errTUILogoutFailed.Error())
		return m, nil
	}
	m.appendLoginEntry(EntrySystem, "Signed out of ChatGPT.")
	return m, nil
}

func startLoginCommand(ctx context.Context, generation uint64, channel chan loginResultMsg) tea.Cmd {
	return func() tea.Msg {
		go runLoginWorker(ctx, generation, channel)
		return waitLogin(generation, channel)()
	}
}

func runLoginWorker(ctx context.Context, generation uint64, channel chan loginResultMsg) {
	defer close(channel)
	opener := func(url string) error {
		channel <- loginResultMsg{generation: generation, url: url}
		_ = exec.Command("open", url).Start()
		return nil
	}
	creds, err := authLoginFn(rootContext(ctx), opener)
	if err != nil {
		if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
			channel <- loginResultMsg{generation: generation, err: err, done: true}
			return
		}
		channel <- loginResultMsg{generation: generation, err: errTUILoginFailed, done: true}
		return
	}
	path, err := authPathFn()
	if err != nil {
		channel <- loginResultMsg{generation: generation, err: auth.ErrCredentialsUnavailable, done: true}
		return
	}
	if err := creds.Save(path); err != nil {
		channel <- loginResultMsg{generation: generation, err: auth.ErrCredentialsPersistence, done: true}
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
	if !msg.done {
		m.appendLoginEntry(EntrySystem, fmt.Sprintf("Open this URL to sign in:\n\n  %s", msg.url))
		return m, waitLogin(m.loginGeneration, m.loginChannel)
	}
	m.loginPending = false
	m.loginChannel = nil
	if msg.err != nil {
		m.statusText = msg.err.Error()
		m.appendLoginEntry(EntryError, fmt.Sprintf("login failed: %v", msg.err))
		return m, nil
	}
	m.statusText = ""
	m.appendLoginEntry(EntrySystem, "Signed in to ChatGPT. Start a new session on the chatgpt provider to use it.")
	return m, nil
}

func (m *Model) appendLoginEntry(kind EntryKind, text string) {
	m.entries = append(m.entries, Entry{ID: m.nextLiveEntryID("login"), Kind: kind, Raw: text})
	m.rerenderAndRefreshViewportContent(!m.autoFollow)
}
