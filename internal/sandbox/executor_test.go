package sandbox

import (
	"bytes"
	"context"
	"errors"
	"io"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

type fakeDriver struct {
	id   DriverID
	caps Capabilities

	mu             sync.Mutex
	requests       []Request
	executeCalls   int
	closeCalls     int
	closedForCalls bool
	closeErr       error

	started         chan struct{}
	release         chan struct{}
	executeFinished chan struct{}
	closeStarted    chan struct{}
	closeRelease    chan struct{}
	closed          chan struct{}

	startedOnce         sync.Once
	executeFinishedOnce sync.Once
	closeStartedOnce    sync.Once
	closedOnce          sync.Once
}

func newFakeDriver() *fakeDriver {
	return &fakeDriver{
		id: "fake-driver",
		caps: Capabilities{
			ReadConfinement:  true,
			WriteConfinement: true,
			NetworkDeny:      true,
			NetworkAllow:     true,
			UnixSocketDeny:   true,
		},
		started:         make(chan struct{}),
		executeFinished: make(chan struct{}),
		closeStarted:    make(chan struct{}),
		closed:          make(chan struct{}),
	}
}

func (d *fakeDriver) ID() DriverID { return d.id }

func (d *fakeDriver) Capabilities() Capabilities { return d.caps }

func (d *fakeDriver) Execute(ctx context.Context, request Request, _ Streams) (ExitStatus, error) {
	defer d.executeFinishedOnce.Do(func() { close(d.executeFinished) })

	d.mu.Lock()
	d.executeCalls++
	d.requests = append(d.requests, request.Clone())
	release := d.release
	d.mu.Unlock()

	d.startedOnce.Do(func() { close(d.started) })
	if release != nil {
		select {
		case <-release:
		case <-ctx.Done():
			return ExitStatus{}, ctx.Err()
		}
	}
	return ExitStatus{Code: 0}, nil
}

func (d *fakeDriver) Close() error {
	d.mu.Lock()
	d.closeCalls++
	d.closedForCalls = true
	release := d.closeRelease
	err := d.closeErr
	d.mu.Unlock()

	d.closeStartedOnce.Do(func() { close(d.closeStarted) })
	if release != nil {
		<-release
	}
	d.closedOnce.Do(func() { close(d.closed) })
	return err
}

func (d *fakeDriver) snapshot() ([]Request, int, int) {
	d.mu.Lock()
	defer d.mu.Unlock()
	requests := make([]Request, len(d.requests))
	copy(requests, d.requests)
	return requests, d.executeCalls, d.closeCalls
}

type nilDriver struct{}

func (*nilDriver) ID() DriverID { return "nil-driver" }
func (*nilDriver) Capabilities() Capabilities {
	return Capabilities{}
}
func (*nilDriver) Execute(context.Context, Request, Streams) (ExitStatus, error) {
	return ExitStatus{}, nil
}
func (*nilDriver) Close() error { return nil }

type nilWriter struct{}

func (*nilWriter) Write(p []byte) (int, error) { return len(p), nil }

func TestFakeDriverRecordsClonedRequest(t *testing.T) {
	driver := newFakeDriver()
	request := Request{Argv: []string{"original"}, Env: []string{"NAME=original"}}

	if _, err := driver.Execute(context.Background(), request, Streams{}); err != nil {
		t.Fatalf("Execute() error = %v", err)
	}
	request.Argv[0] = "mutated"
	request.Env[0] = "NAME=mutated"

	recorded, _, _ := driver.snapshot()
	if recorded[0].Argv[0] != "original" || recorded[0].Env[0] != "NAME=original" {
		t.Fatalf("recorded request retained caller storage: Argv=%q Env=%q", recorded[0].Argv, recorded[0].Env)
	}
}

func TestNewExecutorRejectsNilAndTypedNilDriver(t *testing.T) {
	workspace := t.TempDir()
	policy := Policy{Filesystem: FilesystemWorkspaceWrite, Network: NetworkDeny}

	for _, test := range []struct {
		name   string
		driver Driver
	}{
		{name: "nil interface"},
		{name: "typed nil", driver: (*nilDriver)(nil)},
	} {
		t.Run(test.name, func(t *testing.T) {
			executor, err := NewExecutor(test.driver, policy, workspace)
			if err == nil {
				t.Fatal("NewExecutor() error = nil, want rejection")
			}
			if executor != nil {
				t.Fatal("NewExecutor() returned a non-nil executor")
			}
		})
	}
}

func TestNewExecutorRejectsEmptyDriverIDAndUnsupportedPolicy(t *testing.T) {
	workspace := t.TempDir()

	for _, id := range []DriverID{"", "UPPER", "has_underscore", "has space", "abcdefghijklmnopqrstuvwxyz1234567"} {
		t.Run("invalid ID "+string(id), func(t *testing.T) {
			driver := newFakeDriver()
			driver.id = id
			executor, err := NewExecutor(driver, Policy{Filesystem: FilesystemWorkspaceWrite, Network: NetworkDeny}, workspace)
			if err == nil || executor != nil {
				t.Fatalf("NewExecutor() = (%v, %v), want nil executor and error", executor, err)
			}
		})
	}

	tests := []struct {
		name   string
		policy Policy
		caps   Capabilities
	}{
		{
			name:   "workspace write needs read confinement",
			policy: Policy{Filesystem: FilesystemWorkspaceWrite, Network: NetworkAllow},
			caps:   Capabilities{WriteConfinement: true, NetworkAllow: true, UnixSocketDeny: true},
		},
		{
			name:   "workspace write needs write confinement",
			policy: Policy{Filesystem: FilesystemWorkspaceWrite, Network: NetworkAllow},
			caps:   Capabilities{ReadConfinement: true, NetworkAllow: true, UnixSocketDeny: true},
		},
		{
			name:   "workspace write needs unix socket denial",
			policy: Policy{Filesystem: FilesystemWorkspaceWrite, Network: NetworkAllow},
			caps:   Capabilities{ReadConfinement: true, WriteConfinement: true, NetworkAllow: true},
		},
		{
			name:   "workspace write needs selected network deny",
			policy: Policy{Filesystem: FilesystemWorkspaceWrite, Network: NetworkDeny},
			caps:   Capabilities{ReadConfinement: true, WriteConfinement: true, NetworkAllow: true, UnixSocketDeny: true},
		},
		{
			name:   "workspace write needs selected network allow",
			policy: Policy{Filesystem: FilesystemWorkspaceWrite, Network: NetworkAllow},
			caps:   Capabilities{ReadConfinement: true, WriteConfinement: true, NetworkDeny: true, UnixSocketDeny: true},
		},
		{
			name:   "unconfined rejects network deny",
			policy: Policy{Filesystem: FilesystemUnconfined, Network: NetworkDeny},
			caps:   Capabilities{NetworkDeny: true, NetworkAllow: true},
		},
		{
			name:   "unconfined needs network allow",
			policy: Policy{Filesystem: FilesystemUnconfined, Network: NetworkAllow},
			caps:   Capabilities{},
		},
		{
			name:   "unknown filesystem mode",
			policy: Policy{Filesystem: FilesystemMode(99), Network: NetworkAllow},
			caps:   Capabilities{NetworkAllow: true},
		},
		{
			name:   "unknown network mode",
			policy: Policy{Filesystem: FilesystemWorkspaceWrite, Network: NetworkMode(99)},
			caps:   newFakeDriver().caps,
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			driver := newFakeDriver()
			driver.caps = test.caps
			executor, err := NewExecutor(driver, test.policy, workspace)
			if !errors.Is(err, ErrUnsupportedPolicy) {
				t.Fatalf("NewExecutor() error = %v, want ErrUnsupportedPolicy", err)
			}
			if executor != nil {
				t.Fatal("NewExecutor() returned a non-nil executor")
			}
		})
	}
}

func TestNewExecutorCanonicalizesAndBindsWorkspace(t *testing.T) {
	parent, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	realWorkspace := filepath.Join(parent, "real-workspace")
	if err := os.Mkdir(realWorkspace, 0o700); err != nil {
		t.Fatal(err)
	}
	workspaceLink := filepath.Join(parent, "workspace-link")
	if err := os.Symlink(realWorkspace, workspaceLink); err != nil {
		t.Fatal(err)
	}
	child := filepath.Join(realWorkspace, "child")
	if err := os.Mkdir(child, 0o700); err != nil {
		t.Fatal(err)
	}

	driver := newFakeDriver()
	executor, err := NewExecutor(driver, Policy{Filesystem: FilesystemWorkspaceWrite, Network: NetworkDeny}, workspaceLink)
	if err != nil {
		t.Fatalf("NewExecutor() error = %v", err)
	}
	t.Cleanup(func() { _ = executor.Close() })

	for _, dir := range []string{realWorkspace, child} {
		if _, err := executor.Execute(context.Background(), validRequest(dir), validStreams()); err != nil {
			t.Fatalf("Execute(Dir=%q) error = %v", dir, err)
		}
	}

	other := t.TempDir()
	if _, err := executor.Execute(context.Background(), validRequest(other), validStreams()); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("Execute(outside workspace) error = %v, want ErrInvalidRequest", err)
	}
}

func TestExecutorDefensivelyCopiesArgvAndEnvironment(t *testing.T) {
	driver, executor, workspace := newTestExecutor(t)
	request := validRequest(workspace)
	originalArgv := append([]string(nil), request.Argv...)
	originalEnv := append([]string(nil), request.Env...)

	if _, err := executor.Execute(context.Background(), request, validStreams()); err != nil {
		t.Fatalf("Execute() error = %v", err)
	}
	recorded, _, _ := driver.snapshot()
	recorded[0].Argv[0] = "mutated-by-driver"
	recorded[0].Env[0] = "VALUE=mutated-by-driver"

	if request.Argv[0] != originalArgv[0] || request.Env[0] != originalEnv[0] {
		t.Fatalf("caller request was mutated: Argv=%q Env=%q", request.Argv, request.Env)
	}
	if _, err := executor.Execute(context.Background(), request, validStreams()); err != nil {
		t.Fatalf("second Execute() error = %v", err)
	}
	recorded, _, _ = driver.snapshot()
	if recorded[1].Argv[0] != originalArgv[0] || recorded[1].Env[0] != originalEnv[0] {
		t.Fatalf("retained request altered later call: Argv=%q Env=%q", recorded[1].Argv, recorded[1].Env)
	}

	clone := request.Clone()
	clone.Argv[0] = "clone"
	clone.Env[0] = "CLONE=value"
	if request.Argv[0] != originalArgv[0] || request.Env[0] != originalEnv[0] {
		t.Fatal("Request.Clone() shares slice storage")
	}
	settings := Settings{ReadPaths: []string{"/read"}, AllowEnv: []string{"SAFE"}}
	settingsClone := settings.Clone()
	settingsClone.ReadPaths[0] = "/changed"
	settingsClone.AllowEnv[0] = "CHANGED"
	if settings.ReadPaths[0] != "/read" || settings.AllowEnv[0] != "SAFE" {
		t.Fatal("Settings.Clone() shares slice storage")
	}
}

func TestExecutorRejectsNoncanonicalOrEscapedDirectory(t *testing.T) {
	_, executor, workspace := newTestExecutor(t)
	child := filepath.Join(workspace, "child")
	if err := os.Mkdir(child, 0o700); err != nil {
		t.Fatal(err)
	}
	link := filepath.Join(workspace, "child-link")
	if err := os.Symlink(child, link); err != nil {
		t.Fatal(err)
	}

	for _, test := range []struct {
		name string
		dir  string
	}{
		{name: "relative", dir: "."},
		{name: "unclean", dir: child + string(filepath.Separator) + "."},
		{name: "symlink", dir: link},
		{name: "missing", dir: filepath.Join(workspace, "missing")},
		{name: "escaped", dir: filepath.Dir(workspace)},
	} {
		t.Run(test.name, func(t *testing.T) {
			request := validRequest(workspace)
			request.Dir = test.dir
			if _, err := executor.Execute(context.Background(), request, validStreams()); !errors.Is(err, ErrInvalidRequest) {
				t.Fatalf("Execute() error = %v, want ErrInvalidRequest", err)
			}
		})
	}
}

func TestExecutorRejectsMalformedOrDuplicateEnvironment(t *testing.T) {
	driver, executor, workspace := newTestExecutor(t)
	for _, test := range []struct {
		name string
		env  []string
	}{
		{name: "nil environment", env: nil},
		{name: "missing equals", env: []string{"NAME"}},
		{name: "empty name", env: []string{"=value"}},
		{name: "invalid name", env: []string{"BAD-NAME=value"}},
		{name: "nul in name", env: []string{"BAD\x00NAME=value"}},
		{name: "nul in value", env: []string{"NAME=bad\x00value"}},
		{name: "duplicate", env: []string{"NAME=one", "NAME=two"}},
	} {
		t.Run(test.name, func(t *testing.T) {
			request := validRequest(workspace)
			request.Env = test.env
			if _, err := executor.Execute(context.Background(), request, validStreams()); !errors.Is(err, ErrInvalidRequest) {
				t.Fatalf("Execute() error = %v, want ErrInvalidRequest", err)
			}
		})
	}

	request := validRequest(workspace)
	request.Env = make([]string, 0)
	if _, err := executor.Execute(context.Background(), request, validStreams()); err != nil {
		t.Fatalf("Execute(explicit empty environment) error = %v", err)
	}
	recorded, _, _ := driver.snapshot()
	if recorded[len(recorded)-1].Env == nil {
		t.Fatal("Driver request environment = nil, want explicit empty environment")
	}

	clone := request.Clone()
	if clone.Env == nil {
		t.Fatal("Request.Clone() environment = nil, want explicit empty environment")
	}

	request = validRequest(workspace)
	request.Argv = []string{"sh", "bad\x00argument"}
	if _, err := executor.Execute(context.Background(), request, validStreams()); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("Execute(NUL argument) error = %v, want ErrInvalidRequest", err)
	}
	request = validRequest(workspace)
	request.Argv = nil
	if _, err := executor.Execute(context.Background(), request, validStreams()); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("Execute(empty Argv) error = %v, want ErrInvalidRequest", err)
	}
}

func TestExecutorBuildsFreshDriverRequest(t *testing.T) {
	driver, executor, workspace := newTestExecutor(t)
	request := validRequest(workspace)

	if _, err := executor.Execute(context.Background(), request, validStreams()); err != nil {
		t.Fatalf("first Execute() error = %v", err)
	}
	if _, err := executor.Execute(context.Background(), request, validStreams()); err != nil {
		t.Fatalf("second Execute() error = %v", err)
	}
	recorded, _, _ := driver.snapshot()
	if &recorded[0].Argv[0] == &recorded[1].Argv[0] {
		t.Fatal("driver requests share Argv backing storage")
	}
	if &recorded[0].Env[0] == &recorded[1].Env[0] {
		t.Fatal("driver requests share Env backing storage")
	}

	var typedNil io.Writer = (*nilWriter)(nil)
	streams := validStreams()
	streams.Stdout = typedNil
	if _, err := executor.Execute(context.Background(), request, streams); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("Execute(typed nil writer) error = %v, want ErrInvalidRequest", err)
	}
	streams = validStreams()
	streams.Stderr = nil
	if _, err := executor.Execute(context.Background(), request, streams); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("Execute(nil writer) error = %v, want ErrInvalidRequest", err)
	}
}

func TestExecutorPreservesCancellationIdentityWithoutCallingDriver(t *testing.T) {
	driver, executor, workspace := newTestExecutor(t)
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	if _, err := executor.Execute(ctx, validRequest(workspace), validStreams()); !errors.Is(err, context.Canceled) {
		t.Fatalf("Execute() error = %v, want context.Canceled", err)
	}
	_, calls, _ := driver.snapshot()
	if calls != 0 {
		t.Fatalf("Driver.Execute() calls = %d, want 0", calls)
	}
}

func TestExecutorCloseIsConcurrentAndIdempotent(t *testing.T) {
	driver, executor, _ := newTestExecutor(t)
	driver.closeRelease = make(chan struct{})
	closeIdentity := errors.New("fixed close failure")
	driver.closeErr = closeIdentity

	const callers = 16
	results := make(chan error, callers)
	start := make(chan struct{})
	for range callers {
		go func() {
			<-start
			results <- executor.Close()
		}()
	}
	close(start)
	awaitSignal(t, driver.closeStarted, "Driver.Close start")
	close(driver.closeRelease)

	for range callers {
		if err := <-results; !errors.Is(err, closeIdentity) {
			t.Fatalf("Close() error = %v, want cached first result", err)
		}
	}
	if err := executor.Close(); !errors.Is(err, closeIdentity) {
		t.Fatalf("later Close() error = %v, want cached first result", err)
	}
	_, _, closeCalls := driver.snapshot()
	if closeCalls != 1 {
		t.Fatalf("Driver.Close() calls = %d, want 1", closeCalls)
	}
}

func TestExecutorExecuteRacingCloseNeverRunsAfterClose(t *testing.T) {
	driver, executor, workspace := newTestExecutor(t)
	driver.release = make(chan struct{})
	driver.closeRelease = driver.executeFinished
	var releaseOnce sync.Once
	releaseExecute := func() { releaseOnce.Do(func() { close(driver.release) }) }
	defer releaseExecute()

	executeResult := make(chan error, 1)
	go func() {
		_, err := executor.Execute(context.Background(), validRequest(workspace), validStreams())
		executeResult <- err
	}()
	awaitSignal(t, driver.started, "Driver.Execute start")

	closeResult := make(chan error, 1)
	go func() { closeResult <- executor.Close() }()
	awaitSignal(t, driver.closeStarted, "Driver.Close start while Execute is active")

	select {
	case err := <-closeResult:
		t.Fatalf("Close() returned before Driver.Execute drained: %v", err)
	default:
	}
	if _, err := executor.Execute(context.Background(), validRequest(workspace), validStreams()); !errors.Is(err, ErrClosed) {
		t.Fatalf("Execute() during close error = %v, want ErrClosed", err)
	}
	_, executeCalls, _ := driver.snapshot()
	if executeCalls != 1 {
		t.Fatalf("Driver.Execute() calls = %d, want only the admitted active call", executeCalls)
	}

	releaseExecute()
	awaitErrorResult(t, executeResult, nil, "active Execute completion")
	awaitErrorResult(t, closeResult, nil, "Close completion after active Execute drained")

	if _, err := executor.Execute(context.Background(), validRequest(workspace), validStreams()); !errors.Is(err, ErrClosed) {
		t.Fatalf("Execute() after close error = %v, want ErrClosed", err)
	}
	select {
	case <-driver.closed:
	default:
		t.Fatal("Driver.Close() did not finish")
	}
}

func newTestExecutor(t *testing.T) (*fakeDriver, *Executor, string) {
	t.Helper()
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	driver := newFakeDriver()
	executor, err := NewExecutor(driver, Policy{Filesystem: FilesystemWorkspaceWrite, Network: NetworkDeny}, workspace)
	if err != nil {
		t.Fatalf("NewExecutor() error = %v", err)
	}
	t.Cleanup(func() { _ = executor.Close() })
	return driver, executor, workspace
}

func validRequest(workspace string) Request {
	return Request{
		Argv: []string{"sh", "-c", "exit 0"},
		Dir:  workspace,
		Env:  []string{"VALUE=original", "EMPTY="},
	}
}

func validStreams() Streams {
	return Streams{Stdout: &bytes.Buffer{}, Stderr: &bytes.Buffer{}}
}

func awaitSignal(t *testing.T, signal <-chan struct{}, description string) {
	t.Helper()
	select {
	case <-signal:
	case <-time.After(5 * time.Second):
		t.Fatalf("timed out waiting for %s", description)
	}
}

func awaitErrorResult(t *testing.T, result <-chan error, want error, description string) {
	t.Helper()
	select {
	case err := <-result:
		if want == nil && err != nil {
			t.Fatalf("%s error = %v, want nil", description, err)
		}
		if want != nil && !errors.Is(err, want) {
			t.Fatalf("%s error = %v, want %v", description, err, want)
		}
	case <-time.After(5 * time.Second):
		t.Fatalf("timed out waiting for %s", description)
	}
}
