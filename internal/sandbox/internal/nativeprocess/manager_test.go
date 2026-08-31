//go:build darwin || linux

package nativeprocess

import (
	"bytes"
	"context"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/sandbox"
	"golang.org/x/sys/unix"
)

const blockingDescendantScript = `
/bin/sh -c 'echo "$$"; exec /bin/cat "$1"' child "$1" &
wait
`

const normalExitDescendantScript = `
/bin/sh -c 'echo "$$"; IFS= read -r acknowledgement < "$1"; echo ready > "$2"; exec /bin/cat "$3"' child "$1" "$2" "$3" &
IFS= read -r ready < "$2"
`

type runOutcome struct {
	result Result
	err    error
}

type signalingWriter struct {
	mu     sync.Mutex
	buffer bytes.Buffer
	marker string
	ready  chan struct{}
	once   sync.Once
}

func newSignalingWriter(marker string) *signalingWriter {
	return &signalingWriter{marker: marker, ready: make(chan struct{})}
}

func (w *signalingWriter) Write(data []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()

	written, err := w.buffer.Write(data)
	if strings.Contains(w.buffer.String(), w.marker) {
		w.once.Do(func() { close(w.ready) })
	}
	return written, err
}

func (w *signalingWriter) String() string {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.buffer.String()
}

type pidResult struct {
	pid int
	err error
}

type pidWriter struct {
	mu       sync.Mutex
	buffer   bytes.Buffer
	result   chan pidResult
	reported bool
}

func newPIDWriter() *pidWriter {
	return &pidWriter{result: make(chan pidResult, 1)}
}

func (w *pidWriter) Write(data []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()

	written, err := w.buffer.Write(data)
	if w.reported {
		return written, err
	}
	lineEnd := bytes.IndexByte(w.buffer.Bytes(), '\n')
	if lineEnd < 0 {
		return written, err
	}
	pid, parseErr := strconv.Atoi(strings.TrimSpace(string(w.buffer.Bytes()[:lineEnd])))
	w.reported = true
	w.result <- pidResult{pid: pid, err: parseErr}
	return written, err
}

func TestManagerReportsZeroAndNonzeroExit(t *testing.T) {
	manager := New()
	t.Cleanup(func() { _ = manager.Close() })

	for _, test := range []struct {
		name string
		code int
	}{
		{name: "zero", code: 0},
		{name: "nonzero", code: 23},
	} {
		t.Run(test.name, func(t *testing.T) {
			result, err := manager.Run(context.Background(), shellSpec(t, "exit "+strconv.Itoa(test.code), io.Discard, io.Discard))
			if err != nil {
				t.Fatalf("Run() error = %v", err)
			}
			if result.Code != test.code || result.Signaled || result.Signal != "" {
				t.Fatalf("Run() result = %+v, want ordinary exit %d", result, test.code)
			}
		})
	}
}

func TestManagerUsesNullStdinAndForwardsBothStreams(t *testing.T) {
	manager := New()
	t.Cleanup(func() { _ = manager.Close() })
	stdout := newSignalingWriter("stdout-ready")
	stderr := newSignalingWriter("stderr-ready")

	result, err := manager.Run(context.Background(), shellSpec(t, `
if IFS= read -r unexpected; then
	exit 97
fi
printf stdout-ready
printf stderr-ready >&2
`, stdout, stderr))
	if err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	if result.Code != 0 || result.Signaled {
		t.Fatalf("Run() result = %+v, want zero exit", result)
	}
	awaitSignal(t, stdout.ready, "stdout forwarding")
	awaitSignal(t, stderr.ready, "stderr forwarding")
	if got := stdout.String(); got != "stdout-ready" {
		t.Fatalf("stdout = %q", got)
	}
	if got := stderr.String(); got != "stderr-ready" {
		t.Fatalf("stderr = %q", got)
	}
}

func TestManagerCancellationTerminatesAndReapsProcessGroup(t *testing.T) {
	manager := New()
	t.Cleanup(func() { _ = manager.Close() })
	ctx, cancel := context.WithCancel(context.Background())
	outcome, pid := startBlockingDescendant(t, manager, ctx)

	cancel()
	completed := awaitRun(t, outcome, "cancelled Run")
	if !errors.Is(completed.err, context.Canceled) {
		t.Fatalf("Run() error = %v, want context.Canceled", completed.err)
	}
	assertKilledResult(t, completed.result)
	assertProcessGoneOnce(t, pid)
}

func TestManagerDeadlineCancellationTerminatesAndReapsProcessGroup(t *testing.T) {
	manager := New()
	t.Cleanup(func() { _ = manager.Close() })
	ctx, cancel := context.WithCancelCause(context.Background())
	outcome, pid := startBlockingDescendant(t, manager, ctx)

	cancel(context.DeadlineExceeded)
	completed := awaitRun(t, outcome, "deadline-cancelled Run")
	if !errors.Is(completed.err, context.DeadlineExceeded) {
		t.Fatalf("Run() error = %v, want context.DeadlineExceeded", completed.err)
	}
	assertKilledResult(t, completed.result)
	assertProcessGoneOnce(t, pid)
}

func TestManagerTerminationFailurePreservesCancellationIdentity(t *testing.T) {
	manager := New()
	realKill := manager.kill
	synthetic := errors.New("synthetic syscall detail")
	var injectOnce sync.Once
	manager.kill = func(pid int, signal syscall.Signal) error {
		err := realKill(pid, signal)
		injected := false
		injectOnce.Do(func() { injected = true })
		if injected {
			return synthetic
		}
		return err
	}
	t.Cleanup(func() { _ = manager.Close() })

	ctx, cancel := context.WithCancel(context.Background())
	outcome, pid := startBlockingDescendant(t, manager, ctx)
	cancel()
	completed := awaitRun(t, outcome, "Run after synthetic termination failure")
	if !errors.Is(completed.err, sandbox.ErrChildTerminate) || !errors.Is(completed.err, context.Canceled) {
		t.Fatalf("Run() error = %v, want ErrChildTerminate and context.Canceled", completed.err)
	}
	if strings.Contains(completed.err.Error(), synthetic.Error()) {
		t.Fatalf("Run() exposed raw termination detail: %v", completed.err)
	}
	assertKilledResult(t, completed.result)
	assertProcessGoneOnce(t, pid)
}

func TestManagerNormalShellExitRemovesBackgroundDescendants(t *testing.T) {
	manager := New()
	t.Cleanup(func() { _ = manager.Close() })
	workspace := t.TempDir()
	acknowledgement := makeFIFO(t, workspace, "acknowledgement")
	ready := makeFIFO(t, workspace, "ready")
	blocked := makeFIFO(t, workspace, "blocked")
	stdout := newPIDWriter()
	outcome := make(chan runOutcome, 1)

	go func() {
		result, err := manager.Run(context.Background(), Spec{
			Path:        "/bin/sh",
			Args:        []string{"-c", normalExitDescendantScript, "parent", acknowledgement, ready, blocked},
			Directory:   workspace,
			Environment: testEnvironment(),
			Stdout:      stdout,
			Stderr:      io.Discard,
		})
		outcome <- runOutcome{result: result, err: err}
	}()

	pid := awaitPID(t, stdout)
	writeFIFO(t, acknowledgement, "continue\n")
	completed := awaitRun(t, outcome, "normal shell exit")
	if completed.err != nil {
		t.Fatalf("Run() error = %v", completed.err)
	}
	if completed.result.Code != 0 || completed.result.Signaled {
		t.Fatalf("Run() result = %+v, want normal zero exit", completed.result)
	}
	assertProcessGoneOnce(t, pid)
}

func TestManagerCloseTerminatesActiveGroupsAndRejectsNewRuns(t *testing.T) {
	manager := New()
	outcome, pid := startBlockingDescendant(t, manager, context.Background())
	closeResult := make(chan error, 1)
	go func() { closeResult <- manager.Close() }()

	if err := awaitError(t, closeResult, "Manager.Close"); err != nil {
		t.Fatalf("Close() error = %v", err)
	}
	completed := awaitRun(t, outcome, "Run terminated by Close")
	if completed.err != nil {
		t.Fatalf("active Run() error = %v", completed.err)
	}
	assertKilledResult(t, completed.result)
	assertProcessGoneOnce(t, pid)

	if err := manager.Close(); err != nil {
		t.Fatalf("second Close() error = %v", err)
	}
	if _, err := manager.Run(context.Background(), shellSpec(t, "exit 0", io.Discard, io.Discard)); !errors.Is(err, sandbox.ErrClosed) {
		t.Fatalf("Run() after Close error = %v, want ErrClosed", err)
	}
}

func TestManagerConcurrentRunAndClose(t *testing.T) {
	const runCount = 24
	manager := New()
	start := make(chan struct{})
	outcomes := make(chan runOutcome, runCount)
	for range runCount {
		go func() {
			<-start
			result, err := manager.Run(context.Background(), shellSpec(t, "exit 0", io.Discard, io.Discard))
			outcomes <- runOutcome{result: result, err: err}
		}()
	}
	closeResult := make(chan error, 1)
	go func() {
		<-start
		closeResult <- manager.Close()
	}()
	close(start)

	for range runCount {
		completed := awaitRun(t, outcomes, "concurrent Run")
		if errors.Is(completed.err, sandbox.ErrClosed) {
			continue
		}
		if completed.err != nil {
			t.Fatalf("Run() error = %v", completed.err)
		}
		if completed.result.Code != 0 && !(completed.result.Code == -1 && completed.result.Signaled) {
			t.Fatalf("Run() result = %+v", completed.result)
		}
	}
	if err := awaitError(t, closeResult, "concurrent Close"); err != nil {
		t.Fatalf("Close() error = %v", err)
	}
	if _, err := manager.Run(context.Background(), shellSpec(t, "exit 0", io.Discard, io.Discard)); !errors.Is(err, sandbox.ErrClosed) {
		t.Fatalf("Run() after concurrent Close error = %v, want ErrClosed", err)
	}
}

func shellSpec(t *testing.T, script string, stdout, stderr io.Writer) Spec {
	t.Helper()
	return Spec{
		Path:        "/bin/sh",
		Args:        []string{"-c", script},
		Directory:   t.TempDir(),
		Environment: testEnvironment(),
		Stdout:      stdout,
		Stderr:      stderr,
	}
}

func testEnvironment() []string {
	return []string{"PATH=/usr/bin:/bin", "LC_ALL=C"}
}

func startBlockingDescendant(t *testing.T, manager *Manager, ctx context.Context) (<-chan runOutcome, int) {
	t.Helper()
	workspace := t.TempDir()
	blocked := makeFIFO(t, workspace, "blocked")
	stdout := newPIDWriter()
	outcome := make(chan runOutcome, 1)
	go func() {
		result, err := manager.Run(ctx, Spec{
			Path:        "/bin/sh",
			Args:        []string{"-c", blockingDescendantScript, "parent", blocked},
			Directory:   workspace,
			Environment: testEnvironment(),
			Stdout:      stdout,
			Stderr:      io.Discard,
		})
		outcome <- runOutcome{result: result, err: err}
	}()
	return outcome, awaitPID(t, stdout)
}

func makeFIFO(t *testing.T, directory, name string) string {
	t.Helper()
	path := filepath.Join(directory, name)
	if err := unix.Mkfifo(path, 0o600); err != nil {
		t.Fatalf("Mkfifo() error = %v", err)
	}
	return path
}

func writeFIFO(t *testing.T, path, value string) {
	t.Helper()
	result := make(chan error, 1)
	go func() {
		file, err := os.OpenFile(path, os.O_WRONLY, 0)
		if err == nil {
			_, err = io.WriteString(file, value)
			if closeErr := file.Close(); err == nil {
				err = closeErr
			}
		}
		result <- err
	}()
	if err := awaitError(t, result, "FIFO acknowledgement"); err != nil {
		t.Fatalf("write FIFO error = %v", err)
	}
}

func awaitPID(t *testing.T, writer *pidWriter) int {
	t.Helper()
	select {
	case result := <-writer.result:
		if result.err != nil || result.pid <= 0 {
			t.Fatalf("child PID result = %+v", result)
		}
		return result.pid
	case <-time.After(10 * time.Second):
		t.Fatal("timed out waiting for child PID")
		return 0
	}
}

func awaitSignal(t *testing.T, signal <-chan struct{}, description string) {
	t.Helper()
	select {
	case <-signal:
	case <-time.After(10 * time.Second):
		t.Fatalf("timed out waiting for %s", description)
	}
}

func awaitRun(t *testing.T, outcome <-chan runOutcome, description string) runOutcome {
	t.Helper()
	select {
	case completed := <-outcome:
		return completed
	case <-time.After(10 * time.Second):
		t.Fatalf("timed out waiting for %s", description)
		return runOutcome{}
	}
}

func awaitError(t *testing.T, result <-chan error, description string) error {
	t.Helper()
	select {
	case err := <-result:
		return err
	case <-time.After(10 * time.Second):
		t.Fatalf("timed out waiting for %s", description)
		return nil
	}
}

func assertKilledResult(t *testing.T, result Result) {
	t.Helper()
	if result.Code != -1 || !result.Signaled || result.Signal == "" {
		t.Fatalf("Run() result = %+v, want signal termination", result)
	}
}

func assertProcessGoneOnce(t *testing.T, pid int) {
	t.Helper()
	if err := unix.Kill(pid, 0); !errors.Is(err, unix.ESRCH) {
		if err == nil {
			_ = unix.Kill(pid, unix.SIGKILL)
		}
		t.Fatalf("signal 0 for descendant %d returned %v, want ESRCH", pid, err)
	}
}
