// Package agent runs one ACP agent child process and exposes the session
// operations the bridge needs. The process is started on first use and
// restarted on the next use after it exits.
package agent

import (
	"context"
	"errors"
	"log/slog"
	"strings"
	"sync"
	"time"

	"github.com/coder/acp-go-sdk"
)

const (
	stderrLines  = 20
	minDelay     = time.Second
	maxDelay     = 60 * time.Second
	resetAfter   = 60 * time.Second
	exitWait     = 5 * time.Second
	closeTimeout = 10 * time.Second
)

// ErrNotOpen is returned by Prompt when the session is not open in the
// current agent process, for example after a restart. Call Load first.
var ErrNotOpen = errors.New("session is not open in the agent process")

// ErrClosed is returned after Close.
var ErrClosed = errors.New("agent is closed")

// ExitError reports that the agent process exited while a call was in
// flight (or before it could start). Stderr holds the last 20 lines.
type ExitError struct {
	Status string
	Stderr []string
}

func (e *ExitError) Error() string {
	s := "agent process exited (" + e.Status + ")"
	if len(e.Stderr) > 0 {
		s += "; last stderr lines:\n" + strings.Join(e.Stderr, "\n")
	}
	return s
}

// Handler receives what the agent sends for a session. Update must not
// block: the SDK delivers notifications on one goroutine with a 1024-entry
// queue and closes the connection when it overflows. Permission may block;
// the SDK runs each request on its own goroutine.
type Handler interface {
	Update(sessionID string, u acp.SessionUpdate)
	Permission(ctx context.Context, sessionID string, req acp.RequestPermissionRequest) acp.RequestPermissionResponse
}

// Options configure an Agent. Zero values of the hooks select production
// behavior.
type Options struct {
	Command []string // program and arguments
	Dir     string   // working directory of the process and cwd of every session; absolute
	Env     []string // child environment; nil inherits the connector's

	// Test hooks.
	Now         func() time.Time
	Sleep       func(ctx context.Context, d time.Duration) error
	CloseWait   time.Duration // time to wait after closing stdin before kill; default 10 s
	StderrLevel slog.Level    // level of agent stderr log lines; default info
}

// Agent is one ACP agent child process, safe for concurrent use.
type Agent struct {
	opts Options

	mu      sync.Mutex // guards the fields below; held while starting a process
	handler Handler
	cur     *process
	delay   time.Duration // restart delay owed after the last exit
	closed  bool
}

// New returns an Agent that has not started a process.
func New(opts Options) *Agent {
	if opts.Now == nil {
		opts.Now = time.Now
	}
	if opts.Sleep == nil {
		opts.Sleep = sleep
	}
	if opts.CloseWait == 0 {
		opts.CloseWait = closeTimeout
	}
	return &Agent{opts: opts}
}

// SetHandler must be called before the first session operation.
func (a *Agent) SetHandler(h Handler) {
	a.mu.Lock()
	a.handler = h
	a.mu.Unlock()
}

// New creates a session and returns its id.
func (a *Agent) New(ctx context.Context) (string, error) {
	p, err := a.process(ctx)
	if err != nil {
		return "", err
	}
	resp, err := p.conn.NewSession(ctx, acp.NewSessionRequest{Cwd: a.opts.Dir, McpServers: []acp.McpServer{}})
	if err != nil {
		return "", p.wrap(err)
	}
	p.setOpen(string(resp.SessionId))
	return string(resp.SessionId), nil
}

// Load opens session id in the current process; it returns nil at once when
// it is already open. Updates for id are dropped until the call returns,
// because they are the replayed history. An error that is not an *ExitError
// means the agent refused the load.
func (a *Agent) Load(ctx context.Context, id string) error {
	p, err := a.process(ctx)
	if err != nil {
		return err
	}
	if !p.beginLoad(id) {
		return nil
	}
	// The SDK returns the response after all earlier notifications went
	// through SessionUpdate, so no replayed update arrives after this.
	_, err = p.conn.LoadSession(ctx, acp.LoadSessionRequest{SessionId: acp.SessionId(id), Cwd: a.opts.Dir, McpServers: []acp.McpServer{}})
	p.endLoad(id, err == nil)
	return p.wrap(err)
}

// List returns the first page of session/list for the agent's workspace. The
// agent decides the order and the page size (otto: newest first, 20).
func (a *Agent) List(ctx context.Context) ([]acp.SessionInfo, error) {
	p, err := a.process(ctx)
	if err != nil {
		return nil, err
	}
	resp, err := p.conn.ListSessions(ctx, acp.ListSessionsRequest{Cwd: &a.opts.Dir})
	if err != nil {
		return nil, p.wrap(err)
	}
	return resp.Sessions, nil
}

// Prompt sends text to session id and waits for the turn to end. ctx must
// not be cancelled to stop a turn: the agent ignores the SDK's request
// cancellation, so use Cancel and keep waiting.
func (a *Agent) Prompt(ctx context.Context, id, text string) (acp.StopReason, error) {
	p := a.live()
	if p == nil || !p.isOpen(id) {
		return "", ErrNotOpen
	}
	resp, err := p.conn.Prompt(ctx, acp.PromptRequest{SessionId: acp.SessionId(id), Prompt: []acp.ContentBlock{acp.TextBlock(text)}})
	if err != nil {
		return "", p.wrap(err)
	}
	return resp.StopReason, nil
}

// Cancel sends session/cancel. It does nothing when no process is running.
func (a *Agent) Cancel(ctx context.Context, id string) error {
	p := a.live()
	if p == nil {
		return nil
	}
	return p.wrap(p.conn.Cancel(ctx, acp.CancelNotification{SessionId: acp.SessionId(id)}))
}

// Kill sends SIGKILL to the running process, if any. The next operation
// starts a new process.
func (a *Agent) Kill() {
	if p := a.live(); p != nil {
		_ = p.cmd.Process.Kill()
		<-p.exited
	}
}

// Close closes the process's stdin, waits up to CloseWait for it to exit,
// then kills it. Later calls return ErrClosed.
func (a *Agent) Close() error {
	a.mu.Lock()
	a.closed = true
	p := a.cur
	a.mu.Unlock()
	if p == nil {
		return nil
	}
	_ = p.stdin.Close()
	select {
	case <-p.exited:
	case <-time.After(a.opts.CloseWait):
		slog.Warn("agent did not exit after stdin closed; killing", "wait", a.opts.CloseWait)
		_ = p.cmd.Process.Kill()
		<-p.exited
	}
	return nil
}

// live returns the running process, or nil.
func (a *Agent) live() *process {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.cur == nil || a.cur.isDone() {
		return nil
	}
	return a.cur
}

// process returns the running process, starting one first when needed. The
// restart waits until the previous process's exit time plus the current
// delay.
func (a *Agent) process(ctx context.Context) (*process, error) {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.closed {
		return nil, ErrClosed
	}
	if a.cur != nil {
		if !a.cur.isDone() {
			return a.cur, nil
		}
		ended := a.cur.endedAt
		a.delay = restartDelay(a.delay, ended.Sub(a.cur.startedAt))
		a.cur = nil
		if wait := ended.Add(a.delay).Sub(a.opts.Now()); a.delay > 0 && wait > 0 {
			slog.Info("restarting agent after delay", "delay", wait)
			if err := a.opts.Sleep(ctx, wait); err != nil {
				return nil, err
			}
		}
	}
	return a.start(ctx)
}
