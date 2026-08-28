//go:build darwin

package main

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tui"
	"github.com/creack/pty"
	"golang.org/x/sys/unix"
)

const (
	ptyTestTimeout      = 5 * time.Second
	ptyStepTimeout      = 2 * time.Second
	ptyQuietWindow      = 50 * time.Millisecond
	altScreenEnterSeq   = "\x1b[?1049h"
	altScreenExitSeq    = "\x1b[?1049l"
	assistantStreamText = "stream visible from pty smoke backend"
	ctrlCExitStatusText = "Ctrl+C again to exit"
	contextCanceledText = "context canceled"
	footerProfileModel  = "pty-profile/pty-model"
	footerWorkspaceName = "pty-workspace"
	footerSessionMarker = "resize-session-marker-0123456789"
	wideFooterMarker    = footerWorkspaceName + " | " + footerProfileModel + " | tokens 0/0 | " + footerSessionMarker
	narrowFooterMarker  = footerWorkspaceName + " | " + footerProfileModel + " | tokens 0/0"
)

func TestTUIPseudoTerminalResumeLifecycle(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		writeSSE(w, `{"choices":[{"delta":{"content":"unused"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	home := t.TempDir()
	workspace := filepath.Join(t.TempDir(), "pty-resume-workspace")
	if err := os.Mkdir(workspace, 0o755); err != nil {
		t.Fatal(err)
	}
	root := filepath.Join(home, ".otto", "sessions")
	currentPath := createPTYSession(t, root, workspace, "pty-current-session", "current transcript marker")
	selectedPath := createPTYSession(t, root, workspace, "pty-selected-session", "selected transcript marker")
	now := time.Now()
	setCLISessionMTime(t, currentPath, now)
	setCLISessionMTime(t, selectedPath, now.Add(-time.Hour))
	configPath := writeCLIConfig(t, "openai-compatible", "PTY_TEST_KEY", server.URL)

	master, slave, err := pty.Open()
	if err != nil {
		t.Fatalf("pty.Open() error = %v", err)
	}
	initialMode, err := unix.IoctlGetTermios(int(slave.Fd()), unix.TIOCGETA)
	if err != nil {
		t.Fatalf("read initial terminal mode: %v", err)
	}
	if err := pty.Setsize(slave, &pty.Winsize{Cols: 120, Rows: 30}); err != nil {
		t.Fatalf("pty.Setsize(120x30) error = %v", err)
	}

	collector := newPTYOutputCollector(master)
	runCtx, cancelRun := context.WithCancel(context.Background())
	var stderr bytes.Buffer
	runResult := startRunResult(func() error {
		code := run(runCtx, []string{"--config", configPath, "--cwd", workspace, "--resume", currentPath, "--ui", "tui"}, slave, slave, &stderr, testGetenv(map[string]string{
			"HOME": home, "SHELL": "/bin/sh", "PTY_TEST_KEY": "offline-test-key",
		}))
		if code != 0 {
			return fmt.Errorf("run exit code %d: %s", code, stderr.String())
		}
		return nil
	})

	t.Cleanup(func() {
		cancelRun()
		_ = slave.Close()
		_ = master.Close()
		collector.Wait(t, ptyStepTimeout)
		if runResult.Finished() {
			return
		}
		if err, ok := runResult.Wait(ptyStepTimeout); !ok {
			t.Log("forced TUI cleanup timed out")
		} else if err != nil && !isExpectedCleanupRunError(err) {
			t.Logf("forced TUI cleanup error: %v", err)
		}
	})

	waitForSubsequence(t, collector, 0, altScreenEnterSeq)
	waitForSubsequence(t, collector, 0, "current transcript marker")
	writePTY(t, master, "/resume\r")
	waitForSubsequence(t, collector, 0, "Resume Session")
	waitForSubsequence(t, collector, 0, "selected transcript marker")

	writePTY(t, master, "\x1b[B\r")
	selectedOffset := collector.Len()
	waitForSubsequence(t, collector, selectedOffset, "selected transcript marker")
	waitForSubsequence(t, collector, selectedOffset, "pty-selected-session")

	resizeOffset := collector.Len()
	if err := pty.Setsize(slave, &pty.Winsize{Cols: 82, Rows: 24}); err != nil {
		t.Fatalf("pty.Ptysize(82x24) error = %v", err)
	}
	if err := syscall.Kill(os.Getpid(), syscall.SIGWINCH); err != nil {
		t.Fatalf("SIGWINCH error = %v", err)
	}
	waitForSubsequence(t, collector, resizeOffset, filepath.Base(workspace))

	writePTY(t, master, "/exit\r")
	waitForRunReturn(t, runResult)
	waitForSubsequence(t, collector, 0, altScreenExitSeq)

	output := collector.Snapshot()
	enters, exits := bytes.Count(output, []byte(altScreenEnterSeq)), bytes.Count(output, []byte(altScreenExitSeq))
	if enters != 1 || exits != 1 {
		t.Fatalf("alternate-screen sequences enter=%d exit=%d, want 1/1; tail: %s", enters, exits, tailTerminalOutput(output))
	}
	restoredMode, err := unix.IoctlGetTermios(int(slave.Fd()), unix.TIOCGETA)
	if err != nil {
		t.Fatalf("read restored terminal mode: %v", err)
	}
	const interactiveMode = unix.ICANON | unix.ECHO
	if restoredMode.Lflag&interactiveMode != initialMode.Lflag&interactiveMode {
		t.Fatalf("terminal mode leaked: initial lflag=%#x restored lflag=%#x", initialMode.Lflag, restoredMode.Lflag)
	}
	t.Logf("PTY escape evidence: alt-screen enter=%d exit=%d; ICANON|ECHO initial=%#x restored=%#x", enters, exits, initialMode.Lflag&interactiveMode, restoredMode.Lflag&interactiveMode)
}

func createPTYSession(t *testing.T, root, workspace, id, transcript string) string {
	t.Helper()
	store, err := session.Create(root, session.Header{
		Version: session.CurrentVersion, ID: id, Workspace: workspace, Provider: "openai-compatible",
		Profile: "test", Model: "test-model", CreatedAt: time.Now().UTC(),
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := store.Append(context.Background(), model.Message{
		Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: transcript}}, CreatedAt: time.Now().UTC(),
	}); err != nil {
		_ = store.Close()
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestTUIPseudoTerminalLifecycle(t *testing.T) {
	master, slave, err := pty.Open()
	if err != nil {
		t.Fatalf("pty.Open() error = %v", err)
	}

	collector := newPTYOutputCollector(master)
	runCtx, cancelRun := context.WithCancel(context.Background())
	backend := &ptySmokeBackend{
		promptCh:   make(chan string, 1),
		canceledCh: make(chan struct{}),
	}
	runResult := startRunResult(func() error { return tui.Run(runCtx, slave, slave, backend) })

	t.Cleanup(func() {
		cancelRun()
		_ = slave.Close()
		_ = master.Close()
		collector.Wait(t, ptyStepTimeout)
		if runResult.Finished() {
			return
		}
		if err, ok := runResult.Wait(ptyStepTimeout); !ok {
			t.Log("tui.Run cleanup timed out")
		} else if err != nil && !isExpectedCleanupRunError(err) {
			t.Logf("tui.Run cleanup error: %v", err)
		}
	})

	if err := pty.Setsize(slave, &pty.Winsize{Cols: 100, Rows: 30}); err != nil {
		t.Fatalf("pty.Setsize(100x30) error = %v", err)
	}

	waitForSubsequence(t, collector, 0, altScreenEnterSeq)
	waitForSubsequence(t, collector, 0, wideFooterMarker)

	writePTY(t, master, "lifecycle prompt\r")
	waitForPrompt(t, backend, "lifecycle prompt")
	streamOffset := waitForSubsequence(t, collector, 0, assistantStreamText)

	resizeOffset := collector.Len()
	if err := pty.Setsize(slave, &pty.Winsize{Cols: 80, Rows: 24}); err != nil {
		t.Fatalf("pty.Setsize(80x24) error = %v", err)
	}
	if err := syscall.Kill(os.Getpid(), syscall.SIGWINCH); err != nil {
		t.Fatalf("SIGWINCH error = %v", err)
	}
	resizeOutput := waitForStableSnapshot(t, collector, resizeOffset, narrowFooterMarker)
	if strings.Contains(resizeOutput, footerSessionMarker) {
		t.Fatalf("resize output = %s, want collapsed footer without stale session marker %q", tailTerminalOutput([]byte(resizeOutput)), footerSessionMarker)
	}

	writePTY(t, master, "\x1b")
	waitForCancellation(t, backend)
	waitForSubsequence(t, collector, streamOffset, contextCanceledText)

	writePTY(t, master, "\x03")
	waitForSubsequence(t, collector, 0, ctrlCExitStatusText)

	writePTY(t, master, "\x03")
	waitForRunReturn(t, runResult)
	waitForSubsequence(t, collector, 0, altScreenExitSeq)
}

type ptySmokeBackend struct {
	promptCh   chan string
	canceledCh chan struct{}
	cancelOnce sync.Once
}

func (b *ptySmokeBackend) Prompt(ctx context.Context, text string, emit func(agent.Event)) error {
	b.promptCh <- text
	emit(agent.Event{Type: agent.EventTextDelta, Text: assistantStreamText})
	<-ctx.Done()
	b.cancelOnce.Do(func() { close(b.canceledCh) })
	return ctx.Err()
}

func (b *ptySmokeBackend) NewSession() error { return nil }

func (b *ptySmokeBackend) Info() app.Info {
	return app.Info{
		Workspace: "/tmp/" + footerWorkspaceName,
		Profile:   "pty-profile",
		Model:     "pty-model",
		SessionID: footerSessionMarker,
	}
}

func (b *ptySmokeBackend) History() []model.Message { return nil }

func waitForPrompt(t *testing.T, backend *ptySmokeBackend, want string) {
	t.Helper()
	select {
	case got := <-backend.promptCh:
		if got != want {
			t.Fatalf("prompt = %q, want %q", got, want)
		}
	case <-time.After(ptyStepTimeout):
		t.Fatal("timed out waiting for prompt")
	}
}

func waitForCancellation(t *testing.T, backend *ptySmokeBackend) {
	t.Helper()
	select {
	case <-backend.canceledCh:
	case <-time.After(ptyStepTimeout):
		t.Fatal("timed out waiting for backend cancellation")
	}
}

func waitForRunReturn(t *testing.T, run *runResult) {
	t.Helper()
	err, ok := run.Wait(ptyTestTimeout)
	if !ok {
		t.Fatal("timed out waiting for tui.Run to return")
	}
	if err != nil {
		t.Fatalf("tui.Run() error = %v", err)
	}
}

func writePTY(t *testing.T, file *os.File, text string) {
	t.Helper()
	errCh := make(chan error, 1)
	go func() {
		_, err := io.WriteString(file, text)
		errCh <- err
	}()
	select {
	case err := <-errCh:
		if err != nil {
			t.Fatalf("write %q error = %v", text, err)
		}
	case <-time.After(ptyStepTimeout):
		t.Fatalf("timed out writing %q", text)
	}
}

func waitForSubsequence(t *testing.T, collector *ptyOutputCollector, after int, want string) int {
	t.Helper()
	offset, err := collector.WaitFor(after, []byte(want), ptyStepTimeout)
	if err != nil {
		t.Fatal(err)
	}
	return offset
}

func waitForStableSnapshot(t *testing.T, collector *ptyOutputCollector, after int, want string) string {
	t.Helper()
	snapshot, err := collector.WaitForStableSnapshot(after, []byte(want), ptyQuietWindow, ptyStepTimeout)
	if err != nil {
		t.Fatal(err)
	}
	return snapshot
}

type runResult struct {
	done chan struct{}
	mu   sync.Mutex
	err  error
}

func startRunResult(run func() error) *runResult {
	result := &runResult{done: make(chan struct{})}
	go func() {
		err := run()
		result.mu.Lock()
		result.err = err
		result.mu.Unlock()
		close(result.done)
	}()
	return result
}

func (r *runResult) Finished() bool {
	select {
	case <-r.done:
		return true
	default:
		return false
	}
}

func (r *runResult) Wait(timeout time.Duration) (error, bool) {
	select {
	case <-r.done:
		r.mu.Lock()
		defer r.mu.Unlock()
		return r.err, true
	case <-time.After(timeout):
		return nil, false
	}
}

func isExpectedCleanupRunError(err error) bool {
	return errors.Is(err, context.Canceled) || strings.Contains(err.Error(), context.Canceled.Error())
}

type ptyOutputCollector struct {
	mu   sync.Mutex
	buf  []byte
	done chan struct{}
	err  error
}

func newPTYOutputCollector(master *os.File) *ptyOutputCollector {
	collector := &ptyOutputCollector{done: make(chan struct{})}
	go collector.readLoop(master)
	return collector
}

func (c *ptyOutputCollector) readLoop(master *os.File) {
	defer close(c.done)
	chunk := make([]byte, 4096)
	for {
		n, err := master.Read(chunk)
		if n > 0 {
			c.mu.Lock()
			c.buf = append(c.buf, chunk[:n]...)
			c.mu.Unlock()
		}
		if err != nil {
			c.mu.Lock()
			c.err = err
			c.mu.Unlock()
			return
		}
	}
}

func (c *ptyOutputCollector) Len() int {
	c.mu.Lock()
	defer c.mu.Unlock()
	return len(c.buf)
}

func (c *ptyOutputCollector) Snapshot() []byte {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]byte(nil), c.buf...)
}

func (c *ptyOutputCollector) WaitFor(after int, want []byte, timeout time.Duration) (int, error) {
	deadline := time.Now().Add(timeout)
	for {
		c.mu.Lock()
		start := min(max(after, 0), len(c.buf))
		if index := bytes.Index(c.buf[start:], want); index >= 0 {
			offset := start + index
			c.mu.Unlock()
			return offset, nil
		}
		snapshot := append([]byte(nil), c.buf...)
		readErr := c.err
		c.mu.Unlock()

		select {
		case <-c.done:
			return -1, fmt.Errorf("output closed while waiting for %q: %v\nlast output: %s", want, readErr, tailTerminalOutput(snapshot))
		default:
		}
		if time.Now().After(deadline) {
			return -1, fmt.Errorf("timed out waiting for %q\nlast output: %s", want, tailTerminalOutput(snapshot))
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func (c *ptyOutputCollector) WaitForStableSnapshot(after int, want []byte, quietWindow, timeout time.Duration) (string, error) {
	deadline := time.Now().Add(timeout)
	stableSince := time.Time{}
	lastLen := -1
	for {
		c.mu.Lock()
		start := min(max(after, 0), len(c.buf))
		index := bytes.LastIndex(c.buf[start:], want)
		matchOffset := -1
		if index >= 0 {
			matchOffset = start + index
			if len(c.buf) != lastLen {
				lastLen = len(c.buf)
				stableSince = time.Now()
			} else if !stableSince.IsZero() && time.Since(stableSince) >= quietWindow {
				snapshot := string(append([]byte(nil), c.buf[matchOffset:]...))
				c.mu.Unlock()
				return snapshot, nil
			}
		}
		snapshot := append([]byte(nil), c.buf...)
		readErr := c.err
		closed := false
		select {
		case <-c.done:
			closed = true
		default:
		}
		c.mu.Unlock()

		if closed {
			if index >= 0 {
				return string(snapshot[matchOffset:]), nil
			}
			return "", fmt.Errorf("output closed while waiting for stable %q: %v\nlast output: %s", want, readErr, tailTerminalOutput(snapshot))
		}
		if time.Now().After(deadline) {
			return "", fmt.Errorf("timed out waiting for stable %q\nlast output: %s", want, tailTerminalOutput(snapshot))
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func (c *ptyOutputCollector) Wait(t *testing.T, timeout time.Duration) {
	t.Helper()
	select {
	case <-c.done:
	case <-time.After(timeout):
		t.Fatal("pty output collector did not exit")
	}
}

func tailTerminalOutput(output []byte) string {
	const maxTail = 512
	if len(output) > maxTail {
		output = output[len(output)-maxTail:]
	}
	return fmt.Sprintf("%q", output)
}

func TestPTYOutputCollectorWaitForStableSnapshotIncludesTrailingFooterBytes(t *testing.T) {
	collector := &ptyOutputCollector{done: make(chan struct{})}

	go func() {
		collector.mu.Lock()
		collector.buf = append(collector.buf, []byte(narrowFooterMarker)...)
		collector.mu.Unlock()

		time.Sleep(10 * time.Millisecond)

		collector.mu.Lock()
		collector.buf = append(collector.buf, []byte(" | "+footerSessionMarker)...)
		collector.mu.Unlock()
	}()

	snapshot, err := collector.WaitForStableSnapshot(0, []byte(narrowFooterMarker), 20*time.Millisecond, time.Second)
	if err != nil {
		t.Fatalf("WaitForStableSnapshot() error = %v", err)
	}
	if !strings.Contains(snapshot, footerSessionMarker) {
		t.Fatalf("snapshot = %s, want trailing footer marker %q", tailTerminalOutput([]byte(snapshot)), footerSessionMarker)
	}
}

func TestPTYOutputCollectorWaitForStableSnapshotStartsAtMatchedMarker(t *testing.T) {
	collector := &ptyOutputCollector{done: make(chan struct{})}

	go func() {
		collector.mu.Lock()
		collector.buf = append(collector.buf, []byte("wide redraw before match: "+wideFooterMarker+" | ")...)
		collector.mu.Unlock()

		time.Sleep(10 * time.Millisecond)

		collector.mu.Lock()
		collector.buf = append(collector.buf, []byte(narrowFooterMarker)...)
		collector.mu.Unlock()
	}()

	snapshot, err := collector.WaitForStableSnapshot(0, []byte(narrowFooterMarker), 20*time.Millisecond, time.Second)
	if err != nil {
		t.Fatalf("WaitForStableSnapshot() error = %v", err)
	}
	if !strings.HasPrefix(snapshot, narrowFooterMarker) {
		t.Fatalf("snapshot = %s, want prefix %q", tailTerminalOutput([]byte(snapshot)), narrowFooterMarker)
	}
	if strings.Contains(snapshot, wideFooterMarker) {
		t.Fatalf("snapshot = %s, want to exclude pre-match wide footer %q", tailTerminalOutput([]byte(snapshot)), wideFooterMarker)
	}
}
