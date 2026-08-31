//go:build darwin || linux

package nativeprocess

import (
	"context"
	"errors"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/baiyuqing/otto/internal/sandbox"
	"golang.org/x/sys/unix"
)

type Spec struct {
	Path        string
	Args        []string
	Directory   string
	Environment []string
	Stdout      io.Writer
	Stderr      io.Writer
}

type Result struct {
	Code     int
	Signaled bool
	Signal   string
}

type Manager struct {
	mu        sync.Mutex
	active    map[*process]struct{}
	closing   bool
	closeDone chan struct{}
	closeErr  error

	kill func(int, syscall.Signal) error
}

type process struct {
	command *exec.Cmd
	stdout  *ownedPipeReader
	stderr  *ownedPipeReader
	done    chan struct{}

	mu         sync.Mutex
	cleanupErr error
}

type commandFiles struct {
	stdin       *os.File
	stdoutRead  *os.File
	stdoutWrite *os.File
	stderrRead  *os.File
	stderrWrite *os.File
}

type ownedPipeReader struct {
	file    *os.File
	closing atomic.Bool
}

type deliberatePipeCloseError struct{}

// Signals are sent before this hard deadline. It only bounds inherited pipe
// descriptors retained by an escaped process-group descendant.
const forwarderDeadlockDeadline = 5 * time.Second

func (deliberatePipeCloseError) Error() string { return "native process pipe closed for cleanup" }

func New() *Manager {
	return &Manager{
		active:    make(map[*process]struct{}),
		closeDone: make(chan struct{}),
		kill:      syscall.Kill,
	}
}

func (m *Manager) Run(ctx context.Context, spec Spec) (Result, error) {
	if ctx == nil {
		return Result{}, sandbox.ErrInvalidRequest
	}
	if err := safeContextError(ctx); err != nil {
		return Result{}, err
	}

	command, files, err := prepareCommand(spec)
	if err != nil {
		return Result{}, sandbox.ErrChildLaunch
	}
	proc := &process{
		command: command,
		stdout:  &ownedPipeReader{file: files.stdoutRead},
		stderr:  &ownedPipeReader{file: files.stderrRead},
		done:    make(chan struct{}),
	}

	m.mu.Lock()
	if m.closing {
		m.mu.Unlock()
		files.closeAll()
		return Result{}, sandbox.ErrClosed
	}
	if err := safeContextError(ctx); err != nil {
		m.mu.Unlock()
		files.closeAll()
		return Result{}, err
	}
	if err := command.Start(); err != nil {
		m.mu.Unlock()
		files.closeAll()
		return Result{}, sandbox.ErrChildLaunch
	}
	m.active[proc] = struct{}{}
	m.mu.Unlock()

	cleanupErr := closeStartedChildFiles(files)
	stdoutDone := forward(spec.Stdout, proc.stdout)
	stderrDone := forward(spec.Stderr, proc.stderr)
	waitDone := make(chan error, 1)
	go func() {
		waitDone <- command.Wait()
	}()

	var waitErr error
	var cancellationErr error
	var stdoutErr error
	var stderrErr error
	for waitDone != nil {
		select {
		case waitErr = <-waitDone:
			waitDone = nil
		case copyErr := <-stdoutDone:
			stdoutDone = nil
			stdoutErr = copyErr
			if copyErr != nil {
				cleanupErr = joinBoundedErrors(cleanupErr, sandbox.ErrChildWait, m.terminate(proc))
				waitErr = <-waitDone
				waitDone = nil
			}
		case copyErr := <-stderrDone:
			stderrDone = nil
			stderrErr = copyErr
			if copyErr != nil {
				cleanupErr = joinBoundedErrors(cleanupErr, sandbox.ErrChildWait, m.terminate(proc))
				waitErr = <-waitDone
				waitDone = nil
			}
		case <-ctx.Done():
			cancellationErr = safeContextError(ctx)
			cleanupErr = joinBoundedErrors(cleanupErr, m.terminate(proc))
			waitErr = <-waitDone
			waitDone = nil
		}
	}

	cleanupErr = joinBoundedErrors(cleanupErr, m.terminate(proc))
	stdoutErr, stderrErr, closeErr := finishForwarders(proc, stdoutDone, stderrDone, stdoutErr, stderrErr)
	cleanupErr = joinBoundedErrors(cleanupErr, closeErr, copyInfrastructureError(stdoutErr), copyInfrastructureError(stderrErr))

	result, resultErr := inspectResult(command, waitErr)
	cleanupErr = joinBoundedErrors(cleanupErr, resultErr)
	m.complete(proc, cleanupErr)
	return result, joinBoundedErrors(cleanupErr, cancellationErr)
}

func (m *Manager) Close() error {
	m.mu.Lock()
	if m.closing {
		done := m.closeDone
		m.mu.Unlock()
		<-done

		m.mu.Lock()
		err := m.closeErr
		m.mu.Unlock()
		return err
	}
	m.closing = true
	active := make([]*process, 0, len(m.active))
	for proc := range m.active {
		active = append(active, proc)
	}
	m.mu.Unlock()

	var closeErr error
	for _, proc := range active {
		closeErr = joinBoundedErrors(closeErr, m.terminate(proc))
	}
	for _, proc := range active {
		<-proc.done
		closeErr = joinBoundedErrors(closeErr, proc.error())
	}

	m.mu.Lock()
	m.closeErr = closeErr
	close(m.closeDone)
	m.mu.Unlock()
	return closeErr
}

func prepareCommand(spec Spec) (*exec.Cmd, commandFiles, error) {
	var files commandFiles
	commandPath, err := resolveExecutable(spec)
	if err != nil {
		return nil, files, err
	}
	stdin, err := os.Open(os.DevNull)
	if err != nil {
		return nil, files, err
	}
	files.stdin = stdin
	files.stdoutRead, files.stdoutWrite, err = os.Pipe()
	if err != nil {
		files.closeAll()
		return nil, commandFiles{}, err
	}
	files.stderrRead, files.stderrWrite, err = os.Pipe()
	if err != nil {
		files.closeAll()
		return nil, commandFiles{}, err
	}

	command := exec.Command(commandPath, append([]string(nil), spec.Args...)...)
	command.Args[0] = spec.Path
	command.Dir = spec.Directory
	command.Env = make([]string, len(spec.Environment))
	copy(command.Env, spec.Environment)
	command.Stdin = files.stdin
	command.Stdout = files.stdoutWrite
	command.Stderr = files.stderrWrite
	command.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	return command, files, nil
}

func resolveExecutable(spec Spec) (string, error) {
	if strings.ContainsRune(spec.Path, filepath.Separator) {
		return spec.Path, nil
	}

	requestPath, found := environmentValue(spec.Environment, "PATH")
	if !found {
		return "", sandbox.ErrChildLaunch
	}
	base, err := filepath.Abs(spec.Directory)
	if err != nil {
		return "", sandbox.ErrChildLaunch
	}
	for _, directory := range filepath.SplitList(requestPath) {
		if directory == "" {
			directory = "."
		}
		if !filepath.IsAbs(directory) {
			directory = filepath.Join(base, directory)
		}
		candidate := filepath.Join(directory, spec.Path)
		info, statErr := os.Stat(candidate)
		if statErr != nil || info.IsDir() {
			continue
		}
		if accessErr := unix.Access(candidate, unix.X_OK); accessErr == nil {
			return candidate, nil
		}
	}
	return "", sandbox.ErrChildLaunch
}

func environmentValue(environment []string, name string) (string, bool) {
	prefix := name + "="
	for _, entry := range environment {
		if strings.HasPrefix(entry, prefix) {
			return strings.TrimPrefix(entry, prefix), true
		}
	}
	return "", false
}

func closeStartedChildFiles(files commandFiles) error {
	var closeErr error
	for _, file := range []*os.File{files.stdin, files.stdoutWrite, files.stderrWrite} {
		if file != nil {
			if err := file.Close(); err != nil && !errors.Is(err, os.ErrClosed) {
				closeErr = sandbox.ErrChildWait
			}
		}
	}
	return closeErr
}

func (files commandFiles) closeAll() {
	for _, file := range []*os.File{
		files.stdin,
		files.stdoutRead,
		files.stdoutWrite,
		files.stderrRead,
		files.stderrWrite,
	} {
		if file != nil {
			_ = file.Close()
		}
	}
}

func (reader *ownedPipeReader) Read(buffer []byte) (int, error) {
	count, err := reader.file.Read(buffer)
	if err != nil && reader.closing.Load() && errors.Is(err, os.ErrClosed) {
		return count, deliberatePipeCloseError{}
	}
	return count, err
}

func (reader *ownedPipeReader) Close() error {
	reader.closing.Store(true)
	return reader.file.Close()
}

func closeOwnedReader(reader *ownedPipeReader) error {
	if err := reader.Close(); err != nil && !errors.Is(err, os.ErrClosed) {
		return sandbox.ErrChildWait
	}
	return nil
}

func forward(destination io.Writer, source *ownedPipeReader) <-chan error {
	done := make(chan error, 1)
	go func() {
		var copyErr error
		defer func() {
			if recover() != nil {
				copyErr = sandbox.ErrChildWait
			}
			done <- copyErr
		}()
		_, copyErr = io.Copy(destination, source)
	}()
	return done
}

func finishForwarders(proc *process, stdoutDone, stderrDone <-chan error, stdoutErr, stderrErr error) (error, error, error) {
	timer := time.NewTimer(forwarderDeadlockDeadline)
	defer timer.Stop()
	timeout := timer.C
	var closeErr error
	for stdoutDone != nil || stderrDone != nil {
		select {
		case stdoutErr = <-stdoutDone:
			stdoutDone = nil
		case stderrErr = <-stderrDone:
			stderrDone = nil
		case <-timeout:
			closeErr = joinBoundedErrors(closeOwnedReader(proc.stdout), closeOwnedReader(proc.stderr))
			timeout = nil
		}
	}
	closeErr = joinBoundedErrors(closeErr, closeOwnedReader(proc.stdout), closeOwnedReader(proc.stderr))
	return stdoutErr, stderrErr, closeErr
}

func copyInfrastructureError(err error) error {
	if err == nil {
		return nil
	}
	var deliberate deliberatePipeCloseError
	if errors.As(err, &deliberate) {
		return nil
	}
	return sandbox.ErrChildWait
}

func (m *Manager) terminate(proc *process) error {
	pid := proc.command.Process.Pid
	err := m.kill(-pid, syscall.SIGKILL)
	if err == nil || errors.Is(err, syscall.ESRCH) {
		return nil
	}
	// Darwin can report EPERM after the leader was reaped. Accept it only when
	// one deterministic postcondition check proves that the group is absent.
	if runtime.GOOS == "darwin" && errors.Is(err, syscall.EPERM) {
		if probeErr := m.kill(-pid, 0); errors.Is(probeErr, syscall.ESRCH) {
			return nil
		}
	}
	return sandbox.ErrChildTerminate
}

func inspectResult(command *exec.Cmd, waitErr error) (Result, error) {
	state := command.ProcessState
	if state == nil {
		return Result{}, sandbox.ErrChildWait
	}
	result := Result{Code: state.ExitCode()}
	if waitStatus, ok := state.Sys().(syscall.WaitStatus); ok && waitStatus.Signaled() {
		result.Code = -1
		result.Signaled = true
		result.Signal = waitStatus.Signal().String()
	}
	if waitErr == nil {
		return result, nil
	}
	var exitErr *exec.ExitError
	if errors.As(waitErr, &exitErr) {
		return result, nil
	}
	return result, sandbox.ErrChildWait
}

func (m *Manager) complete(proc *process, cleanupErr error) {
	proc.mu.Lock()
	proc.cleanupErr = cleanupErr
	proc.mu.Unlock()

	m.mu.Lock()
	delete(m.active, proc)
	m.mu.Unlock()
	close(proc.done)
}

func (proc *process) error() error {
	proc.mu.Lock()
	defer proc.mu.Unlock()
	return proc.cleanupErr
}

func safeContextError(ctx context.Context) error {
	if ctx.Err() == nil {
		return nil
	}
	if errors.Is(ctx.Err(), context.DeadlineExceeded) || errors.Is(context.Cause(ctx), context.DeadlineExceeded) {
		return context.DeadlineExceeded
	}
	return context.Canceled
}

func joinBoundedErrors(values ...error) error {
	known := [...]error{
		sandbox.ErrChildLaunch,
		sandbox.ErrChildWait,
		sandbox.ErrChildTerminate,
		context.Canceled,
		context.DeadlineExceeded,
	}
	joined := make([]error, 0, len(known))
	for _, identity := range known {
		for _, value := range values {
			if errors.Is(value, identity) {
				joined = append(joined, identity)
				break
			}
		}
	}
	switch len(joined) {
	case 0:
		return nil
	case 1:
		return joined[0]
	default:
		return errors.Join(joined...)
	}
}
