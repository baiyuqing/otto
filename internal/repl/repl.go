package repl

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"strings"
	"sync"
	"unicode"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/session"
)

const maxInputBytes = 1 << 20

type Input struct {
	scanner *bufio.Scanner
}

func NewInput(reader io.Reader) *Input {
	scanner := bufio.NewScanner(reader)
	scanner.Buffer(make([]byte, 64*1024), maxInputBytes+1)
	return &Input{scanner: scanner}
}

type REPL struct {
	input        *Input
	stdout       io.Writer
	stderr       io.Writer
	backend      app.Backend
	mu           sync.Mutex
	activeCancel context.CancelFunc
}

type commandError struct {
	command string
	err     error
}

func (e *commandError) Error() string {
	return e.err.Error()
}

func (e *commandError) Unwrap() error {
	return e.err
}

func IsCommandError(err error, command string) bool {
	var target *commandError
	return errors.As(err, &target) && target.command == command
}

func New(stdin io.Reader, stdout, stderr io.Writer, backend app.Backend) *REPL {
	return NewWithInput(NewInput(stdin), stdout, stderr, backend)
}

func NewWithInput(input *Input, stdout, stderr io.Writer, backend app.Backend) *REPL {
	return &REPL{input: input, stdout: stdout, stderr: stderr, backend: backend}
}

const ottoMark = "(●ᴥ●)  otto"

const logo = `     ____  __  __
    / __ \/ /_/ /____
   / /_/ / __/ __/ __ \
   \____/\__/\__/\____/
`

func (r *REPL) Run(ctx context.Context) error {
	_, _ = io.WriteString(r.stdout, logo)
	if info := r.backend.Info(); info.SessionID != "" {
		_, _ = fmt.Fprintf(r.stdout, "Session: %s\n", info.SessionID)
	}
	lines := make(chan scanResult)
	ack := make(chan struct{})
	stop := make(chan struct{})
	defer close(stop)
	go scanLines(r.input.scanner, lines, ack, stop)

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
			done, err := r.command(ctx, trimmed)
			if done || err != nil {
				return err
			}
			sendAck(ack, stop)
			continue
		}

		err := r.prompt(ctx, line)
		if ctx.Err() != nil {
			return ctx.Err()
		}
		if errors.Is(err, session.ErrFatalPersistence) {
			return err
		}
		sendAck(ack, stop)
	}
}

// RunOnce executes a single prompt with the same rendering as Run and returns
// the turn's error without the interactive banner, prompt marker, or loop.
func (r *REPL) RunOnce(ctx context.Context, prompt string) error {
	return r.prompt(ctx, prompt)
}

func (r *REPL) prompt(ctx context.Context, line string) error {
	errorRendered := false
	err := r.withActiveCancel(ctx, func(turnCtx context.Context) error {
		return r.backend.Prompt(turnCtx, line, func(event agent.Event) {
			if r.renderEvent(event) {
				errorRendered = true
			}
		})
	})
	if err != nil && !errorRendered {
		_, _ = fmt.Fprintln(r.stderr, err)
	}
	_, _ = fmt.Fprintln(r.stdout)
	if ctx.Err() != nil {
		return ctx.Err()
	}
	return err
}

func (r *REPL) compact(ctx context.Context, focus string) (agent.CompactionResult, error) {
	var result agent.CompactionResult
	renderedIDs := make(map[string]struct{})
	renderedNoopEmpty := false
	errorRendered := false
	shouldRenderFallback := func(result agent.CompactionResult) bool {
		if result.CheckpointID != "" {
			if _, ok := renderedIDs[result.CheckpointID]; ok {
				return false
			}
		}
		if result.Noop && result.CheckpointID == "" {
			return !renderedNoopEmpty
		}
		return true
	}
	renderCompaction := func(compaction agent.CompactionEvent) {
		switch {
		case compaction.Noop && compaction.CheckpointID == "":
			if renderedNoopEmpty {
				return
			}
			renderedNoopEmpty = true
		case compaction.CheckpointID != "":
			if _, ok := renderedIDs[compaction.CheckpointID]; ok {
				return
			}
			renderedIDs[compaction.CheckpointID] = struct{}{}
		}
		r.renderCompaction(compaction)
	}
	err := r.withActiveCancel(ctx, func(turnCtx context.Context) error {
		var innerErr error
		result, innerErr = r.backend.Compact(turnCtx, focus, func(event agent.Event) {
			switch event.Type {
			case agent.EventCompactionCompleted:
				if event.Compaction != nil {
					renderCompaction(*event.Compaction)
				}
			default:
				if r.renderEvent(event) {
					errorRendered = true
				}
			}
		})
		return innerErr
	})
	if ctxErr := ctx.Err(); ctxErr != nil {
		return agent.CompactionResult{}, ctxErr
	}
	if err != nil {
		if errors.Is(err, context.Canceled) && !errors.Is(err, session.ErrFatalPersistence) {
			return agent.CompactionResult{}, nil
		}
		if !errorRendered {
			_, _ = fmt.Fprintln(r.stderr, err)
		}
		return agent.CompactionResult{}, &commandError{command: "/compact", err: err}
	}
	if shouldRenderFallback(result) {
		r.renderCompactionResult(result)
	}
	return result, nil
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

func (r *REPL) command(ctx context.Context, command string) (bool, error) {
	name, args, ok := splitCommand(command)
	if !ok {
		_, _ = fmt.Fprintf(r.stderr, "unknown command: %s\n", command)
		return false, nil
	}
	switch name {
	case "help":
		if args != "" {
			break
		}
		_, _ = io.WriteString(r.stdout, "/help     show commands\n/exit     exit Otto\n/new      start a new session\n/session  show session details\n/archive  archive current session and start a new one\n/model [profile] show current model, or switch profiles in a fresh session\n/compact [focus] compact context\n/memory search <query> | /memory forget <id> | /memory review <id> accept|reject\n/remember [--scope user|workspace] [--kind K] [--key K] <text>\n/login [status] sign in to ChatGPT (or show status)\n/logout   sign out of ChatGPT\n")
		return false, nil
	case "exit":
		if args != "" {
			break
		}
		return true, nil
	case "new":
		if args != "" {
			break
		}
		if err := r.backend.NewSession(); err != nil {
			return false, &commandError{command: command, err: err}
		}
		if info := r.backend.Info(); info.SessionID != "" {
			_, _ = fmt.Fprintf(r.stdout, "Session: %s\n", info.SessionID)
		}
		return false, nil
	case "archive":
		if args != "" {
			break
		}
		archiver, ok := r.backend.(app.SessionArchiver)
		if !ok {
			return false, &commandError{command: command, err: app.ErrPersistenceDisabled}
		}
		result, err := archiver.ArchiveCurrentSession(ctx)
		if err != nil {
			return false, &commandError{command: command, err: err}
		}
		_, _ = fmt.Fprintf(r.stdout, "Archived: %s\n", result.Path)
		if info := r.backend.Info(); info.SessionID != "" {
			_, _ = fmt.Fprintf(r.stdout, "Session: %s\n", info.SessionID)
		}
		return false, nil
	case "session":
		if args != "" {
			break
		}
		info := r.backend.Info()
		_, _ = fmt.Fprintf(r.stdout, "ID: %s\nPath: %s\nProvider: %s\nModel: %s\n", info.SessionID, info.SessionPath, info.Provider, info.Model)
		return false, nil
	case "compact":
		focus := strings.TrimSpace(args)
		if _, err := r.compact(ctx, focus); err != nil {
			return false, err
		}
		return false, nil
	case "model":
		return r.modelCommand(ctx, args)
	case "memory":
		return r.memoryCommand(ctx, args)
	case "remember":
		return r.rememberCommand(ctx, args)
	case "login":
		return r.loginCommand(ctx, args)
	case "logout":
		if args != "" {
			break
		}
		return r.logoutCommand()
	}
	_, _ = fmt.Fprintf(r.stderr, "unknown command: %s\n", command)
	return false, nil
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

func (r *REPL) withActiveCancel(ctx context.Context, fn func(context.Context) error) error {
	turnCtx, cancel := context.WithCancel(ctx)
	r.mu.Lock()
	r.activeCancel = cancel
	r.mu.Unlock()
	defer func() {
		r.mu.Lock()
		r.activeCancel = nil
		r.mu.Unlock()
		cancel()
	}()
	return fn(turnCtx)
}

func (r *REPL) renderEvent(event agent.Event) bool {
	switch event.Type {
	case agent.EventTextDelta:
		_, _ = io.WriteString(r.stdout, event.Text)
	case agent.EventToolCallStarted:
		_, _ = fmt.Fprintf(r.stdout, "\n[tool] %s (%s)\n", event.ToolName, event.ToolCallID)
	case agent.EventToolCallFinished:
		_, _ = fmt.Fprintf(r.stdout, "[tool result] %s\n", firstLine(event.ToolResult.Content))
	case agent.EventCompactionCompleted:
		if event.Compaction != nil {
			r.renderCompaction(*event.Compaction)
		}
	case agent.EventCompactionWarning:
		if event.Err != nil {
			_, _ = fmt.Fprintln(r.stderr, event.Err)
		}
	case agent.EventMemoryWarning:
		if event.Err != nil {
			_, _ = fmt.Fprintln(r.stderr, event.Err)
		}
	case agent.EventAgentError:
		if event.Err != nil {
			_, _ = fmt.Fprintln(r.stderr, event.Err)
			return true
		}
	}
	return false
}

func (r *REPL) renderCompaction(compaction agent.CompactionEvent) {
	r.renderCompactionResult(agent.CompactionResult{
		CheckpointID:         compaction.CheckpointID,
		TokensBefore:         compaction.TokensBefore,
		EstimatedTokensAfter: compaction.EstimatedTokensAfter,
		Noop:                 compaction.Noop,
	})
}

func (r *REPL) renderCompactionResult(result agent.CompactionResult) {
	_, _ = io.WriteString(r.stdout, compactionLine(result))
}

func compactionLine(result agent.CompactionResult) string {
	if result.Noop {
		return "\n[context] no-op\n"
	}
	before := formatTokenCount(result.TokensBefore)
	if result.EstimatedTokensAfter > 0 {
		return fmt.Sprintf("\n[context] compacted %s → %s tokens\n", before, formatTokenCount(result.EstimatedTokensAfter))
	}
	return fmt.Sprintf("\n[context] compacted %s tokens\n", before)
}

func formatTokenCount(tokens int) string {
	if tokens < 1000 {
		return fmt.Sprintf("%d", tokens)
	}
	return fmt.Sprintf("%dk", tokens/1000)
}

func splitCommand(command string) (string, string, bool) {
	if !strings.HasPrefix(command, "/") {
		return "", "", false
	}
	body := command[1:]
	if body == "" {
		return "", "", false
	}
	if index := strings.IndexFunc(body, unicode.IsSpace); index >= 0 {
		return body[:index], strings.TrimSpace(body[index:]), true
	}
	return body, "", true
}

func firstLine(content string) string {
	if index := strings.IndexByte(content, '\n'); index >= 0 {
		return content[:index]
	}
	return content
}
