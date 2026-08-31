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

func (*nonDrainingDriver) Close() error { return nil }

func TestCloseDrainObserverDetectsReturnBeforeExecutionRelease(t *testing.T) {
	release := make(chan struct{})
	var releaseOnce sync.Once
	releaseExecution := func() { releaseOnce.Do(func() { close(release) }) }
	defer releaseExecution()
	base := &nonDrainingDriver{executeStarted: make(chan struct{}), executeRelease: release}
	observer := newCloseDrainObserver(base, release, 1)
	executeResult := make(chan execution, 1)
	go func() {
		status, err := observer.Execute(context.Background(), sandbox.Request{}, sandbox.Streams{})
		executeResult <- execution{status: status, err: err}
	}()
	awaitSignal(t, base.executeStarted, "instrumented Execute start")

	if err := observer.Close(); err != nil {
		t.Fatalf("Close() error = %v", err)
	}
	observation := awaitCloseDrainObservation(t, observer.closeReturned)
	if !observation.beforeRelease {
		t.Fatal("observer missed Close returning before active execution release")
	}
	releaseExecution()
	completed := awaitExecution(t, executeResult, "instrumented Execute release")
	if completed.err != nil {
		t.Fatalf("Execute() error = %v", completed.err)
	}
}

type recordingPolicyDriver struct {
	capabilities sandbox.Capabilities
	closeOnce    sync.Once
	closed       chan struct{}
}

func (d *recordingPolicyDriver) ID() sandbox.DriverID { return "policy-recorder" }

func (d *recordingPolicyDriver) Capabilities() sandbox.Capabilities { return d.capabilities }

func (d *recordingPolicyDriver) Execute(context.Context, sandbox.Request, sandbox.Streams) (sandbox.ExitStatus, error) {
	return sandbox.ExitStatus{}, nil
}

func (d *recordingPolicyDriver) Close() error {
	d.closeOnce.Do(func() { close(d.closed) })
	return nil
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
