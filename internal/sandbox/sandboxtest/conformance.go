//go:build darwin || linux

package sandboxtest

import (
	"bytes"
	"context"
	"errors"
	"io"
	"net"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/sandbox"
	"golang.org/x/sys/unix"
)

type Fixture struct {
	Workspace   string
	OutsideFile string
	AllowedRead string
	UnixSocket  string
	Environment []string
	Policy      sandbox.Policy
}

type Case struct {
	NewDriver    func(testing.TB, Fixture) sandbox.Driver
	Request      func(testing.TB, Fixture, []string) sandbox.Request
	ShellCommand func(testing.TB, string) []string
	TCPClient    func(testing.TB, string) []string
	UnixClient   func(testing.TB, string) []string
}

type execution struct {
	status sandbox.ExitStatus
	err    error
}

type contractExecutorFactory struct {
	testCase     Case
	fixture      Fixture
	capabilities sandbox.Capabilities
}

func newContractExecutorFactory(testCase Case, fixture Fixture, capabilities sandbox.Capabilities) contractExecutorFactory {
	return contractExecutorFactory{testCase: testCase, fixture: fixture, capabilities: capabilities}
}

func (f contractExecutorFactory) open(t testing.TB, policy sandbox.Policy) (sandbox.Driver, *sandbox.Executor, Fixture) {
	return f.openDecorated(t, policy, nil)
}

func (f contractExecutorFactory) openDecorated(t testing.TB, policy sandbox.Policy, decorate func(sandbox.Driver) sandbox.Driver) (sandbox.Driver, *sandbox.Executor, Fixture) {
	t.Helper()
	fixture := f.fixture
	fixture.Policy = policy
	base := f.testCase.NewDriver(t, fixture)
	if isNilDriver(base) {
		t.Fatal("NewDriver returned nil")
	}
	if got := base.Capabilities(); got != f.capabilities {
		_ = base.Close()
		t.Fatalf("Driver capabilities changed between constructions: got %+v want %+v", got, f.capabilities)
	}
	driver := base
	decorated := decorate != nil
	if decorated {
		driver = decorate(base)
		if isNilDriver(driver) {
			_ = base.Close()
			t.Fatal("Driver decorator returned nil")
		}
	}
	executor, err := sandbox.NewExecutor(driver, policy, fixture.Workspace)
	if err != nil {
		_ = base.Close()
		t.Fatalf("sandbox.NewExecutor() error = %v", err)
	}
	t.Cleanup(contractExecutorCleanup(base, executor, decorated))
	return driver, executor, fixture
}

func contractExecutorCleanup(base sandbox.Driver, executor *sandbox.Executor, decorated bool) func() {
	if decorated {
		// Observation channels may be intentionally left full on a failed
		// conformance assertion. Cleanup must not re-enter the decorator.
		return func() { _ = base.Close() }
	}
	return func() { _ = executor.Close() }
}

type closeDrainObservation struct {
	err                     error
	beforeExecuteCompletion bool
}

// closeInvocationObserver is the callee beneath closeDrainObserver. Its
// invocation event therefore proves that the outer Close crossed the delegate
// call boundary rather than merely reaching a point before that call.
type closeInvocationObserver struct {
	sandbox.Driver
	invoked                   chan<- struct{}
	returned                  chan<- closeDrainObservation
	executeReturnRelease      <-chan struct{}
	executeCompletionObserved <-chan struct{}
}

func (d *closeInvocationObserver) Close() error {
	d.invoked <- struct{}{}
	err := d.Driver.Close()
	observation := closeDrainObservation{err: err}
	// A return while the explicit Execute-return gate remains closed is a
	// deterministic return-before-completion. Once that gate is released, hold
	// the observed Close until the caller has itself observed Execute return.
	select {
	case <-d.executeCompletionObserved:
	default:
		select {
		case <-d.executeReturnRelease:
			<-d.executeCompletionObserved
		default:
			observation.beforeExecuteCompletion = true
		}
	}
	d.returned <- observation
	<-d.executeCompletionObserved
	return err
}

type closeDrainObserver struct {
	driver                    sandbox.Driver
	underlyingCloseInvoked    chan struct{}
	closeReturned             chan closeDrainObservation
	executeCompletionObserved chan struct{}
	executeCompletionOnce     sync.Once
}

func newCloseDrainObserver(driver sandbox.Driver, executeReturnRelease <-chan struct{}, closeCallers int) *closeDrainObserver {
	underlyingCloseInvoked := make(chan struct{}, closeCallers)
	closeReturned := make(chan closeDrainObservation, closeCallers)
	executeCompletionObserved := make(chan struct{})
	return &closeDrainObserver{
		driver: &closeInvocationObserver{
			Driver:                    driver,
			invoked:                   underlyingCloseInvoked,
			returned:                  closeReturned,
			executeReturnRelease:      executeReturnRelease,
			executeCompletionObserved: executeCompletionObserved,
		},
		underlyingCloseInvoked:    underlyingCloseInvoked,
		closeReturned:             closeReturned,
		executeCompletionObserved: executeCompletionObserved,
	}
}

func (d *closeDrainObserver) ID() sandbox.DriverID { return d.driver.ID() }

func (d *closeDrainObserver) Capabilities() sandbox.Capabilities { return d.driver.Capabilities() }

func (d *closeDrainObserver) Execute(ctx context.Context, request sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
	return d.driver.Execute(ctx, request, streams)
}

func (d *closeDrainObserver) observeExecuteCompletion() {
	d.executeCompletionOnce.Do(func() { close(d.executeCompletionObserved) })
}

func (d *closeDrainObserver) Close() error {
	return d.driver.Close()
}

type executeReturnBarrierWriter struct {
	*synchronizedWriter
	entered              chan struct{}
	streamRelease        <-chan struct{}
	executeReturnBlocked chan struct{}
	executeReturnRelease <-chan struct{}
	enteredOnce          sync.Once
	blockedOnce          sync.Once
}

func newExecuteReturnBarrierWriter(streamRelease, executeReturnRelease <-chan struct{}) *executeReturnBarrierWriter {
	return &executeReturnBarrierWriter{
		synchronizedWriter:   newSynchronizedWriter(),
		entered:              make(chan struct{}),
		streamRelease:        streamRelease,
		executeReturnBlocked: make(chan struct{}),
		executeReturnRelease: executeReturnRelease,
	}
}

func (w *executeReturnBarrierWriter) Write(data []byte) (int, error) {
	written, err := w.synchronizedWriter.Write(data)
	w.enteredOnce.Do(func() { close(w.entered) })
	<-w.streamRelease
	w.blockedOnce.Do(func() { close(w.executeReturnBlocked) })
	<-w.executeReturnRelease
	return written, err
}

type synchronizedWriter struct {
	mu       sync.Mutex
	buffer   bytes.Buffer
	reported int
	lines    chan string
}

func newSynchronizedWriter() *synchronizedWriter {
	return &synchronizedWriter{lines: make(chan string, 4)}
}

func (w *synchronizedWriter) Write(data []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()

	written, err := w.buffer.Write(data)
	for {
		unreported := w.buffer.Bytes()[w.reported:]
		lineEnd := bytes.IndexByte(unreported, '\n')
		if lineEnd < 0 {
			break
		}
		w.lines <- string(unreported[:lineEnd])
		w.reported += lineEnd + 1
	}
	return written, err
}

func (w *synchronizedWriter) String() string {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.buffer.String()
}

func RunDriverContract(t *testing.T, testCase Case) {
	t.Helper()
	validateCase(t, testCase)
	fixture := newFixture(t)
	probe := testCase.NewDriver(t, fixture)
	if isNilDriver(probe) {
		t.Fatal("NewDriver returned nil")
	}
	t.Cleanup(func() { _ = probe.Close() })
	capabilities := probe.Capabilities()
	fixture.Policy = policyForCapabilities(t, capabilities)
	if err := probe.Close(); err != nil {
		t.Fatalf("probe Driver.Close() error = %v", err)
	}

	factory := newContractExecutorFactory(testCase, fixture, capabilities)
	newExecutor := func(t *testing.T) (sandbox.Driver, *sandbox.Executor) {
		driver, executor, _ := factory.open(t, fixture.Policy)
		return driver, executor
	}

	t.Run("ordinary and nonzero exits with separate streams", func(t *testing.T) {
		_, executor := newExecutor(t)
		var stdout bytes.Buffer
		var stderr bytes.Buffer
		request := shellRequest(t, testCase, fixture, `printf stdout-contract; printf stderr-contract >&2`)
		status, err := executor.Execute(context.Background(), request, sandbox.Streams{Stdout: &stdout, Stderr: &stderr})
		if err != nil {
			t.Fatalf("Execute() error = %v", err)
		}
		if status.Code != 0 || status.Signaled || status.Signal != "" {
			t.Fatalf("zero-exit status = %+v", status)
		}
		if stdout.String() != "stdout-contract" || stderr.String() != "stderr-contract" {
			t.Fatalf("separate streams = (%q, %q)", stdout.String(), stderr.String())
		}

		status, err = executor.Execute(context.Background(), shellRequest(t, testCase, fixture, "exit 37"), discardStreams())
		if err != nil {
			t.Fatalf("nonzero Execute() error = %v", err)
		}
		if status.Code != 37 || status.Signaled || status.Signal != "" {
			t.Fatalf("nonzero status = %+v", status)
		}
	})

	t.Run("supplied environment exactly replaces host environment", func(t *testing.T) {
		t.Setenv("HOME", "/host-only-home")
		t.Setenv("USER", "host-only-user")
		t.Setenv("OTTO_SANDBOX_CONFORMANCE_HOST_ONLY", "must-not-reach-child")
		_, executor := newExecutor(t)
		var stdout bytes.Buffer
		request := testCase.Request(t, fixture, []string{"/usr/bin/env"})
		for _, entry := range request.Env {
			if strings.ContainsAny(entry, "\r\n") {
				t.Fatal("environment fixture contains a newline")
			}
		}
		status, err := executor.Execute(context.Background(), request, sandbox.Streams{Stdout: &stdout, Stderr: io.Discard})
		if err != nil || status.Code != 0 {
			t.Fatalf("Execute() = (%+v, %v)", status, err)
		}
		if !environmentDumpMatches(stdout.String(), request.Env) {
			t.Fatal("complete child environment differs from Request.Env")
		}
	})

	t.Run("pre-cancellation does not start a child", func(t *testing.T) {
		_, executor := newExecutor(t)
		marker := filepath.Join(fixture.Workspace, "pre-cancel marker")
		local := fixtureWithEnvironment(fixture, "SANDBOX_CONFORMANCE_MARKER", marker)
		ctx, cancel := context.WithCancel(context.Background())
		cancel()
		_, err := executor.Execute(ctx, shellRequest(t, testCase, local, `printf started > "$SANDBOX_CONFORMANCE_MARKER"`), discardStreams())
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("Execute() error = %v, want context.Canceled", err)
		}
		if _, statErr := os.Lstat(marker); !os.IsNotExist(statErr) {
			t.Fatalf("pre-cancelled child created marker: %v", statErr)
		}
	})

	t.Run("deadline cancellation removes the process group", func(t *testing.T) {
		_, executor := newExecutor(t)
		blocked := makeFIFO(t, fixture.Workspace, "deadline-blocked")
		local := fixtureWithEnvironment(fixture, "SANDBOX_CONFORMANCE_BLOCK_FIFO", blocked)
		ctx, cancel := context.WithCancelCause(context.Background())
		stdout := newSynchronizedWriter()
		result := make(chan execution, 1)
		go func() {
			status, err := executor.Execute(ctx, shellRequest(t, testCase, local, `/bin/sh -c 'echo "$$"; exec /bin/cat "$SANDBOX_CONFORMANCE_BLOCK_FIFO"' descendant & wait`), sandbox.Streams{Stdout: stdout, Stderr: io.Discard})
			result <- execution{status: status, err: err}
		}()
		pid := parsePID(t, awaitLine(t, stdout.lines, "deadline descendant PID"))
		cancel(context.DeadlineExceeded)
		completed := awaitExecution(t, result, "deadline cancellation")
		if !errors.Is(completed.err, context.DeadlineExceeded) {
			t.Fatalf("Execute() error = %v, want context.DeadlineExceeded", completed.err)
		}
		assertSignaled(t, completed.status)
		assertGoneOnce(t, pid)
	})

	t.Run("normal leader exit removes a background process", func(t *testing.T) {
		_, executor := newExecutor(t)
		blocked := makeFIFO(t, fixture.Workspace, "normal-exit-blocked")
		release := makeFIFO(t, fixture.Workspace, "normal-exit-release")
		local := fixtureWithEnvironment(fixture, "SANDBOX_CONFORMANCE_BLOCK_FIFO", blocked)
		local = fixtureWithEnvironment(local, "SANDBOX_CONFORMANCE_RELEASE_FIFO", release)
		stdout := newSynchronizedWriter()
		result := make(chan execution, 1)
		go func() {
			status, err := executor.Execute(context.Background(), shellRequest(t, testCase, local, `/bin/sh -c 'echo "$$"; exec /bin/cat "$SANDBOX_CONFORMANCE_BLOCK_FIFO"' descendant & IFS= read -r release < "$SANDBOX_CONFORMANCE_RELEASE_FIFO"; exit 0`), sandbox.Streams{Stdout: stdout, Stderr: io.Discard})
			result <- execution{status: status, err: err}
		}()
		descendantPID := parsePID(t, awaitLine(t, stdout.lines, "background descendant PID"))
		writeFIFO(t, release, "continue\n")
		completed := awaitExecution(t, result, "normal leader exit")
		if completed.err != nil || completed.status.Code != 0 || completed.status.Signaled {
			t.Fatalf("Execute() = (%+v, %v)", completed.status, completed.err)
		}
		assertGoneOnce(t, descendantPID)
	})

	t.Run("driver close drains active work and is idempotent", func(t *testing.T) {
		const closeCallers = 12
		streamRelease := make(chan struct{})
		executeReturnRelease := make(chan struct{})
		var streamReleaseOnce sync.Once
		var executeReturnReleaseOnce sync.Once
		releaseStream := func() { streamReleaseOnce.Do(func() { close(streamRelease) }) }
		releaseExecuteReturn := func() { executeReturnReleaseOnce.Do(func() { close(executeReturnRelease) }) }
		defer releaseStream()
		defer releaseExecuteReturn()

		var observer *closeDrainObserver
		driver, executor, local := factory.openDecorated(t, fixture.Policy, func(base sandbox.Driver) sandbox.Driver {
			observer = newCloseDrainObserver(base, executeReturnRelease, closeCallers)
			return observer
		})
		blocked := makeFIFO(t, local.Workspace, "close-blocked")
		local = fixtureWithEnvironment(local, "SANDBOX_CONFORMANCE_BLOCK_FIFO", blocked)
		stdout := newExecuteReturnBarrierWriter(streamRelease, executeReturnRelease)
		executeResult := make(chan execution, 1)
		go func() {
			status, err := executor.Execute(context.Background(), shellRequest(t, testCase, local, `echo "$$"; exec /bin/cat "$SANDBOX_CONFORMANCE_BLOCK_FIFO"`), sandbox.Streams{Stdout: stdout, Stderr: io.Discard})
			executeResult <- execution{status: status, err: err}
			observer.observeExecuteCompletion()
		}()
		pid := parsePID(t, awaitLine(t, stdout.lines, "close leader PID"))
		awaitSignal(t, stdout.entered, "active Execute output barrier")

		start := make(chan struct{})
		closeResults := make(chan error, closeCallers)
		for range closeCallers {
			go func() {
				<-start
				closeResults <- driver.Close()
			}()
		}
		close(start)
		for range closeCallers {
			awaitSignal(t, observer.underlyingCloseInvoked, "underlying Driver.Close invocation")
		}

		releaseStream()
		awaitSignal(t, stdout.executeReturnBlocked, "post-stream Execute return barrier")
		select {
		case returned := <-observer.closeReturned:
			t.Fatalf("Driver.Close() returned while active Execute was causally blocked: %v", returned.err)
		default:
		}

		releaseExecuteReturn()
		completed := awaitExecution(t, executeResult, "active Execute completion during Driver.Close")
		if completed.err != nil {
			t.Fatalf("active Execute() error = %v", completed.err)
		}
		assertSignaled(t, completed.status)
		assertGoneOnce(t, pid)
		for range closeCallers {
			observation := awaitCloseDrainObservation(t, observer.closeReturned)
			if observation.beforeExecuteCompletion || observation.err != nil {
				t.Fatalf("Driver.Close() observation = %+v", observation)
			}
			if err := awaitError(t, closeResults, "concurrent Driver.Close"); err != nil {
				t.Fatalf("Driver.Close() error = %v", err)
			}
		}
		if err := driver.Close(); err != nil {
			t.Fatalf("idempotent Driver.Close() error = %v", err)
		}
		if _, err := executor.Execute(context.Background(), shellRequest(t, testCase, local, "exit 0"), discardStreams()); !errors.Is(err, sandbox.ErrClosed) {
			t.Fatalf("Execute() after Driver.Close error = %v, want ErrClosed", err)
		}
	})

	t.Run("executor clones requests before delegation", func(t *testing.T) {
		_, executor := newExecutor(t)
		release := makeFIFO(t, fixture.Workspace, "clone-release")
		local := fixtureWithEnvironment(fixture, "SANDBOX_CONFORMANCE_RELEASE_FIFO", release)
		stdout := newSynchronizedWriter()
		request := shellRequest(t, testCase, local, `printf '%s\n' "$$"; IFS= read -r release < "$SANDBOX_CONFORMANCE_RELEASE_FIFO"; printf 'clone:%s' "$SANDBOX_CONFORMANCE_VISIBLE"`)
		result := make(chan execution, 1)
		go func() {
			status, err := executor.Execute(context.Background(), request, sandbox.Streams{Stdout: stdout, Stderr: io.Discard})
			result <- execution{status: status, err: err}
		}()
		_ = parsePID(t, awaitLine(t, stdout.lines, "request clone leader PID"))
		for index := range request.Argv {
			request.Argv[index] = "mutated-argument"
		}
		for index := range request.Env {
			request.Env[index] = "MUTATED_ENVIRONMENT=value"
		}
		writeFIFO(t, release, "continue\n")
		completed := awaitExecution(t, result, "request clone execution")
		if completed.err != nil || completed.status.Code != 0 {
			t.Fatalf("Execute() = (%+v, %v)", completed.status, completed.err)
		}
		if !strings.Contains(stdout.String(), "clone:visible-value") {
			t.Fatalf("cloned request output = %q", stdout.String())
		}
	})

	t.Run("canonical paths with spaces unicode and quotes", func(t *testing.T) {
		_, executor := newExecutor(t)
		var stdout bytes.Buffer
		request := shellRequest(t, testCase, fixture, `printf special-data > "$SANDBOX_CONFORMANCE_SPECIAL_PATH"; /bin/pwd`)
		status, err := executor.Execute(context.Background(), request, sandbox.Streams{Stdout: &stdout, Stderr: io.Discard})
		if err != nil || status.Code != 0 {
			t.Fatalf("Execute() = (%+v, %v)", status, err)
		}
		data, err := os.ReadFile(environmentValue(t, fixture.Environment, "SANDBOX_CONFORMANCE_SPECIAL_PATH"))
		if err != nil || string(data) != "special-data" {
			t.Fatalf("special-path file = (%q, %v)", data, err)
		}
		if strings.TrimSpace(stdout.String()) != fixture.Workspace {
			t.Fatalf("pwd = %q, want workspace", stdout.String())
		}
	})

	t.Run("validation errors are bounded", func(t *testing.T) {
		_, executor := newExecutor(t)
		sensitivePath := filepath.Join(fixture.Workspace, "missing secret path")
		request := shellRequest(t, testCase, fixture, "exit 0")
		request.Dir = sensitivePath
		request.Argv = append(request.Argv, "secret-argument-value")
		request.Env = append(request.Env, "SECRET_VALUE=secret-environment-value")
		_, err := executor.Execute(context.Background(), request, discardStreams())
		if !errors.Is(err, sandbox.ErrInvalidRequest) || err.Error() != sandbox.ErrInvalidRequest.Error() {
			t.Fatalf("Execute() error = %v, want fixed ErrInvalidRequest", err)
		}
		for _, secret := range []string{sensitivePath, "secret-argument-value", "secret-environment-value"} {
			if strings.Contains(err.Error(), secret) {
				t.Fatalf("validation error exposed request data: %v", err)
			}
		}
	})

	t.Run("concurrent calls complete", func(t *testing.T) {
		_, executor := newExecutor(t)
		const callers = 16
		start := make(chan struct{})
		results := make(chan execution, callers)
		for range callers {
			go func() {
				<-start
				status, err := executor.Execute(context.Background(), shellRequest(t, testCase, fixture, "exit 0"), discardStreams())
				results <- execution{status: status, err: err}
			}()
		}
		close(start)
		for range callers {
			completed := awaitExecution(t, results, "concurrent Execute")
			if completed.err != nil || completed.status.Code != 0 || completed.status.Signaled {
				t.Fatalf("concurrent Execute() = (%+v, %v)", completed.status, completed.err)
			}
		}
	})

	if capabilities.ReadConfinement || capabilities.WriteConfinement {
		runFilesystemCapabilityChecks(t, testCase, fixture, capabilities, newExecutor)
	}
	if capabilities.NetworkAllow {
		policy, ok := policyForNetworkCapability(capabilities, sandbox.NetworkAllow)
		if !ok {
			t.Fatal("Driver advertises NetworkAllow without a matching conformance policy")
		}
		runTCPAllowCheck(t, testCase, factory, policy)
	}
	if capabilities.NetworkDeny {
		policy, ok := policyForNetworkCapability(capabilities, sandbox.NetworkDeny)
		if !ok {
			t.Fatal("Driver advertises NetworkDeny without a matching conformance policy")
		}
		runTCPDenyCheck(t, testCase, factory, policy)
	}
	if capabilities.UnixSocketDeny {
		runUnixDenyCheck(t, testCase, fixture, newExecutor)
	}
}

func runFilesystemCapabilityChecks(t *testing.T, testCase Case, fixture Fixture, capabilities sandbox.Capabilities, newExecutor func(*testing.T) (sandbox.Driver, *sandbox.Executor)) {
	t.Helper()
	if capabilities.ReadConfinement {
		t.Run("advertised read confinement", func(t *testing.T) {
			_, executor := newExecutor(t)
			local := fixtureWithEnvironment(fixture, "SANDBOX_CONFORMANCE_READ_PATH", fixture.AllowedRead)
			status, err := executor.Execute(context.Background(), shellRequest(t, testCase, local, `/bin/cat "$SANDBOX_CONFORMANCE_READ_PATH"`), discardStreams())
			if err != nil || status.Code != 0 {
				t.Fatalf("allowed read Execute() = (%+v, %v)", status, err)
			}

			local = fixtureWithEnvironment(fixture, "SANDBOX_CONFORMANCE_READ_PATH", fixture.OutsideFile)
			status, err = executor.Execute(context.Background(), shellRequest(t, testCase, local, `/bin/cat "$SANDBOX_CONFORMANCE_READ_PATH"`), discardStreams())
			if err != nil || status.Code == 0 {
				t.Fatalf("outside read Execute() = (%+v, %v), want denied status", status, err)
			}

			link := filepath.Join(fixture.Workspace, "outside read symlink")
			if err := os.Symlink(fixture.OutsideFile, link); err != nil {
				t.Fatal(err)
			}
			local = fixtureWithEnvironment(fixture, "SANDBOX_CONFORMANCE_READ_PATH", link)
			status, err = executor.Execute(context.Background(), shellRequest(t, testCase, local, `/bin/cat "$SANDBOX_CONFORMANCE_READ_PATH"`), discardStreams())
			if err != nil || status.Code == 0 {
				t.Fatalf("symlink read Execute() = (%+v, %v), want denied status", status, err)
			}
		})
	}

	if capabilities.WriteConfinement {
		t.Run("advertised write confinement", func(t *testing.T) {
			_, executor := newExecutor(t)
			outsideWrite := filepath.Join(filepath.Dir(fixture.Workspace), "outside write target")
			local := fixtureWithEnvironment(fixture, "SANDBOX_CONFORMANCE_WRITE_PATH", outsideWrite)
			status, err := executor.Execute(context.Background(), shellRequest(t, testCase, local, `printf denied > "$SANDBOX_CONFORMANCE_WRITE_PATH"`), discardStreams())
			if err != nil || status.Code == 0 {
				t.Fatalf("outside write Execute() = (%+v, %v), want denied status", status, err)
			}
			if _, statErr := os.Lstat(outsideWrite); !os.IsNotExist(statErr) {
				t.Fatalf("outside write target exists: %v", statErr)
			}

			target := filepath.Join(filepath.Dir(fixture.Workspace), "outside symlink target")
			if err := os.WriteFile(target, []byte("unchanged"), 0o600); err != nil {
				t.Fatal(err)
			}
			link := filepath.Join(fixture.Workspace, "outside write symlink")
			if err := os.Symlink(target, link); err != nil {
				t.Fatal(err)
			}
			local = fixtureWithEnvironment(fixture, "SANDBOX_CONFORMANCE_WRITE_PATH", link)
			status, err = executor.Execute(context.Background(), shellRequest(t, testCase, local, `printf changed > "$SANDBOX_CONFORMANCE_WRITE_PATH"`), discardStreams())
			if err != nil || status.Code == 0 {
				t.Fatalf("symlink write Execute() = (%+v, %v), want denied status", status, err)
			}
			data, readErr := os.ReadFile(target)
			if readErr != nil || string(data) != "unchanged" {
				t.Fatalf("outside symlink target changed: data=%q error=%v", data, readErr)
			}
		})
	}
}

func runTCPAllowCheck(t *testing.T, testCase Case, factory contractExecutorFactory, policy sandbox.Policy) {
	t.Helper()
	t.Run("advertised local TCP allow", func(t *testing.T) {
		if testCase.TCPClient == nil {
			t.Fatal("Driver advertises NetworkAllow without a TCPClient adapter")
		}
		listener, err := net.Listen("tcp4", "127.0.0.1:0")
		if err != nil {
			t.Fatal(err)
		}
		defer listener.Close()
		accepted := acceptOne(listener)
		_, executor, fixture := factory.open(t, policy)
		request := testCase.Request(t, fixture, testCase.TCPClient(t, listener.Addr().String()))
		status, err := executor.Execute(context.Background(), request, discardStreams())
		if err != nil || status.Code != 0 {
			t.Fatalf("TCP client Execute() = (%+v, %v)", status, err)
		}
		if err := awaitError(t, accepted, "allowed TCP accept"); err != nil {
			t.Fatalf("TCP accept error = %v", err)
		}
	})
}

func runTCPDenyCheck(t *testing.T, testCase Case, factory contractExecutorFactory, policy sandbox.Policy) {
	t.Helper()
	t.Run("advertised local TCP deny", func(t *testing.T) {
		if testCase.TCPClient == nil {
			t.Fatal("Driver advertises NetworkDeny without a TCPClient adapter")
		}
		listener, err := net.Listen("tcp4", "127.0.0.1:0")
		if err != nil {
			t.Fatal(err)
		}
		accepted := acceptOne(listener)
		_, executor, fixture := factory.open(t, policy)
		request := testCase.Request(t, fixture, testCase.TCPClient(t, listener.Addr().String()))
		status, err := executor.Execute(context.Background(), request, discardStreams())
		if err != nil || status.Code == 0 {
			_ = listener.Close()
			t.Fatalf("TCP deny Execute() = (%+v, %v), want denied status", status, err)
		}
		if err := listener.Close(); err != nil {
			t.Fatal(err)
		}
		if err := awaitError(t, accepted, "denied TCP accept"); err == nil {
			t.Fatal("TCP listener accepted a connection despite NetworkDeny")
		}
	})
}

func runUnixDenyCheck(t *testing.T, testCase Case, fixture Fixture, newExecutor func(*testing.T) (sandbox.Driver, *sandbox.Executor)) {
	t.Helper()
	t.Run("advertised Unix socket deny", func(t *testing.T) {
		if testCase.UnixClient == nil {
			t.Fatal("Driver advertises UnixSocketDeny without a UnixClient adapter")
		}
		_ = os.Remove(fixture.UnixSocket)
		listener, err := net.Listen("unix", fixture.UnixSocket)
		if err != nil {
			t.Fatal(err)
		}
		accepted := acceptOne(listener)
		_, executor := newExecutor(t)
		request := testCase.Request(t, fixture, testCase.UnixClient(t, fixture.UnixSocket))
		status, err := executor.Execute(context.Background(), request, discardStreams())
		if err != nil || status.Code == 0 {
			_ = listener.Close()
			t.Fatalf("Unix deny Execute() = (%+v, %v), want denied status", status, err)
		}
		if err := listener.Close(); err != nil {
			t.Fatal(err)
		}
		if err := awaitError(t, accepted, "denied Unix accept"); err == nil {
			t.Fatal("Unix listener accepted a connection despite UnixSocketDeny")
		}
	})
}

func validateCase(t *testing.T, testCase Case) {
	t.Helper()
	if testCase.NewDriver == nil || testCase.Request == nil || testCase.ShellCommand == nil {
		t.Fatal("Driver conformance Case is missing a required adapter")
	}
}

func newFixture(t *testing.T) Fixture {
	t.Helper()
	temporary, err := os.MkdirTemp("/tmp", "otto-sandbox-contract-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := os.RemoveAll(temporary); err != nil {
			t.Errorf("remove conformance fixture: %v", err)
		}
	})
	base, err := filepath.EvalSymlinks(temporary)
	if err != nil {
		t.Fatal(err)
	}
	workspace := filepath.Join(base, "workspace space ü 'quote;()[]")
	if err := os.Mkdir(workspace, 0o700); err != nil {
		t.Fatal(err)
	}
	outside := filepath.Join(base, "outside file")
	allowed := filepath.Join(base, "allowed read file")
	for path, content := range map[string]string{outside: "outside-data", allowed: "allowed-data"} {
		if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	special := filepath.Join(workspace, "special space ü 'quote;()[] file")
	return Fixture{
		Workspace:   workspace,
		OutsideFile: outside,
		AllowedRead: allowed,
		UnixSocket:  filepath.Join(workspace, "contract.sock"),
		Environment: []string{
			"PATH=/usr/bin:/bin",
			"LC_ALL=C",
			"SANDBOX_CONFORMANCE_VISIBLE=visible-value",
			"SANDBOX_CONFORMANCE_SPECIAL_PATH=" + special,
		},
		Policy: sandbox.Policy{Filesystem: sandbox.FilesystemWorkspaceWrite, Network: sandbox.NetworkAllow},
	}
}

func policyForCapabilities(t *testing.T, capabilities sandbox.Capabilities) sandbox.Policy {
	t.Helper()
	if policy, ok := policyForNetworkCapability(capabilities, sandbox.NetworkAllow); ok {
		return policy
	}
	if policy, ok := policyForNetworkCapability(capabilities, sandbox.NetworkDeny); ok {
		return policy
	}
	t.Fatal("Driver capabilities cannot satisfy a conformance policy")
	return sandbox.Policy{}
}

func policyForNetworkCapability(capabilities sandbox.Capabilities, network sandbox.NetworkMode) (sandbox.Policy, bool) {
	confined := capabilities.ReadConfinement && capabilities.WriteConfinement && capabilities.UnixSocketDeny
	switch network {
	case sandbox.NetworkAllow:
		if !capabilities.NetworkAllow {
			return sandbox.Policy{}, false
		}
		filesystem := sandbox.FilesystemUnconfined
		if confined {
			filesystem = sandbox.FilesystemWorkspaceWrite
		}
		return sandbox.Policy{Filesystem: filesystem, Network: network}, true
	case sandbox.NetworkDeny:
		if !capabilities.NetworkDeny || !confined {
			return sandbox.Policy{}, false
		}
		return sandbox.Policy{Filesystem: sandbox.FilesystemWorkspaceWrite, Network: network}, true
	default:
		return sandbox.Policy{}, false
	}
}

func shellRequest(t testing.TB, testCase Case, fixture Fixture, script string) sandbox.Request {
	t.Helper()
	return testCase.Request(t, fixture, testCase.ShellCommand(t, script))
}

func fixtureWithEnvironment(fixture Fixture, name, value string) Fixture {
	fixture.Environment = append([]string{}, fixture.Environment...)
	fixture.Environment = append(fixture.Environment, name+"="+value)
	return fixture
}

func environmentDumpMatches(dump string, expected []string) bool {
	if len(expected) == 0 {
		return dump == ""
	}
	for _, entry := range expected {
		if strings.ContainsAny(entry, "\r\n") {
			return false
		}
	}
	if !strings.HasSuffix(dump, "\n") {
		return false
	}
	actual := strings.Split(strings.TrimSuffix(dump, "\n"), "\n")
	return slices.Equal(actual, expected)
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
	if err := awaitError(t, result, "FIFO release"); err != nil {
		t.Fatalf("write FIFO error = %v", err)
	}
}

func environmentValue(t *testing.T, environment []string, name string) string {
	t.Helper()
	prefix := name + "="
	for _, entry := range environment {
		if strings.HasPrefix(entry, prefix) {
			return strings.TrimPrefix(entry, prefix)
		}
	}
	t.Fatalf("environment does not contain %s", name)
	return ""
}

func discardStreams() sandbox.Streams {
	return sandbox.Streams{Stdout: io.Discard, Stderr: io.Discard}
}

func acceptOne(listener net.Listener) <-chan error {
	result := make(chan error, 1)
	go func() {
		connection, err := listener.Accept()
		if err == nil {
			err = connection.Close()
		}
		result <- err
	}()
	return result
}

func awaitSignal(t *testing.T, signal <-chan struct{}, description string) {
	t.Helper()
	select {
	case <-signal:
	case <-time.After(10 * time.Second):
		t.Fatalf("timed out waiting for %s", description)
	}
}

func awaitCloseDrainObservation(t *testing.T, result <-chan closeDrainObservation) closeDrainObservation {
	t.Helper()
	select {
	case observation := <-result:
		return observation
	case <-time.After(10 * time.Second):
		t.Fatal("timed out waiting for Driver.Close observation")
		return closeDrainObservation{}
	}
}

func awaitLine(t *testing.T, lines <-chan string, description string) string {
	t.Helper()
	select {
	case line := <-lines:
		return line
	case <-time.After(10 * time.Second):
		t.Fatalf("timed out waiting for %s", description)
		return ""
	}
}

func awaitExecution(t *testing.T, result <-chan execution, description string) execution {
	t.Helper()
	select {
	case completed := <-result:
		return completed
	case <-time.After(10 * time.Second):
		t.Fatalf("timed out waiting for %s", description)
		return execution{}
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

func parsePID(t *testing.T, value string) int {
	t.Helper()
	pid, err := strconv.Atoi(strings.TrimSpace(value))
	if err != nil || pid <= 0 {
		t.Fatalf("invalid child PID %q", value)
	}
	return pid
}

func assertSignaled(t *testing.T, status sandbox.ExitStatus) {
	t.Helper()
	if status.Code != -1 || !status.Signaled || status.Signal == "" {
		t.Fatalf("status = %+v, want signal termination", status)
	}
}

func assertGoneOnce(t *testing.T, pid int) {
	t.Helper()
	if err := unix.Kill(pid, 0); !errors.Is(err, unix.ESRCH) {
		if err == nil {
			_ = unix.Kill(pid, unix.SIGKILL)
		}
		t.Fatalf("signal 0 for process %d returned %v, want ESRCH", pid, err)
	}
}

func isNilDriver(driver sandbox.Driver) bool {
	if driver == nil {
		return true
	}
	value := reflect.ValueOf(driver)
	switch value.Kind() {
	case reflect.Chan, reflect.Func, reflect.Interface, reflect.Map, reflect.Pointer, reflect.Slice:
		return value.IsNil()
	default:
		return false
	}
}
