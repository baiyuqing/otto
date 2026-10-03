package agent

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"os"
	"os/exec"
	"sync"
	"time"

	"github.com/coder/acp-go-sdk"
)

// restartDelay returns the delay before the next start. prev is the delay
// applied before the process that just exited and ran is how long that
// process ran: a run of resetAfter or longer clears the delay; a shorter one
// doubles it from minDelay up to maxDelay.
func restartDelay(prev, ran time.Duration) time.Duration {
	switch {
	case ran >= resetAfter:
		return 0
	case prev == 0:
		return minDelay
	default:
		return min(prev*2, maxDelay)
	}
}

func sleep(ctx context.Context, d time.Duration) error {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-t.C:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

// process is one running (or exited) child.
type process struct {
	cmd   *exec.Cmd
	conn  *acp.ClientSideConnection
	stdin io.WriteCloser
	// writeFailed is closed when a write to stdin fails. The SDK reports
	// that as an ordinary internal error, possibly before the exit or the
	// end of stdout is observed.
	writeFailed chan struct{}
	tail        *tail
	exited      chan struct{} // closed after Wait returns and endedAt is set

	startedAt time.Time
	// memoryReview is set from the initialize response.
	memoryReview     bool
	approvalDialogue bool
	// endedAt and waitErr are valid after exited is closed.
	endedAt time.Time
	waitErr error

	mu      sync.Mutex
	open    map[string]bool
	loading map[string]bool
}

func (p *process) isDone() bool {
	select {
	case <-p.exited:
		return true
	default:
		return false
	}
}

func (p *process) isOpen(id string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.open[id]
}

func (p *process) setOpen(id string) {
	p.mu.Lock()
	p.open[id] = true
	p.mu.Unlock()
}

// beginLoad reports false when id is already open; otherwise it marks id as
// loading.
func (p *process) beginLoad(id string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.open[id] {
		return false
	}
	p.loading[id] = true
	return true
}

func (p *process) endLoad(id string, ok bool) {
	p.mu.Lock()
	delete(p.loading, id)
	if ok {
		p.open[id] = true
	}
	p.mu.Unlock()
}

func (p *process) isLoading(id string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.loading[id]
}

// wrap turns the error of a failed call into an *ExitError when the process
// has exited (or its stdout closed), and returns err unchanged otherwise.
func (p *process) wrap(err error) error {
	if err == nil {
		return nil
	}
	select {
	case <-p.conn.Done():
	case <-p.exited:
	case <-p.writeFailed:
	default:
		return err
	}
	select {
	case <-p.exited:
	case <-time.After(exitWait):
		return err
	}
	return p.exitError()
}

func (p *process) exitError() *ExitError {
	status := "exit status 0"
	if p.waitErr != nil {
		status = p.waitErr.Error()
	}
	return &ExitError{Remote: p.cmd == nil, Status: status, Stderr: p.tail.lines()}
}

// start launches the command and runs the ACP initialize handshake. On a
// handshake failure the process is left in a.cur, exited, so the next call
// applies the restart delay. a.mu is held.
func (a *Agent) start(ctx context.Context) (*process, error) {
	if a.opts.Address != "" {
		return a.connect(ctx)
	}
	if len(a.opts.Command) == 0 {
		return nil, errors.New("agent command is empty")
	}
	cmd := exec.Command(a.opts.Command[0], a.opts.Command[1:]...)
	cmd.Dir = a.opts.Dir
	cmd.Env = a.opts.Env
	p := &process{cmd: cmd, exited: make(chan struct{}), writeFailed: make(chan struct{}), open: map[string]bool{}, loading: map[string]bool{}}
	p.tail = &tail{level: a.opts.StderrLevel}
	cmd.Stderr = p.tail
	var err error
	if p.stdin, err = cmd.StdinPipe(); err != nil {
		return nil, err
	}
	// An os.Pipe instead of StdoutPipe: Wait closes StdoutPipe's reader when
	// the process exits, which can discard output the SDK has not read yet.
	stdoutR, stdoutW, err := os.Pipe()
	if err != nil {
		return nil, err
	}
	cmd.Stdout = stdoutW
	p.startedAt = a.opts.Now()
	if err := cmd.Start(); err != nil {
		stdoutR.Close()
		stdoutW.Close()
		return nil, fmt.Errorf("start agent: %w", err)
	}
	stdoutW.Close()
	slog.Info("agent started", "pid", cmd.Process.Pid, "command", a.opts.Command[0])
	go func() {
		p.waitErr = cmd.Wait()
		p.endedAt = a.opts.Now()
		close(p.exited)
		slog.Info("agent exited", "status", p.exitError().Status)
	}()
	// No SetLogger: the receive goroutine reads the logger as soon as the
	// connection exists, and the SDK's default is slog.Default().
	p.conn = acp.NewClientSideConnection(&client{a: a, p: p, handler: a.handler}, &stdinWriter{p: p}, stdoutR)
	go func() {
		<-p.conn.Done()
		stdoutR.Close()
	}()
	a.cur = p

	return a.initialize(ctx, p)
}

func (a *Agent) initialize(ctx context.Context, p *process) (*process, error) {
	resp, err := p.conn.Initialize(ctx, acp.InitializeRequest{
		ProtocolVersion: acp.ProtocolVersionNumber,
		ClientInfo:      &acp.Implementation{Name: "otto-connect"},
	})
	if err == nil && resp.ProtocolVersion != acp.ProtocolVersionNumber {
		err = fmt.Errorf("agent speaks ACP version %d, need %d", resp.ProtocolVersion, acp.ProtocolVersionNumber)
	}
	if err != nil {
		err = p.wrap(err)
		_ = p.stdin.Close()
		if p.cmd != nil {
			_ = p.cmd.Process.Kill()
		}
		<-p.exited
		var ee *ExitError
		if !errors.As(err, &ee) {
			return nil, fmt.Errorf("initialize agent: %w", err)
		}
		return nil, err
	}
	p.memoryReview = advertisesMemoryReview(resp.AgentCapabilities.Meta)
	otto, _ := resp.AgentCapabilities.Meta["otto"].(map[string]any)
	p.approvalDialogue, _ = otto["approvalDialogue"].(bool)
	return p, nil
}

// advertisesMemoryReview reads _meta.otto.memoryReview.
func advertisesMemoryReview(meta map[string]any) bool {
	otto, _ := meta["otto"].(map[string]any)
	on, _ := otto["memoryReview"].(bool)
	return on
}

// stdinWriter writes to the process's stdin and closes writeFailed on the
// first failed write.
type stdinWriter struct {
	p    *process
	once sync.Once
}

func (w *stdinWriter) Write(b []byte) (int, error) {
	n, err := w.p.stdin.Write(b)
	if err != nil {
		w.once.Do(func() { close(w.p.writeFailed) })
	}
	return n, err
}

// client implements acp.Client for one process.
type client struct {
	a       *Agent
	p       *process
	handler Handler
}

func (c *client) SessionUpdate(_ context.Context, n acp.SessionNotification) error {
	if c.handler == nil || c.p.isLoading(string(n.SessionId)) {
		return nil
	}
	c.handler.Update(string(n.SessionId), n.Update)
	return nil
}

func (c *client) RequestPermission(ctx context.Context, r acp.RequestPermissionRequest) (acp.RequestPermissionResponse, error) {
	if c.handler == nil {
		return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeCancelled()}, nil
	}
	return c.handler.Permission(ctx, string(r.SessionId), r), nil
}

var errNoCapability = errors.New("not supported: otto-connect advertises no fs or terminal capability")

func (*client) ReadTextFile(context.Context, acp.ReadTextFileRequest) (acp.ReadTextFileResponse, error) {
	return acp.ReadTextFileResponse{}, errNoCapability
}

func (*client) WriteTextFile(context.Context, acp.WriteTextFileRequest) (acp.WriteTextFileResponse, error) {
	return acp.WriteTextFileResponse{}, errNoCapability
}

func (*client) CreateTerminal(context.Context, acp.CreateTerminalRequest) (acp.CreateTerminalResponse, error) {
	return acp.CreateTerminalResponse{}, errNoCapability
}

func (*client) KillTerminal(context.Context, acp.KillTerminalRequest) (acp.KillTerminalResponse, error) {
	return acp.KillTerminalResponse{}, errNoCapability
}

func (*client) TerminalOutput(context.Context, acp.TerminalOutputRequest) (acp.TerminalOutputResponse, error) {
	return acp.TerminalOutputResponse{}, errNoCapability
}

func (*client) ReleaseTerminal(context.Context, acp.ReleaseTerminalRequest) (acp.ReleaseTerminalResponse, error) {
	return acp.ReleaseTerminalResponse{}, errNoCapability
}

func (*client) WaitForTerminalExit(context.Context, acp.WaitForTerminalExitRequest) (acp.WaitForTerminalExitResponse, error) {
	return acp.WaitForTerminalExitResponse{}, errNoCapability
}

// tail is the io.Writer behind the child's stderr. It keeps the last
// stderrLines complete lines and logs each one.
type tail struct {
	level slog.Level
	mu    sync.Mutex
	buf   []byte
	last  []string
}

func (t *tail) Write(b []byte) (int, error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.buf = append(t.buf, b...)
	for {
		i := bytes.IndexByte(t.buf, '\n')
		if i < 0 {
			break
		}
		t.add(string(bytes.TrimRight(t.buf[:i], "\r")))
		t.buf = t.buf[i+1:]
	}
	return len(b), nil
}

func (t *tail) add(line string) {
	slog.Log(context.Background(), t.level, "agent stderr: "+line)
	t.last = append(t.last, line)
	if len(t.last) > stderrLines {
		t.last = t.last[1:]
	}
}

// lines returns the kept lines, including an unterminated last line.
func (t *tail) lines() []string {
	t.mu.Lock()
	defer t.mu.Unlock()
	out := append([]string(nil), t.last...)
	if len(t.buf) > 0 {
		out = append(out, string(t.buf))
		if len(out) > stderrLines {
			out = out[1:]
		}
	}
	return out
}

// connect opens a transport, never starts or kills the server process.
func (a *Agent) connect(ctx context.Context) (*process, error) {
	address := a.opts.Address
	host, port, err := net.SplitHostPort(address)
	if err != nil {
		return nil, errors.New("invalid ACP address")
	}
	if host == "localhost" {
		address = net.JoinHostPort("127.0.0.1", port)
	} else {
		ip := net.ParseIP(host)
		if ip == nil || !ip.IsLoopback() {
			return nil, errors.New("ACP requires a loopback address")
		}
	}
	if a.opts.Token == "" || len(a.opts.Token) > 4000 {
		return nil, errors.New("invalid ACP token")
	}
	for _, b := range []byte(a.opts.Token) {
		if b < 33 || b > 126 {
			return nil, errors.New("invalid ACP token")
		}
	}
	conn, err := (&net.Dialer{Timeout: 5 * time.Second}).DialContext(ctx, "tcp", address)
	if err != nil {
		return nil, fmt.Errorf("connect ACP: %w", err)
	}
	// A deadline covers authentication plus initialize, then normal turns have no timeout.
	_ = conn.SetDeadline(time.Now().Add(5 * time.Second))
	if _, err := io.WriteString(conn, "Authorization: Bearer "+a.opts.Token+"\n"); err != nil {
		conn.Close()
		return nil, errors.New("ACP authentication write failed")
	}
	p := &process{stdin: conn, exited: make(chan struct{}), writeFailed: make(chan struct{}),
		tail: &tail{level: a.opts.StderrLevel}, startedAt: a.opts.Now(), open: map[string]bool{}, loading: map[string]bool{}}
	p.conn = acp.NewClientSideConnection(&client{a: a, p: p, handler: a.handler}, &stdinWriter{p: p}, conn)
	a.cur = p
	go func() {
		<-p.conn.Done()
		conn.Close()
		p.waitErr = errors.New("ACP connection closed")
		p.endedAt = a.opts.Now()
		close(p.exited)
	}()
	result, err := a.initialize(ctx, p)
	if err == nil {
		_ = conn.SetDeadline(time.Time{})
	}
	return result, err
}
