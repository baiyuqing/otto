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

type nonDrainingDriver struct {
	executeStarted chan struct{}
	executeRelease chan struct{}
	closeRelease   chan struct{}
	startedOnce    sync.Once
}

func (d *nonDrainingDriver) ID() sandbox.DriverID { return "non-draining" }

func (d *nonDrainingDriver) Capabilities() sandbox.Capabilities {
	return sandbox.Capabilities{NetworkAllow: true}
}

func (d *nonDrainingDriver) Execute(context.Context, sandbox.Request, sandbox.Streams) (sandbox.ExitStatus, error) {
	d.startedOnce.Do(func() { close(d.executeStarted) })
	<-d.executeRelease
	return sandbox.ExitStatus{}, nil
}

func (d *nonDrainingDriver) Close() error {
	<-d.closeRelease
	return nil
}

func TestCloseDrainObserverDetectsReturnBeforeExecuteCompletion(t *testing.T) {
	closeRelease := make(chan struct{})
	executeRelease := make(chan struct{})
	var closeReleaseOnce sync.Once
	var executeReleaseOnce sync.Once
	releaseClose := func() { closeReleaseOnce.Do(func() { close(closeRelease) }) }
	releaseExecute := func() { executeReleaseOnce.Do(func() { close(executeRelease) }) }
	defer releaseClose()
	defer releaseExecute()

	base := &nonDrainingDriver{
		executeStarted: make(chan struct{}),
		executeRelease: executeRelease,
		closeRelease:   closeRelease,
	}
	observer := newCloseDrainObserver(base, executeRelease, 1)
	executeResult := make(chan execution, 1)
	go func() {
		status, err := observer.Execute(context.Background(), sandbox.Request{}, sandbox.Streams{})
		executeResult <- execution{status: status, err: err}
		observer.observeExecuteCompletion()
	}()
	awaitSignal(t, base.executeStarted, "instrumented Execute start")

	closeResult := make(chan error, 1)
	go func() { closeResult <- observer.Close() }()
	awaitSignal(t, observer.underlyingCloseInvoked, "underlying Close invocation")
	releaseClose()
	observation := awaitCloseDrainObservation(t, observer.closeReturned)
	if !observation.beforeExecuteCompletion {
		t.Fatal("observer missed Close returning before active Execute completed")
	}
	select {
	case completed := <-executeResult:
		t.Fatalf("Execute() completed before its deterministic release: %+v", completed)
	default:
	}

	releaseExecute()
	completed := awaitExecution(t, executeResult, "instrumented Execute completion")
	if completed.err != nil {
		t.Fatalf("Execute() error = %v", completed.err)
	}
	if err := awaitError(t, closeResult, "instrumented Close completion"); err != nil {
		t.Fatalf("Close() error = %v", err)
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
	executeReturnRelease := make(chan struct{})
	close(executeReturnRelease)
	observer := newCloseDrainObserver(base, executeReturnRelease, observedCloseCallers)
	if _, err := observer.Execute(context.Background(), sandbox.Request{}, sandbox.Streams{}); err != nil {
		t.Fatalf("observed Execute() error = %v", err)
	}
	observer.observeExecuteCompletion()
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
		awaitSignal(t, observer.underlyingCloseInvoked, "underlying Close invocation")
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
