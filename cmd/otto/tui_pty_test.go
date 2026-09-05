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
	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/sandbox/sandboxtest"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tui"
	"github.com/creack/pty"
	"golang.org/x/sys/unix"
)

const (
	ptyTestTimeout = 5 * time.Second
	ptyStepTimeout = 2 * time.Second
	ptyQuietWindow = 50 * time.Millisecond
	// cursorShowSeq marks the end of a completed render in inline mode; it
	// appears after every frame, so it is used only to confirm the TUI has
	// drawn at least once before sending input.
	cursorShowSeq = "\x1b[?25h"
	// exitRestoreSeq is the OSC 112 cursor-color reset Bubble Tea writes once,
	// at final terminal-state teardown after tui.Run returns. It is the
	// inline-mode replacement for waiting on an alternate-screen exit.
	exitRestoreSeq = "\x1b]112\a"
	// fullRedrawSeq is the erase-display (CSI J, mode 0) Bubble Tea's inline
	// renderer issues immediately before redrawing the live region from
	// scratch; its presence in captured output is evidence of a full redraw.
	fullRedrawSeq               = "\x1b[J"
	assistantStreamText         = "stream visible from pty smoke backend"
	ctrlCExitStatusMarker       = "press Ctrl+C"
	contextCanceledText         = "context canceled"
	footerProfileModel          = "pty-profile/pty-model"
	footerWorkspaceName         = "pty-workspace"
	footerSessionMarker         = "resize-session-marker-0123456789"
	selectedResumeSessionID     = "pty-selected-session"
	selectedAssistantTranscript = "assistant-only selected resume transcript marker"
	wideFooterMarker            = footerWorkspaceName + " | " + footerProfileModel + " | unsafe | tokens 0/0 | " + footerSessionMarker
	narrowFooterMarker          = footerWorkspaceName + " | " + footerProfileModel + " | unsafe | tokens 0/0"
	ptyCompactFocus             = "focus on PTY compaction"
)

func TestTUIPseudoTerminalSandboxChecklist(t *testing.T) {
	sandboxtest.RunChecklist(t, []sandboxtest.ChecklistItem{
		{
			Name: "footer and help render sandbox off warning",
			Run: func(t *testing.T) {
				master, slave, err := pty.Open()
				if err != nil {
					t.Fatalf("pty.Open() error = %v", err)
				}
				if err := pty.Setsize(slave, &pty.Winsize{Cols: 120, Rows: 30}); err != nil {
					t.Fatalf("pty.Setsize(120x30) error = %v", err)
				}
				collector := newPTYOutputCollector(master)
				runCtx, cancelRun := context.WithCancel(context.Background())
				backend := &ptySmokeBackend{promptCh: make(chan string, 1), canceledCh: make(chan struct{}), compactCh: make(chan string, 1), compactCanceledCh: make(chan struct{})}
				runResult := startRunResult(func() error { return tui.Run(runCtx, slave, slave, backend) })
				defer func() {
					cancelRun()
					_ = slave.Close()
					_ = master.Close()
					collector.Wait(t, ptyStepTimeout)
				}()

				waitForSubsequence(t, collector, 0, wideFooterMarker)
				writePTY(t, master, "?")
				waitForSubsequence(t, collector, 0, "Sandbox: sandbox off · WARNING: bash is unsandboxed")
				closeOffset := collector.Len()
				writePTY(t, master, "\x1b")
				waitForSubsequence(t, collector, closeOffset, wideFooterMarker)
				writePTY(t, master, "/exit\r")
				waitForRunReturn(t, runResult)
				waitForSubsequence(t, collector, 0, exitRestoreSeq)
			},
		},
		{
			Name: "session overlay keeps sandbox status visible",
			Run: func(t *testing.T) {
				master, slave, err := pty.Open()
				if err != nil {
					t.Fatalf("pty.Open() error = %v", err)
				}
				if err := pty.Setsize(slave, &pty.Winsize{Cols: 120, Rows: 30}); err != nil {
					t.Fatalf("pty.Setsize(120x30) error = %v", err)
				}
				collector := newPTYOutputCollector(master)
				runCtx, cancelRun := context.WithCancel(context.Background())
				backend := &ptySmokeBackend{promptCh: make(chan string, 1), canceledCh: make(chan struct{}), compactCh: make(chan string, 1), compactCanceledCh: make(chan struct{})}
				runResult := startRunResult(func() error { return tui.Run(runCtx, slave, slave, backend) })
				defer func() {
					cancelRun()
					_ = slave.Close()
					_ = master.Close()
					collector.Wait(t, ptyStepTimeout)
				}()

				waitForSubsequence(t, collector, 0, wideFooterMarker)
				writePTY(t, master, "/session\r")
				waitForSubsequence(t, collector, 0, "Sandbox: sandbox off · WARNING: bash is unsandboxed")
				closeOffset := collector.Len()
				writePTY(t, master, "\x1b")
				waitForSubsequence(t, collector, closeOffset, wideFooterMarker)
				writePTY(t, master, "/exit\r")
				waitForRunReturn(t, runResult)
				waitForSubsequence(t, collector, 0, exitRestoreSeq)
			},
		},
	})
}

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
		code := runForTest(t, runCtx, []string{"--config", configPath, "--cwd", workspace, "--resume", currentPath, "--ui", "tui"}, slave, slave, &stderr, testEnviron(map[string]string{
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
	// The finished transcript (selectedAssistantTranscript) was already
	// flushed to native scrollback via tea.Println before this resize, so it
	// is outside the emulator's tracked screen window in inline mode; only
	// the footer's session ID and the absence of the Resume picker remain
	// checkable here.
	resumeScreen, resizeRaw := waitForTerminalScreen(t, collector, resizeOffset, 140, 34, func(screen *ptyTerminalScreen) bool {
		if screen.FullRedraws() == 0 || !screen.Complete() {
			return false
		}
		content := screen.String()
		if !strings.Contains(content, selectedResumeSessionID) || strings.Contains(content, "Resume Session") {
			return false
		}
		x, y, visible := screen.Cursor()
		return visible && x == 4 && y == 3
	})
	redrawOffset := bytes.Index(resizeRaw, []byte(fullRedrawSeq))
	if redrawOffset < 0 {
		t.Fatalf("post-resize raw output = %s, want Bubble Tea full-redraw delimiter %q", tailTerminalOutput(resizeRaw), fullRedrawSeq)
	}
	if resumeScreen.width != 140 || resumeScreen.height != 34 {
		t.Fatalf("post-resize terminal screen = %dx%d, want 140x34", resumeScreen.width, resumeScreen.height)
	}
	if x, y, visible := resumeScreen.Cursor(); !visible || x != 4 || y != 3 {
		t.Fatalf("post-resume terminal cursor = (%d,%d) visible=%v, want (4,3) visible", x, y, visible)
	}
	t.Logf("PTY redraw evidence: raw delimiter=%q at offset=%d full-redraws=%d final-screen=%dx%d contains session ID and no Resume modal; accepted sequences=%q", fullRedrawSeq, redrawOffset, resumeScreen.FullRedraws(), resumeScreen.width, resumeScreen.height, resumeScreen.AcceptedCSI())

	writePTY(t, master, "/exit\r")
	waitForRunReturn(t, runResult)
	waitForSubsequence(t, collector, 0, exitRestoreSeq)

	if !runResult.Finished() {
		t.Fatal("run process was still active before terminal restoration check")
	}
	restoredMode, err := unix.IoctlGetTermios(int(slave.Fd()), unix.TIOCGETA)
	if err != nil {
		t.Fatalf("read restored terminal mode: %v", err)
	}
	if *restoredMode != *initialMode {
		t.Fatalf("terminal mode leaked after process exit: %s", diffTermios(*initialMode, *restoredMode))
	}
}

func TestTUIPseudoTerminalArchiveLifecycle(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		writeSSE(w, `{"choices":[{"delta":{"content":"unused"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	home := t.TempDir()
	workspace := filepath.Join(t.TempDir(), "pty-archive-workspace")
	if err := os.Mkdir(workspace, 0o755); err != nil {
		t.Fatal(err)
	}
	root := filepath.Join(home, ".otto", "sessions")
	archiveTargetPath := createPTYSession(t, root, workspace, "pty-archive-target", "archive target label", "")
	currentPath := createPTYSession(t, root, workspace, "pty-archive-current", "current archive transcript", "current archive assistant")
	now := time.Now()
	setCLISessionMTime(t, archiveTargetPath, now)
	setCLISessionMTime(t, currentPath, now.Add(-time.Hour))
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

	waitForSubsequence(t, collector, 0, "current archive transcript")
	writePTY(t, master, "/archive\r")
	waitForSubsequence(t, collector, 0, "Archive Session")
	waitForSubsequence(t, collector, 0, "archive target label")
	// The archive target is the newest session and therefore the first row;
	// Enter archives it without touching the current session.
	writePTY(t, master, "\r")
	waitForSubsequence(t, collector, 0, "archived session pty-archive-target")

	archivedPath := filepath.Join(filepath.Dir(archiveTargetPath), "archive", "pty-archive-target.jsonl")
	if _, err := os.Stat(archivedPath); err != nil {
		t.Fatalf("archived session missing on disk: %v", err)
	}
	if _, err := os.Stat(archiveTargetPath); !os.IsNotExist(err) {
		t.Fatalf("archived source still exists on disk")
	}

	writePTY(t, master, "/exit\r")
	waitForRunReturn(t, runResult)
	waitForSubsequence(t, collector, 0, exitRestoreSeq)

	if !runResult.Finished() {
		t.Fatal("run process was still active before terminal restoration check")
	}
	restoredMode, err := unix.IoctlGetTermios(int(slave.Fd()), unix.TIOCGETA)
	if err != nil {
		t.Fatalf("read restored terminal mode: %v", err)
	}
	if *restoredMode != *initialMode {
		t.Fatalf("terminal mode leaked after process exit: %s", diffTermios(*initialMode, *restoredMode))
	}
	t.Logf("PTY archive evidence: picker opened, target archived to %s, termios restored", archivedPath)
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

	screenOffset := waitForSubsequence(t, collector, 0, wideFooterMarker)

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
	expectedCursorX := len([]rune("> ")) + len([]rune("/compact "+ptyCompactFocus)) + 2
	completedScreen, _ := waitForTerminalScreen(t, collector, completionResizeOffset, 96, 30, func(screen *ptyTerminalScreen) bool {
		content := screen.String()
		if screen.FullRedraws() == 0 || !screen.Complete() ||
			!strings.Contains(content, "/compact "+ptyCompactFocus) ||
			strings.Contains(content, "compact context") {
			return false
		}
		x, y, visible := screen.Cursor()
		return visible && x == expectedCursorX && y == 3
	})
	if x, y, visible := completedScreen.Cursor(); !visible || x != expectedCursorX || y != 3 {
		t.Fatalf("completed command cursor = (%d,%d) visible=%v, want (%d,3) visible", x, y, visible, expectedCursorX)
	}

	writePTY(t, master, "\r")
	waitForCompactFocus(t, backend, ptyCompactFocus)
	// Incremental ANSI updates need not contain the label as contiguous bytes.
	waitForTerminalScreen(t, collector, completionResizeOffset, 96, 30, func(screen *ptyTerminalScreen) bool {
		return screen.Complete() && strings.Contains(screen.String(), "compacting context")
	})

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
	// contextCanceledText was already confirmed above (waitForSubsequence at
	// screenOffset); by this point later turns have pushed it into native
	// scrollback, outside the emulator's tracked screen window, so it is not
	// re-checked here.
	compactScreen, _ := waitForTerminalScreen(t, collector, resizeOffset, 80, 24, func(screen *ptyTerminalScreen) bool {
		content := screen.String()
		if screen.FullRedraws() == 0 || !screen.Complete() ||
			!strings.Contains(content, narrowFooterMarker) ||
			!strings.Contains(content, "[context] no-op") ||
			strings.Contains(content, "compact context") {
			return false
		}
		x, y, visible := screen.Cursor()
		return visible && x == 4 && y == 3
	})
	if x, y, visible := compactScreen.Cursor(); !visible || x != 4 || y != 3 {
		t.Fatalf("post-compaction terminal cursor = (%d,%d) visible=%v, want (4,3) visible", x, y, visible)
	}
	if sequences := compactScreen.AcceptedCSI(); len(sequences) == 0 {
		t.Fatal("compaction screen accepted no CSI sequences")
	}
	writePTY(t, master, "/exit\r")
	waitForRunReturn(t, runResult)
	waitForSubsequence(t, collector, 0, exitRestoreSeq)

	if !runResult.Finished() {
		t.Fatal("run process was still active before terminal restoration check")
	}
	output := collector.Snapshot()
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
}

func TestTUIPseudoTerminalCancelsSandboxedBash(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		writeSSE(w, `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-bash","type":"function","function":{"name":"bash","arguments":"{\"command\":\"touch host-child-must-not-start\"}"}}]},"finish_reason":"tool_calls"}]}`)
	}))
	defer server.Close()

	home := t.TempDir()
	workspace := filepath.Join(t.TempDir(), "sandboxed-bash-pty")
	if err := os.Mkdir(workspace, 0o755); err != nil {
		t.Fatal(err)
	}
	configPath := writeCLIConfig(t, "openai-compatible", "PTY_TEST_KEY", server.URL)

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

	started := make(chan struct{})
	canceled := make(chan struct{})
	executor := &recordingSandboxExecutor{execute: func(ctx context.Context, _ sandbox.Request, _ sandbox.Streams) (sandbox.ExitStatus, error) {
		close(started)
		<-ctx.Done()
		close(canceled)
		return sandbox.ExitStatus{Code: -1, Signaled: true, Signal: "killed"}, context.Canceled
	}}
	deps := deterministicRunDependencies(t)
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
		return sandboxRuntime{
			Executor: executor, Environment: []string{"PATH=/usr/bin:/bin"},
			Info:               app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true},
			RedactionsComplete: true,
			close:              newSandboxRuntimeCloser(nil),
		}
	}

	collector := newPTYOutputCollector(master)
	runCtx, cancelRun := context.WithCancel(context.Background())
	var stderr bytes.Buffer
	runResult := startRunResult(func() error {
		code := runWithDependencies(runCtx, []string{"--config", configPath, "--cwd", workspace, "--ui", "tui"}, slave, slave, &stderr, testEnviron(map[string]string{
			"HOME": home, "SHELL": "/bin/sh", "PTY_TEST_KEY": "provider-value",
		}), deps)
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
			t.Log("sandboxed Bash TUI cleanup timed out")
		} else if err != nil && !isExpectedCleanupRunError(err) {
			t.Logf("sandboxed Bash TUI cleanup error: %v", err)
		}
	})

	if _, err := collector.WaitForEvent(0, []byte(cursorShowSeq), ptyStepTimeout); err != nil {
		t.Fatal(err)
	}
	writePTY(t, master, "run sandboxed bash\r")
	awaitPTYEvent(t, started, "sandbox Executor start")
	writePTY(t, master, "\x03")
	awaitPTYEvent(t, canceled, "sandbox Executor cancellation")
	if _, err := collector.WaitForEvent(0, []byte(contextCanceledText), ptyStepTimeout); err != nil {
		t.Fatal(err)
	}
	writePTY(t, master, "/exit\r")
	waitForRunReturn(t, runResult)
	if _, err := collector.WaitForEvent(0, []byte(exitRestoreSeq), ptyStepTimeout); err != nil {
		t.Fatal(err)
	}

	if executor.calls.Load() != 1 {
		t.Fatalf("Executor calls = %d, want 1", executor.calls.Load())
	}
	if _, err := os.Stat(filepath.Join(workspace, "host-child-must-not-start")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("fake Seatbelt path started a host child: %v", err)
	}
	if stderr.Len() != 0 {
		t.Fatalf("successful Seatbelt run wrote stderr: %q", stderr.String())
	}
	restoredMode, err := unix.IoctlGetTermios(int(slave.Fd()), unix.TIOCGETA)
	if err != nil {
		t.Fatalf("read restored terminal mode: %v", err)
	}
	if *restoredMode != *initialMode {
		t.Fatalf("terminal mode leaked after sandboxed Bash cancellation: %s", diffTermios(*initialMode, *restoredMode))
	}
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
		if screen.FullRedraws() == 0 || !screen.Complete() ||
			!strings.Contains(content, narrowFooterMarker) ||
			strings.Contains(content, footerSessionMarker) {
			return false
		}
		x, y, visible := screen.Cursor()
		return visible && x == 4 && y == 7
	})
	if x, y, visible := lifecycleScreen.Cursor(); !visible || x != 4 || y != 7 {
		t.Fatalf("post-resize terminal cursor = (%d,%d) visible=%v, want (4,7) visible", x, y, visible)
	}
	t.Logf("PTY lifecycle accepted sequences=%q", lifecycleScreen.AcceptedCSI())

	writePTY(t, master, "\x1b")
	waitForCancellation(t, backend)
	waitForSubsequence(t, collector, streamOffset, contextCanceledText)

	ctrlCOffset := collector.Len()
	writePTY(t, master, "\x03")
	waitForSubsequence(t, collector, ctrlCOffset, ctrlCExitStatusMarker)

	writePTY(t, master, "\x03")
	waitForRunReturn(t, runResult)
	waitForSubsequence(t, collector, 0, exitRestoreSeq)
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
		Sandbox: app.SandboxInfo{
			Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true, Reason: app.SandboxReasonNone,
		},
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

func awaitPTYEvent(t *testing.T, event <-chan struct{}, name string) {
	t.Helper()
	select {
	case <-event:
	case <-time.After(ptyStepTimeout):
		t.Fatalf("timed out waiting for %s", name)
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
	mu      sync.Mutex
	buf     []byte
	done    chan struct{}
	updated chan struct{}
	err     error
}

func newPTYOutputCollector(master *os.File) *ptyOutputCollector {
	collector := &ptyOutputCollector{done: make(chan struct{}), updated: make(chan struct{}, 1)}
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
			c.signalUpdate()
		}
		if err != nil {
			c.mu.Lock()
			c.err = err
			c.mu.Unlock()
			c.signalUpdate()
			return
		}
	}
}

func (c *ptyOutputCollector) signalUpdate() {
	select {
	case c.updated <- struct{}{}:
	default:
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

func (c *ptyOutputCollector) WaitForEvent(after int, want []byte, timeout time.Duration) (int, error) {
	deadline := time.After(timeout)
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
		closed := false
		select {
		case <-c.done:
			closed = true
		default:
		}
		c.mu.Unlock()
		if closed {
			return -1, fmt.Errorf("output closed while waiting for %q: %v\nlast output: %s", want, readErr, tailTerminalOutput(snapshot))
		}
		select {
		case <-c.updated:
		case <-deadline:
			return -1, fmt.Errorf("timed out waiting for %q\nlast output: %s", want, tailTerminalOutput(snapshot))
		}
	}
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
			validEvidence := fullRedrawSeq + selectedAssistantTranscript + "\r\n" + selectedResumeSessionID
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
				fullRedrawSeq + selectedAssistantTranscript + "\r\n" + selectedResumeSessionID,
				"\r\x1b[2", "d\x1b[1", "L",
			},
			want:        true,
			wantRedraws: 1,
		},
		{
			name:        "one full redraw still showing modal",
			chunks:      []string{fullRedrawSeq + selectedAssistantTranscript + " " + selectedResumeSessionID + " Resume Session"},
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

func TestTUIPseudoTerminalHelpOverlayClosesWithoutResidue(t *testing.T) {
	master, slave, err := pty.Open()
	if err != nil {
		t.Fatalf("pty.Open() error = %v", err)
	}
	if err := pty.Setsize(slave, &pty.Winsize{Cols: 100, Rows: 30}); err != nil {
		t.Fatalf("pty.Setsize(100x30) error = %v", err)
	}
	collector := newPTYOutputCollector(master)
	runCtx, cancelRun := context.WithCancel(context.Background())
	backend := &ptySmokeBackend{promptCh: make(chan string, 1), canceledCh: make(chan struct{})}
	runResult := startRunResult(func() error { return tui.Run(runCtx, slave, slave, backend) })
	defer func() {
		cancelRun()
		_ = slave.Close()
		_ = master.Close()
		collector.Wait(t, ptyStepTimeout)
	}()

	waitForSubsequence(t, collector, 0, wideFooterMarker)
	helpOffset := collector.Len()
	writePTY(t, master, "/help\r")
	waitForSubsequence(t, collector, helpOffset, "Ctrl+O toggle details")
	closeOffset := collector.Len()
	writePTY(t, master, "\x1b")
	waitForSubsequence(t, collector, closeOffset, wideFooterMarker)
	waitForTerminalScreen(t, collector, helpOffset, 100, 30, func(screen *ptyTerminalScreen) bool {
		content := screen.String()
		return screen.Complete() && strings.Contains(content, wideFooterMarker) && !strings.Contains(content, "Ctrl+O toggle details")
	})
	writePTY(t, master, "/exit\r")
	waitForRunReturn(t, runResult)
}

func TestTUIPseudoTerminalSlashSuggestionsCloseWithoutResidue(t *testing.T) {
	master, slave, err := pty.Open()
	if err != nil {
		t.Fatalf("pty.Open: %v", err)
	}
	if err := pty.Setsize(slave, &pty.Winsize{Cols: 100, Rows: 30}); err != nil {
		t.Fatalf("pty.Setsize: %v", err)
	}
	collector := newPTYOutputCollector(master)
	runCtx, cancelRun := context.WithCancel(context.Background())
	backend := &ptySmokeBackend{promptCh: make(chan string, 1), canceledCh: make(chan struct{})}
	runResult := startRunResult(func() error { return tui.Run(runCtx, slave, slave, backend) })
	defer func() {
		cancelRun()
		_ = slave.Close()
		_ = master.Close()
		collector.Wait(t, ptyStepTimeout)
	}()

	waitForSubsequence(t, collector, 0, wideFooterMarker)
	openOffset := collector.Len()
	writePTY(t, master, "/")
	waitForSubsequence(t, collector, openOffset, "show help")
	closeOffset := collector.Len()
	writePTY(t, master, "\x1b")
	waitForSubsequence(t, collector, closeOffset, wideFooterMarker)
	waitForTerminalScreen(t, collector, openOffset, 100, 30, func(screen *ptyTerminalScreen) bool {
		content := screen.String()
		return screen.Complete() && strings.Contains(content, wideFooterMarker) && !strings.Contains(content, "show help")
	})

	writePTY(t, master, "/exit\r")
	waitForRunReturn(t, runResult)
}
