package repl

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"strings"
	"sync"

	"github.com/baiyuqing/otto/internal/agent"
)

const maxInputBytes = 1 << 20

var ErrNewSession = errors.New("start a new session")

type AgentRunner interface {
	Run(context.Context, string, func(agent.Event)) error
}

type Info struct {
	SessionID   string
	SessionPath string
	Provider    string
	Model       string
}

type REPL struct {
	stdin        io.Reader
	stdout       io.Writer
	stderr       io.Writer
	runner       AgentRunner
	info         Info
	mu           sync.Mutex
	activeCancel context.CancelFunc
}

func New(stdin io.Reader, stdout, stderr io.Writer, runner AgentRunner, info Info) *REPL {
	return &REPL{stdin: stdin, stdout: stdout, stderr: stderr, runner: runner, info: info}
}

func (r *REPL) Run(ctx context.Context) error {
	if r.info.SessionID != "" {
		_, _ = fmt.Fprintf(r.stdout, "Session: %s\n", r.info.SessionID)
	}
	scanner := bufio.NewScanner(r.stdin)
	scanner.Buffer(make([]byte, 64*1024), maxInputBytes+1)

	lines := make(chan scanResult)
	ack := make(chan struct{})
	stop := make(chan struct{})
	defer close(stop)
	go scanLines(scanner, lines, ack, stop)

	for {
		_, _ = io.WriteString(r.stdout, "> ")
		var result scanResult
		select {
		case <-ctx.Done():
			return ctx.Err()
		case result = <-lines:
		}
		if !result.ok {
			return result.err
		}

		line := result.text
		trimmed := strings.TrimSpace(line)
		switch {
		case trimmed == "":
			sendAck(ack, stop)
			continue
		case strings.HasPrefix(trimmed, "/"):
			done, err := r.command(trimmed)
			if done || err != nil {
				return err
			}
			sendAck(ack, stop)
			continue
		}

		turnCtx, cancel := context.WithCancel(ctx)
		r.mu.Lock()
		r.activeCancel = cancel
		r.mu.Unlock()

		errorRendered := false
		err := r.runner.Run(turnCtx, line, func(event agent.Event) {
			switch event.Type {
			case agent.EventTextDelta:
				_, _ = io.WriteString(r.stdout, event.Text)
			case agent.EventToolCallStarted:
				_, _ = fmt.Fprintf(r.stdout, "\n[tool] %s (%s)\n", event.ToolName, event.ToolCallID)
			case agent.EventToolCallFinished:
				_, _ = fmt.Fprintf(r.stdout, "[tool result] %s\n", firstLine(event.ToolResult.Content))
			case agent.EventAgentError:
				errorRendered = true
				if event.Err != nil {
					_, _ = fmt.Fprintln(r.stderr, event.Err)
				}
			}
		})

		r.mu.Lock()
		r.activeCancel = nil
		r.mu.Unlock()
		cancel()
		if err != nil && !errorRendered {
			_, _ = fmt.Fprintln(r.stderr, err)
		}
		_, _ = fmt.Fprintln(r.stdout)
		if ctx.Err() != nil {
			return ctx.Err()
		}
		sendAck(ack, stop)
	}
}

func (r *REPL) Interrupt() bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.activeCancel == nil {
		return false
	}
	r.activeCancel()
	r.activeCancel = nil
	return true
}

func (r *REPL) command(command string) (bool, error) {
	switch command {
	case "/help":
		_, _ = io.WriteString(r.stdout, "/help     show commands\n/exit     exit Otto\n/new      start a new session\n/session  show session details\n")
		return false, nil
	case "/exit":
		return true, nil
	case "/new":
		return true, ErrNewSession
	case "/session":
		_, _ = fmt.Fprintf(r.stdout, "ID: %s\nPath: %s\nProvider: %s\nModel: %s\n", r.info.SessionID, r.info.SessionPath, r.info.Provider, r.info.Model)
		return false, nil
	default:
		_, _ = fmt.Fprintf(r.stderr, "unknown command: %s\n", command)
		return false, nil
	}
}

type scanResult struct {
	text string
	ok   bool
	err  error
}

func scanLines(scanner *bufio.Scanner, lines chan<- scanResult, ack <-chan struct{}, stop <-chan struct{}) {
	for scanner.Scan() {
		select {
		case lines <- scanResult{text: scanner.Text(), ok: true}:
		case <-stop:
			return
		}
		select {
		case <-ack:
		case <-stop:
			return
		}
	}
	select {
	case lines <- scanResult{err: scanner.Err()}:
	case <-stop:
	}
}

func sendAck(ack chan<- struct{}, stop <-chan struct{}) {
	select {
	case ack <- struct{}{}:
	case <-stop:
	}
}

func firstLine(content string) string {
	if index := strings.IndexByte(content, '\n'); index >= 0 {
		return content[:index]
	}
	return content
}
