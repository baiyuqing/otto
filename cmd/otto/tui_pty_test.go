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
	ptyTestTimeout              = 5 * time.Second
	ptyStepTimeout              = 2 * time.Second
	ptyQuietWindow              = 50 * time.Millisecond
	altScreenEnterSeq           = "\x1b[?1049h"
	altScreenExitSeq            = "\x1b[?1049l"
	bubbleTeaFullRedrawSeq      = "\x1b[H\x1b[2J"
	assistantStreamText         = "stream visible from pty smoke backend"
	ctrlCExitStatusText         = "Ctrl+C again to exit"
	contextCanceledText         = "context canceled"
	footerProfileModel          = "pty-profile/pty-model"
	footerWorkspaceName         = "pty-workspace"
	footerSessionMarker         = "resize-session-marker-0123456789"
	selectedResumeSessionID     = "pty-selected-session"
	selectedAssistantTranscript = "assistant-only selected resume transcript marker"
	wideFooterMarker            = footerWorkspaceName + " | " + footerProfileModel + " | tokens 0/0 | " + footerSessionMarker
	narrowFooterMarker          = footerWorkspaceName + " | " + footerProfileModel + " | tokens 0/0"
	ptyCompactFocus             = "focus on PTY compaction"
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
	currentPath := createPTYSession(t, root, workspace, "pty-current-session", "current transcript marker", "")
	selectedPath := createPTYSession(t, root, workspace, selectedResumeSessionID, "selected picker label", selectedAssistantTranscript)
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
	waitForSubsequence(t, collector, 0, "selected picker label")

	selectedOffset := collector.Len()
	writePTY(t, master, "\x1b[B\r")
	// The selected ID is not rendered in the picker, so this synchronizes on
	// the replacement commit. Display evidence comes from the terminal-screen
	// assertion after resize below.
	waitForSubsequence(t, collector, selectedOffset, selectedResumeSessionID)

	resizeOffset := collector.Len()
	if err := pty.Setsize(slave, &pty.Winsize{Cols: 140, Rows: 34}); err != nil {
		t.Fatalf("pty.Ptysize(140x34) error = %v", err)
	}
	if err := syscall.Kill(os.Getpid(), syscall.SIGWINCH); err != nil {
		t.Fatalf("SIGWINCH error = %v", err)
	}
	resumeScreen, resizeRaw := waitForTerminalScreen(t, collector, resizeOffset, 140, 34, ptyScreenHasResumeEvidence)
	redrawOffset := bytes.Index(resizeRaw, []byte(bubbleTeaFullRedrawSeq))
	if redrawOffset < 0 {
		t.Fatalf("post-resize raw output = %s, want Bubble Tea full-redraw delimiter %q", tailTerminalOutput(resizeRaw), bubbleTeaFullRedrawSeq)
	}
	if resumeScreen.width != 140 || resumeScreen.height != 34 {
		t.Fatalf("post-resize terminal screen = %dx%d, want 140x34", resumeScreen.width, resumeScreen.height)
	}
	if x, y, visible := resumeScreen.Cursor(); !visible || x != 4 || y != 30 {
		t.Fatalf("post-resume terminal cursor = (%d,%d) visible=%v, want (4,30) visible", x, y, visible)
	}
	t.Logf("PTY redraw evidence: raw delimiter=%q at offset=%d full-redraws=%d final-screen=%dx%d contains transcript+session ID and no Resume modal; accepted sequences=%q", bubbleTeaFullRedrawSeq, redrawOffset, resumeScreen.FullRedraws(), resumeScreen.width, resumeScreen.height, resumeScreen.AcceptedCSI())

	writePTY(t, master, "/exit\r")
	waitForRunReturn(t, runResult)
	waitForSubsequence(t, collector, 0, altScreenExitSeq)

	if !runResult.Finished() {
		t.Fatal("run process was still active before terminal restoration check")
	}
	output := collector.Snapshot()
	enters, exits := bytes.Count(output, []byte(altScreenEnterSeq)), bytes.Count(output, []byte(altScreenExitSeq))
	if enters != 1 || exits != 1 {
		t.Fatalf("alternate-screen sequences enter=%d exit=%d, want 1/1; tail: %s", enters, exits, tailTerminalOutput(output))
	}
	restoredMode, err := unix.IoctlGetTermios(int(slave.Fd()), unix.TIOCGETA)
	if err != nil {
		t.Fatalf("read restored terminal mode: %v", err)
	}
	if *restoredMode != *initialMode {
		t.Fatalf("terminal mode leaked after process exit: %s", diffTermios(*initialMode, *restoredMode))
	}
	t.Logf("PTY escape evidence: alt-screen enter=%d exit=%d; full termios restored", enters, exits)
}

func diffTermios(initial, restored unix.Termios) string {
	var diffs []string
	if initial.Iflag != restored.Iflag {
		diffs = append(diffs, fmt.Sprintf("Iflag %#x -> %#x", initial.Iflag, restored.Iflag))
	}
	if initial.Oflag != restored.Oflag {
		diffs = append(diffs, fmt.Sprintf("Oflag %#x -> %#x", initial.Oflag, restored.Oflag))
	}
	if initial.Cflag != restored.Cflag {
		diffs = append(diffs, fmt.Sprintf("Cflag %#x -> %#x", initial.Cflag, restored.Cflag))
	}
	if initial.Lflag != restored.Lflag {
		diffs = append(diffs, fmt.Sprintf("Lflag %#x -> %#x", initial.Lflag, restored.Lflag))
	}
	for index := range initial.Cc {
		if initial.Cc[index] != restored.Cc[index] {
			diffs = append(diffs, fmt.Sprintf("Cc[%d] %#x -> %#x", index, initial.Cc[index], restored.Cc[index]))
		}
	}
	if initial.Ispeed != restored.Ispeed {
		diffs = append(diffs, fmt.Sprintf("Ispeed %d -> %d", initial.Ispeed, restored.Ispeed))
	}
	if initial.Ospeed != restored.Ospeed {
		diffs = append(diffs, fmt.Sprintf("Ospeed %d -> %d", initial.Ospeed, restored.Ospeed))
	}
	if len(diffs) == 0 {
		return "no field differences"
	}
	return strings.Join(diffs, ", ")
}

func createPTYSession(t *testing.T, root, workspace, id, userTranscript, assistantTranscript string) string {
	t.Helper()
	store, err := session.Create(root, session.Header{
		Version: session.CurrentVersion, ID: id, Workspace: workspace, Provider: "openai-compatible",
		Profile: "test", Model: "test-model", CreatedAt: time.Now().UTC(),
	})
	if err != nil {
		t.Fatal(err)
	}
	for _, message := range []model.Message{
		{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: userTranscript}}, CreatedAt: time.Now().UTC()},
		{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: assistantTranscript}}, CreatedAt: time.Now().UTC(), FinishReason: model.FinishStop},
	} {
		if message.Text() == "" {
			continue
		}
		if err := store.Append(context.Background(), message); err != nil {
			_ = store.Close()
			t.Fatal(err)
		}
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestTUICompactCommandCompletionCancelAndTerminalRestore(t *testing.T) {
	master, slave, err := pty.Open()
	if err != nil {
		t.Fatalf("pty.Open() error = %v", err)
	}
	initialMode, err := unix.IoctlGetTermios(int(slave.Fd()), unix.TIOCGETA)
	if err != nil {
		t.Fatalf("read initial terminal mode: %v", err)
	}
	if err := pty.Setsize(slave, &pty.Winsize{Cols: 100, Rows: 30}); err != nil {
		t.Fatalf("pty.Setsize(100x30) error = %v", err)
	}

	collector := newPTYOutputCollector(master)
	runCtx, cancelRun := context.WithCancel(context.Background())
	backend := &ptySmokeBackend{
		promptCh:          make(chan string, 1),
		canceledCh:        make(chan struct{}),
		compactCh:         make(chan string, 2),
		compactCanceledCh: make(chan struct{}),
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

	enterOffset := waitForSubsequence(t, collector, 0, altScreenEnterSeq)
	screenOffset := enterOffset + len(altScreenEnterSeq)
	waitForSubsequence(t, collector, screenOffset, wideFooterMarker)

	writePTY(t, master, "/c")
	waitForSubsequence(t, collector, screenOffset, "compact context")

	writePTY(t, master, "\t")
	writePTY(t, master, " "+ptyCompactFocus)

	completionResizeOffset := collector.Len()
	if err := pty.Setsize(slave, &pty.Winsize{Cols: 96, Rows: 30}); err != nil {
		t.Fatalf("pty.Setsize(96x30) error = %v", err)
	}
	if err := syscall.Kill(os.Getpid(), syscall.SIGWINCH); err != nil {
		t.Fatalf("SIGWINCH error = %v", err)
	}
	completedScreen, _ := waitForTerminalScreen(t, collector, completionResizeOffset, 96, 30, func(screen *ptyTerminalScreen) bool {
		content := screen.String()
		return screen.FullRedraws() > 0 && screen.Complete() &&
			strings.Contains(content, "/compact "+ptyCompactFocus) &&
			!strings.Contains(content, "compact context")
	})
	expectedCursorX := len([]rune("> ")) + len([]rune("/compact "+ptyCompactFocus)) + 2
	if x, y, visible := completedScreen.Cursor(); !visible || x != expectedCursorX || y != 26 {
		t.Fatalf("completed command cursor = (%d,%d) visible=%v, want (%d,26) visible", x, y, visible, expectedCursorX)
	}

	writePTY(t, master, "\r")
	waitForCompactFocus(t, backend, ptyCompactFocus)
	waitForSubsequence(t, collector, screenOffset, "compacting context")

	writePTY(t, master, "\x1b")
	waitForCompactCancellation(t, backend)
	waitForSubsequence(t, collector, screenOffset, contextCanceledText)

	writePTY(t, master, "/compact "+ptyCompactFocus+"\r")
	waitForCompactFocus(t, backend, ptyCompactFocus)
	waitForSubsequence(t, collector, screenOffset, "[context] no-op")

	resizeOffset := collector.Len()
	if err := pty.Setsize(slave, &pty.Winsize{Cols: 80, Rows: 24}); err != nil {
		t.Fatalf("pty.Setsize(80x24) error = %v", err)
	}
	if err := syscall.Kill(os.Getpid(), syscall.SIGWINCH); err != nil {
		t.Fatalf("SIGWINCH error = %v", err)
	}
	compactScreen, _ := waitForTerminalScreen(t, collector, resizeOffset, 80, 24, func(screen *ptyTerminalScreen) bool {
		content := screen.String()
		return screen.FullRedraws() > 0 && screen.Complete() &&
			strings.Contains(content, narrowFooterMarker) &&
			strings.Contains(content, "[context] no-op") &&
			strings.Contains(content, contextCanceledText) &&
			!strings.Contains(content, "compact context")
	})
	if x, y, visible := compactScreen.Cursor(); !visible || x != 4 || y != 20 {
		t.Fatalf("post-compaction terminal cursor = (%d,%d) visible=%v, want (4,20) visible", x, y, visible)
	}
	if sequences := compactScreen.AcceptedCSI(); len(sequences) == 0 {
		t.Fatal("compaction screen accepted no CSI sequences")
	}
	writePTY(t, master, "/exit\r")
	waitForRunReturn(t, runResult)
	waitForSubsequence(t, collector, 0, altScreenExitSeq)

	if !runResult.Finished() {
		t.Fatal("run process was still active before terminal restoration check")
	}
	output := collector.Snapshot()
	enters, exits := bytes.Count(output, []byte(altScreenEnterSeq)), bytes.Count(output, []byte(altScreenExitSeq))
	if enters != 1 || exits != 1 {
		t.Fatalf("alternate-screen sequences enter=%d exit=%d, want 1/1; tail: %s", enters, exits, tailTerminalOutput(output))
	}
	restoredMode, err := unix.IoctlGetTermios(int(slave.Fd()), unix.TIOCGETA)
	if err != nil {
		t.Fatalf("read restored terminal mode: %v", err)
	}
	if *restoredMode != *initialMode {
		t.Fatalf("terminal mode leaked after process exit: %s", diffTermios(*initialMode, *restoredMode))
	}
	if strings.Contains(string(output), "\x00") {
		t.Fatalf("raw PTY output leaked NUL control: %s", tailTerminalOutput(output))
	}

	t.Logf("PTY compaction accepted sequences=%q", compactScreen.AcceptedCSI())
	t.Logf("PTY compaction escape evidence: alt-screen enter=%d exit=%d; full termios restored", enters, exits)
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
	lifecycleScreen, _ := waitForTerminalScreen(t, collector, resizeOffset, 80, 24, func(screen *ptyTerminalScreen) bool {
		content := screen.String()
		return screen.FullRedraws() > 0 && screen.Complete() &&
			strings.Contains(content, narrowFooterMarker) &&
			!strings.Contains(content, footerSessionMarker)
	})
	if x, y, visible := lifecycleScreen.Cursor(); !visible || x != 4 || y != 20 {
		t.Fatalf("post-resize terminal cursor = (%d,%d) visible=%v, want (4,20) visible", x, y, visible)
	}
	t.Logf("PTY lifecycle accepted sequences=%q", lifecycleScreen.AcceptedCSI())

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
	promptCh          chan string
	canceledCh        chan struct{}
	compactCh         chan string
	compactCanceledCh chan struct{}
	cancelOnce        sync.Once
	compactCancelOnce sync.Once
	compactMu         sync.Mutex
	compactCalls      int
}

func (b *ptySmokeBackend) Prompt(ctx context.Context, text string, emit func(agent.Event)) error {
	b.promptCh <- text
	emit(agent.Event{Type: agent.EventTextDelta, Text: assistantStreamText})
	<-ctx.Done()
	b.cancelOnce.Do(func() { close(b.canceledCh) })
	return ctx.Err()
}

func (b *ptySmokeBackend) Compact(ctx context.Context, focus string, emit func(agent.Event)) (agent.CompactionResult, error) {
	if b.compactCh != nil {
		b.compactCh <- focus
	}
	if emit != nil {
		emit(agent.Event{Type: agent.EventCompactionStarted, Compaction: &agent.CompactionEvent{Reason: agent.CompactionManual}})
	}
	b.compactMu.Lock()
	b.compactCalls++
	call := b.compactCalls
	b.compactMu.Unlock()
	if call == 1 {
		<-ctx.Done()
		b.compactCancelOnce.Do(func() {
			if b.compactCanceledCh != nil {
				close(b.compactCanceledCh)
			}
		})
		return agent.CompactionResult{}, ctx.Err()
	}
	return agent.CompactionResult{Reason: agent.CompactionManual, Noop: true}, nil
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

func waitForCompactFocus(t *testing.T, backend *ptySmokeBackend, want string) {
	t.Helper()
	select {
	case got := <-backend.compactCh:
		if got != want {
			t.Fatalf("compact focus = %q, want %q", got, want)
		}
	case <-time.After(ptyStepTimeout):
		t.Fatal("timed out waiting for compact focus")
	}
}

func waitForCompactCancellation(t *testing.T, backend *ptySmokeBackend) {
	t.Helper()
	select {
	case <-backend.compactCanceledCh:
	case <-time.After(ptyStepTimeout):
		t.Fatal("timed out waiting for compact cancellation")
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

func waitForTerminalScreen(t *testing.T, collector *ptyOutputCollector, after, width, height int, accept func(*ptyTerminalScreen) bool) (*ptyTerminalScreen, []byte) {
	t.Helper()
	screen, raw, err := collector.WaitForTerminalScreen(after, width, height, accept, ptyQuietWindow, ptyStepTimeout)
	if err != nil {
		t.Fatal(err)
	}
	return screen, raw
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

func (c *ptyOutputCollector) WaitForTerminalScreen(after, width, height int, accept func(*ptyTerminalScreen) bool, quietWindow, timeout time.Duration) (*ptyTerminalScreen, []byte, error) {
	deadline := time.Now().Add(timeout)
	stableSince := time.Time{}
	lastLen := -1
	for {
		c.mu.Lock()
		start := min(max(after, 0), len(c.buf))
		raw := append([]byte(nil), c.buf[start:]...)
		readErr := c.err
		closed := false
		select {
		case <-c.done:
			closed = true
		default:
		}
		c.mu.Unlock()

		screen := newPTYTerminalScreen(width, height)
		if _, err := screen.Write(raw); err != nil {
			return nil, raw, fmt.Errorf("interpret terminal output: %w\nraw tail: %s", err, tailTerminalOutput(raw))
		}
		matched := accept(screen)
		if len(raw) != lastLen {
			lastLen = len(raw)
			stableSince = time.Time{}
		} else if matched {
			if stableSince.IsZero() {
				stableSince = time.Now()
			} else if time.Since(stableSince) >= quietWindow {
				return screen, raw, nil
			}
		} else {
			stableSince = time.Time{}
		}

		if closed {
			if matched {
				return screen, raw, nil
			}
			return nil, raw, fmt.Errorf("output closed before terminal screen matched: %v\nfinal screen: %q\nraw tail: %s", readErr, screen.String(), tailTerminalOutput(raw))
		}
		if time.Now().After(deadline) {
			return nil, raw, fmt.Errorf("timed out waiting for matching %dx%d terminal screen\nlast screen: %q\nraw tail: %s", width, height, screen.String(), tailTerminalOutput(raw))
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

func TestPTYTerminalScreenTracksObservedCursorVisibility(t *testing.T) {
	screen := newPTYTerminalScreen(80, 24)
	if _, err := screen.Write([]byte("\x1b[?25h")); err != nil {
		t.Fatalf("show cursor: %v", err)
	}
	if _, _, visible := screen.Cursor(); !visible {
		t.Fatal("cursor visible = false after show sequence")
	}
	if _, err := screen.Write([]byte("\x1b[?25l")); err != nil {
		t.Fatalf("hide cursor: %v", err)
	}
	if _, _, visible := screen.Cursor(); visible {
		t.Fatal("cursor visible = true after hide sequence")
	}
}

func TestPTYTerminalScreenRejectsUnsupportedControlsAndCSI(t *testing.T) {
	tests := []struct {
		name     string
		sequence string
	}{
		{name: "NUL", sequence: "\x00"},
		{name: "unsupported C0", sequence: "\x01"},
		{name: "DEL", sequence: "\x7f"},
		{name: "Unicode control", sequence: "\u0085"},
		{name: "insert mode", sequence: "\x1b[4h"},
		{name: "alternate screen enter", sequence: "\x1b[?1049h"},
		{name: "alternate screen exit", sequence: "\x1b[?1049l"},
		{name: "unknown private mode", sequence: "\x1b[?9999h"},
		{name: "unobserved synchronized-output mode", sequence: "\x1b[?2026l"},
		{name: "unknown public mode", sequence: "\x1b[20l"},
		{name: "unobserved SGR", sequence: "\x1b[8m"},
		{name: "malformed params", sequence: "\x1b[1,2m"},
		{name: "overflow param", sequence: "\x1b[999999999999999999999999L"},
		{name: "negative param", sequence: "\x1b[-1L"},
		{name: "unexpected private prefix", sequence: "\x1b[>1m"},
		{name: "extra params", sequence: "\x1b[1;2;3H"},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			screen := newPTYTerminalScreen(140, 34)
			validEvidence := bubbleTeaFullRedrawSeq + selectedAssistantTranscript + "\r\n" + selectedResumeSessionID
			if _, err := screen.Write([]byte(validEvidence)); err != nil {
				t.Fatalf("write valid evidence: %v", err)
			}
			if !ptyScreenHasResumeEvidence(screen) {
				t.Fatal("valid setup did not satisfy evidence predicate")
			}

			if _, err := screen.Write([]byte(test.sequence)); err == nil {
				t.Fatalf("screen.Write(%q) error = nil, want rejection", test.sequence)
			}
			if ptyScreenHasResumeEvidence(screen) {
				t.Fatalf("evidence predicate accepted screen after rejected sequence %q", test.sequence)
			}
		})
	}
}

func TestPTYTerminalScreenResumeEvidenceDoesNotAggregateAcrossFrames(t *testing.T) {
	tests := []struct {
		name        string
		chunks      []string
		want        bool
		wantRedraws int
	}{
		{
			name: "split reads and separate full redraws",
			chunks: []string{
				"\x1b[", "H\x1b[2", "J" + selectedAssistantTranscript,
				"\x1b[H\x1b", "[2J" + selectedResumeSessionID,
			},
			want:        false,
			wantRedraws: 2,
		},
		{
			name: "split reads in one full redraw",
			chunks: []string{
				"\x1b", "[H\x1b[", "2J" + selectedAssistantTranscript + "\r\npty-",
				"selected-session",
			},
			want:        true,
			wantRedraws: 1,
		},
		{
			name: "split incremental insert-line update preserves one screen",
			chunks: []string{
				bubbleTeaFullRedrawSeq + selectedAssistantTranscript + "\r\n" + selectedResumeSessionID,
				"\r\x1b[2", "d\x1b[1", "L",
			},
			want:        true,
			wantRedraws: 1,
		},
		{
			name:        "one full redraw still showing modal",
			chunks:      []string{bubbleTeaFullRedrawSeq + selectedAssistantTranscript + " " + selectedResumeSessionID + " Resume Session"},
			want:        false,
			wantRedraws: 1,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			screen := newPTYTerminalScreen(140, 34)
			for _, chunk := range test.chunks {
				if _, err := screen.Write([]byte(chunk)); err != nil {
					t.Fatalf("screen.Write(%q) error = %v", chunk, err)
				}
			}
			if got := screen.FullRedraws(); got != test.wantRedraws {
				t.Fatalf("FullRedraws() = %d, want %d", got, test.wantRedraws)
			}
			if got := ptyScreenHasResumeEvidence(screen); got != test.want {
				t.Fatalf("ptyScreenHasResumeEvidence() = %t, want %t; redraws=%d screen=%q", got, test.want, screen.FullRedraws(), screen.String())
			}
		})
	}
}
