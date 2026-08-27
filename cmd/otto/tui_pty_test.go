//go:build darwin

package main

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"os"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/tui"
	"github.com/creack/pty"
)

const (
	ptyTestTimeout      = 5 * time.Second
	ptyStepTimeout      = 2 * time.Second
	altScreenEnterSeq   = "\x1b[?1049h"
	altScreenExitSeq    = "\x1b[?1049l"
	assistantStreamText = "stream visible from pty smoke backend"
	ctrlCExitStatusText = "Ctrl+C again to exit"
	contextCanceledText = "context canceled"
	footerProfileModel  = "pty-profile/pty-model"
)

func TestTUIPseudoTerminalLifecycle(t *testing.T) {
	master, slave, err := pty.Open()
	if err != nil {
		t.Fatalf("pty.Open() error = %v", err)
	}

	collector := newPTYOutputCollector(master)
	runCtx, cancelRun := context.WithCancel(context.Background())
	runErrCh := make(chan error, 1)
	backend := &ptySmokeBackend{
		promptCh:   make(chan string, 1),
		canceledCh: make(chan struct{}),
	}

	t.Cleanup(func() {
		cancelRun()
		_ = slave.Close()
		_ = master.Close()
		collector.Wait(t, ptyStepTimeout)
		select {
		case err := <-runErrCh:
			if err != nil && err != context.Canceled {
				t.Logf("tui.Run cleanup error: %v", err)
			}
		case <-time.After(ptyStepTimeout):
			t.Log("tui.Run cleanup timed out")
		}
	})

	if err := pty.Setsize(slave, &pty.Winsize{Cols: 100, Rows: 30}); err != nil {
		t.Fatalf("pty.Setsize(100x30) error = %v", err)
	}

	go func() {
		runErrCh <- tui.Run(runCtx, slave, slave, backend)
	}()

	waitForSubsequence(t, collector, 0, altScreenEnterSeq)
	waitForSubsequence(t, collector, 0, footerProfileModel)

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
	waitForSubsequence(t, collector, max(streamOffset, resizeOffset), footerProfileModel)

	writePTY(t, master, "\x1b")
	waitForCancellation(t, backend)
	waitForSubsequence(t, collector, streamOffset, contextCanceledText)

	writePTY(t, master, "\x03")
	waitForSubsequence(t, collector, 0, ctrlCExitStatusText)

	writePTY(t, master, "\x03")
	waitForRunReturn(t, runErrCh)
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
	return app.Info{Profile: "pty-profile", Model: "pty-model", SessionID: "pty-session"}
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

func waitForRunReturn(t *testing.T, errCh <-chan error) {
	t.Helper()
	select {
	case err := <-errCh:
		if err != nil {
			t.Fatalf("tui.Run() error = %v", err)
		}
	case <-time.After(ptyTestTimeout):
		t.Fatal("timed out waiting for tui.Run to return")
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
