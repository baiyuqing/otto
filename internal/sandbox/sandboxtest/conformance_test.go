//go:build darwin || linux

package sandboxtest

import (
	"context"
	"io"
	"path/filepath"
	"sync"
	"testing"

	"github.com/baiyuqing/otto/internal/sandbox"
)

type streamWorkDriver struct {
	drainClose bool
	workDone   chan struct{}
	doneOnce   sync.Once
}

func (d *streamWorkDriver) ID() sandbox.DriverID { return "stream-work" }

func (d *streamWorkDriver) Capabilities() sandbox.Capabilities {
	return sandbox.Capabilities{NetworkAllow: true}
}

func (d *streamWorkDriver) Execute(_ context.Context, _ sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
	_, err := streams.Stdout.Write([]byte("driver-owned work"))
	d.doneOnce.Do(func() { close(d.workDone) })
	return sandbox.ExitStatus{}, err
}

func (d *streamWorkDriver) Close() error {
	if d.drainClose {
		<-d.workDone
	}
	return nil
}

type testDriverWorkBarrierWriter struct {
	entered chan struct{}
	release <-chan struct{}
	once    sync.Once
}

func (w *testDriverWorkBarrierWriter) Write(data []byte) (int, error) {
	w.once.Do(func() { close(w.entered) })
	<-w.release
	return len(data), nil
}

func TestCloseDrainObserverDetectsReturnWhileDriverWorkIsBlocked(t *testing.T) {
	workRelease := make(chan struct{})
	var workReleaseOnce sync.Once
	releaseWork := func() { workReleaseOnce.Do(func() { close(workRelease) }) }
	defer releaseWork()

	base := &streamWorkDriver{workDone: make(chan struct{})}
	observer := newCloseDrainObserver(base, workRelease, 1)
	writer := &testDriverWorkBarrierWriter{entered: make(chan struct{}), release: workRelease}
	executeResult := make(chan execution, 1)
	go func() {
		status, err := observer.Execute(context.Background(), sandbox.Request{}, sandbox.Streams{Stdout: writer})
		executeResult <- execution{status: status, err: err}
	}()
	awaitSignal(t, writer.entered, "blocked Driver-owned stream work")

	closeResult := make(chan error, 1)
	go func() { closeResult <- observer.Close() }()
	awaitSignal(t, observer.closeDelegateEntered, "Driver.Close observation delegate entry")
	observation := awaitCloseDrainObservation(t, observer.closeReturned)
	if !observation.returnedWhileDriverWorkBlocked {
		t.Fatal("observer missed Close returning while Driver-owned work was blocked")
	}
	if err := awaitError(t, closeResult, "non-draining Close return while Driver work is blocked"); err != nil {
		t.Fatalf("Close() error = %v", err)
	}

	releaseWork()
	completed := awaitExecution(t, executeResult, "instrumented Execute completion")
	if completed.err != nil {
		t.Fatalf("Execute() error = %v", completed.err)
	}
}

func TestCloseDrainObserverAcceptsReturnAfterDriverWorkBeforeCallerAcknowledgment(t *testing.T) {
	workRelease := make(chan struct{})
	callerAcknowledge := make(chan struct{})
	var workReleaseOnce sync.Once
	var callerAcknowledgeOnce sync.Once
	releaseWork := func() { workReleaseOnce.Do(func() { close(workRelease) }) }
	acknowledgeCaller := func() { callerAcknowledgeOnce.Do(func() { close(callerAcknowledge) }) }
	defer acknowledgeCaller()
	defer releaseWork()

	base := &streamWorkDriver{drainClose: true, workDone: make(chan struct{})}
	observer := newCloseDrainObserver(base, workRelease, 1)
	writer := &testDriverWorkBarrierWriter{entered: make(chan struct{}), release: workRelease}
	awaitingCallerAcknowledgment := make(chan struct{})
	executeResult := make(chan execution, 1)
	go func() {
		status, err := observer.Execute(context.Background(), sandbox.Request{}, sandbox.Streams{Stdout: writer})
		close(awaitingCallerAcknowledgment)
		<-callerAcknowledge
		executeResult <- execution{status: status, err: err}
	}()
	awaitSignal(t, writer.entered, "blocked Driver-owned stream work")

	closeResult := make(chan error, 1)
	go func() { closeResult <- observer.Close() }()
	awaitSignal(t, observer.closeDelegateEntered, "Driver.Close observation delegate entry")
	releaseWork()
	awaitSignal(t, awaitingCallerAcknowledgment, "external Execute caller acknowledgment barrier")

	observation := awaitCloseDrainObservation(t, observer.closeReturned)
	if observation.returnedWhileDriverWorkBlocked || observation.err != nil {
		t.Fatalf("Close observation after Driver work drained = %+v", observation)
	}
	if err := awaitError(t, closeResult, "Close before external caller acknowledgment"); err != nil {
		t.Fatalf("Close() error = %v", err)
	}

	acknowledgeCaller()
	completed := awaitExecution(t, executeResult, "acknowledged Execute result")
	if completed.err != nil {
		t.Fatalf("Execute() error = %v", completed.err)
	}
}

func TestDecoratedExecutorCleanupBypassesUndrainedCloseObservation(t *testing.T) {
	const observedCloseCallers = 12
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	base := &recordingPolicyDriver{
		capabilities: sandbox.Capabilities{NetworkAllow: true},
		closed:       make(chan struct{}),
	}
	driverWorkReleased := make(chan struct{})
	close(driverWorkReleased)
	observer := newCloseDrainObserver(base, driverWorkReleased, observedCloseCallers)
	if _, err := observer.Execute(context.Background(), sandbox.Request{}, sandbox.Streams{}); err != nil {
		t.Fatalf("observed Execute() error = %v", err)
	}
	executor, err := sandbox.NewExecutor(observer, sandbox.Policy{
		Filesystem: sandbox.FilesystemUnconfined,
		Network:    sandbox.NetworkAllow,
	}, workspace)
	if err != nil {
		t.Fatalf("sandbox.NewExecutor() error = %v", err)
	}
	cleanup := contractExecutorCleanup(base, executor, true)

	for range observedCloseCallers {
		if err := observer.Close(); err != nil {
			t.Fatalf("observed Close() error = %v", err)
		}
	}
	for range observedCloseCallers {
		awaitSignal(t, observer.closeDelegateEntered, "Driver.Close observation delegate entry")
	}
	if len(observer.closeReturned) != cap(observer.closeReturned) {
		t.Fatal("test did not fill the undrained Close observation channel")
	}

	cleanupDone := make(chan struct{})
	go func() {
		cleanup()
		close(cleanupDone)
	}()
	awaitSignal(t, cleanupDone, "decorated Executor raw-Driver cleanup")
	if got := base.closeCallCount(); got != observedCloseCallers+1 {
		t.Fatalf("raw Driver.Close() calls = %d, want %d observed closes plus cleanup", got, observedCloseCallers)
	}
}

type recordingPolicyDriver struct {
	capabilities sandbox.Capabilities
	closeOnce    sync.Once
	closed       chan struct{}
	mu           sync.Mutex
	closeCalls   int
}

func (d *recordingPolicyDriver) ID() sandbox.DriverID { return "policy-recorder" }

func (d *recordingPolicyDriver) Capabilities() sandbox.Capabilities { return d.capabilities }

func (d *recordingPolicyDriver) Execute(context.Context, sandbox.Request, sandbox.Streams) (sandbox.ExitStatus, error) {
	return sandbox.ExitStatus{}, nil
}

func (d *recordingPolicyDriver) Close() error {
	d.mu.Lock()
	d.closeCalls++
	d.mu.Unlock()
	d.closeOnce.Do(func() { close(d.closed) })
	return nil
}

func (d *recordingPolicyDriver) closeCallCount() int {
	d.mu.Lock()
	defer d.mu.Unlock()
	return d.closeCalls
}

func TestEnvironmentDumpMatchesCompleteRequestEnvironment(t *testing.T) {
	expected := []string{
		"PATH=/request/bin:/usr/bin",
		"LC_ALL=C",
		"VISIBLE=controlled-value",
	}
	if !environmentDumpMatches("PATH=/request/bin:/usr/bin\nLC_ALL=C\nVISIBLE=controlled-value\n", expected) {
		t.Fatal("exact environment dump did not match request environment")
	}
	for name, dump := range map[string]string{
		"injected HOME":     "HOME=/host/home\nPATH=/request/bin:/usr/bin\nLC_ALL=C\nVISIBLE=controlled-value\n",
		"injected USER":     "PATH=/request/bin:/usr/bin\nUSER=host-user\nLC_ALL=C\nVISIBLE=controlled-value\n",
		"injected host env": "PATH=/request/bin:/usr/bin\nLC_ALL=C\nVISIBLE=controlled-value\nHOST_ONLY=injected\n",
		"missing entry":     "PATH=/request/bin:/usr/bin\nVISIBLE=controlled-value\n",
		"reordered entries": "LC_ALL=C\nPATH=/request/bin:/usr/bin\nVISIBLE=controlled-value\n",
		"truncated output":  "PATH=/request/bin:/usr/bin\nLC_ALL=C\nVISIBLE=controlled-value",
	} {
		t.Run(name, func(t *testing.T) {
			if environmentDumpMatches(dump, expected) {
				t.Fatal("non-exact environment dump matched request environment")
			}
		})
	}
}

func TestContractExecutorFactoryUsesMatchingPoliciesForDualNetworkCapabilities(t *testing.T) {
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	capabilities := sandbox.Capabilities{
		ReadConfinement:  true,
		WriteConfinement: true,
		NetworkAllow:     true,
		NetworkDeny:      true,
		UnixSocketDeny:   true,
	}
	var mu sync.Mutex
	var constructedPolicies []sandbox.Policy
	var drivers []*recordingPolicyDriver
	fixture := Fixture{Workspace: workspace}
	factory := newContractExecutorFactory(Case{
		NewDriver: func(_ testing.TB, received Fixture) sandbox.Driver {
			driver := &recordingPolicyDriver{capabilities: capabilities, closed: make(chan struct{})}
			mu.Lock()
			constructedPolicies = append(constructedPolicies, received.Policy)
			drivers = append(drivers, driver)
			mu.Unlock()
			return driver
		},
	}, fixture, capabilities)

	allowPolicy, ok := policyForNetworkCapability(capabilities, sandbox.NetworkAllow)
	if !ok {
		t.Fatal("dual-capability Driver has no allow policy")
	}
	denyPolicy, ok := policyForNetworkCapability(capabilities, sandbox.NetworkDeny)
	if !ok {
		t.Fatal("dual-capability Driver has no deny policy")
	}
	wantAllow := sandbox.Policy{Filesystem: sandbox.FilesystemWorkspaceWrite, Network: sandbox.NetworkAllow}
	wantDeny := sandbox.Policy{Filesystem: sandbox.FilesystemWorkspaceWrite, Network: sandbox.NetworkDeny}
	if allowPolicy != wantAllow || denyPolicy != wantDeny {
		t.Fatalf("network policies = (%+v, %+v), want (%+v, %+v)", allowPolicy, denyPolicy, wantAllow, wantDeny)
	}

	for _, policy := range []sandbox.Policy{allowPolicy, denyPolicy} {
		_, executor, local := factory.open(t, policy)
		if local.Policy != policy {
			t.Fatalf("constructed fixture policy = %+v, want %+v", local.Policy, policy)
		}
		if _, err := executor.Execute(context.Background(), sandbox.Request{
			Argv: []string{"ignored"},
			Dir:  workspace,
			Env:  []string{},
		}, sandbox.Streams{Stdout: io.Discard, Stderr: io.Discard}); err != nil {
			t.Fatalf("Execute(policy=%+v) error = %v", policy, err)
		}
		if err := executor.Close(); err != nil {
			t.Fatalf("Executor.Close(policy=%+v) error = %v", policy, err)
		}
	}

	mu.Lock()
	gotPolicies := append([]sandbox.Policy(nil), constructedPolicies...)
	gotDrivers := append([]*recordingPolicyDriver(nil), drivers...)
	mu.Unlock()
	if len(gotPolicies) != 2 || gotPolicies[0] != wantAllow || gotPolicies[1] != wantDeny {
		t.Fatalf("NewDriver fixture policies = %+v, want allow then deny", gotPolicies)
	}
	if len(gotDrivers) != 2 || gotDrivers[0] == gotDrivers[1] {
		t.Fatal("allow and deny checks did not receive fresh Drivers")
	}
	for index, driver := range gotDrivers {
		select {
		case <-driver.closed:
		default:
			t.Fatalf("Driver %d was leaked", index)
		}
	}
}
