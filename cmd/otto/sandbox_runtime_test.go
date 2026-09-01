package main

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/safetext"
	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/sandbox/seatbelt"
)

func TestOpenSandboxAutoSelectsSeatbeltOnDarwin(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	fixture.settings.Driver = sandbox.DriverAuto

	runtime := openSandboxRuntimeWithDependencies(context.Background(), fixture.options(), fixture.dependencies())
	t.Cleanup(func() { _ = runtime.Close() })

	if fixture.seatbeltCalls.Load() != 1 || fixture.directCalls.Load() != 0 {
		t.Fatalf("driver calls = seatbelt %d direct %d, want 1 and 0", fixture.seatbeltCalls.Load(), fixture.directCalls.Load())
	}
	wantInfo := app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true, Reason: app.SandboxReasonNone}
	if runtime.Executor == nil || runtime.Environment == nil || runtime.Info != wantInfo {
		t.Fatalf("runtime = Executor %T Environment %#v Info %#v, want usable Seatbelt", runtime.Executor, runtime.Environment, runtime.Info)
	}
	if fixture.openOptions.Shell != fixture.shell || fixture.openOptions.Workspace != fixture.workspace {
		t.Fatalf("Seatbelt options = %#v, want canonical shell/workspace", fixture.openOptions)
	}
}

func TestOpenSandboxExplicitSeatbeltUsesSeatbelt(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	fixture.settings.Driver = sandbox.DriverSeatbelt
	fixture.settings.Network = sandbox.NetworkDeny

	runtime := openSandboxRuntimeWithDependencies(context.Background(), fixture.options(), fixture.dependencies())
	t.Cleanup(func() { _ = runtime.Close() })

	if fixture.seatbeltCalls.Load() != 1 || fixture.directCalls.Load() != 0 {
		t.Fatalf("driver calls = seatbelt %d direct %d, want 1 and 0", fixture.seatbeltCalls.Load(), fixture.directCalls.Load())
	}
	wantInfo := app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkDenied, BashAvailable: true, Reason: app.SandboxReasonNone}
	if runtime.Info != wantInfo {
		t.Fatalf("Info = %#v, want %#v", runtime.Info, wantInfo)
	}
	if fixture.openOptions.Network != sandbox.NetworkDeny {
		t.Fatalf("Seatbelt Network = %v, want deny", fixture.openOptions.Network)
	}
}

func TestOpenSandboxRejectsInvalidNetworkBeforeEveryRuntimeConstructor(t *testing.T) {
	for _, driver := range []sandbox.DriverMode{sandbox.DriverAuto, sandbox.DriverSeatbelt, sandbox.DriverOff} {
		for _, network := range []sandbox.NetworkMode{0, 99} {
			t.Run(string(driver)+"/network-"+fmt.Sprint(network), func(t *testing.T) {
				fixture := newSandboxRuntimeFixture(t)
				options := fixture.options()
				options.Settings.Driver = driver
				options.Settings.Network = network

				runtime := openSandboxRuntimeWithDependencies(context.Background(), options, fixture.dependencies())
				if runtime.Executor != nil || runtime.Info.Mode != app.SandboxUnavailable || runtime.Info.Reason != app.SandboxReasonPolicyUnsupported {
					t.Fatalf("runtime = %#v, want policy-unsupported unavailable", runtime)
				}
				if fixture.classifyCalls.Load() != 0 || fixture.resolveCalls.Load() != 0 || fixture.seatbeltCalls.Load() != 0 || fixture.directCalls.Load() != 0 || fixture.executorCalls.Load() != 0 {
					t.Fatalf("invalid network constructed state: classify/resolve/seatbelt/direct/executor = %d/%d/%d/%d/%d",
						fixture.classifyCalls.Load(), fixture.resolveCalls.Load(), fixture.seatbeltCalls.Load(), fixture.directCalls.Load(), fixture.executorCalls.Load())
				}
			})
		}
	}
}

func TestOpenSandboxRejectsUnsupportedSeatbeltPolicyBeforeOpeningDriver(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	options := fixture.options()
	options.Settings.Network = 0

	runtime := openSandboxRuntimeWithDependencies(context.Background(), options, fixture.dependencies())
	if runtime.Executor != nil || runtime.Info.Mode != app.SandboxUnavailable || runtime.Info.Reason != app.SandboxReasonPolicyUnsupported {
		t.Fatalf("runtime = %#v, want policy-unsupported unavailable result", runtime)
	}
	if fixture.seatbeltCalls.Load() != 0 || fixture.directCalls.Load() != 0 || fixture.executorCalls.Load() != 0 {
		t.Fatalf("unsupported policy opened resources: seatbelt=%d direct=%d executor=%d", fixture.seatbeltCalls.Load(), fixture.directCalls.Load(), fixture.executorCalls.Load())
	}
}

func TestOpenSandboxUnavailableRetainsClassifiedHostRedactions(t *testing.T) {
	for _, test := range []struct {
		name  string
		setup func(*sandboxRuntimeFixture, *sandboxOpenOptions)
	}{
		{
			name: "unsupported platform",
			setup: func(fixture *sandboxRuntimeFixture, _ *sandboxOpenOptions) {
				fixture.platform = "linux"
			},
		},
		{
			name: "invalid shell",
			setup: func(_ *sandboxRuntimeFixture, options *sandboxOpenOptions) {
				options.Shell = filepath.Join(t.TempDir(), "missing-shell")
			},
		},
		{
			name: "Seatbelt missing",
			setup: func(fixture *sandboxRuntimeFixture, _ *sandboxOpenOptions) {
				fixture.openErr = &sandbox.UnavailableError{Reason: sandbox.ReasonSeatbeltMissing}
			},
		},
		{
			name: "policy failure",
			setup: func(_ *sandboxRuntimeFixture, options *sandboxOpenOptions) {
				options.Workspace = filepath.Join(t.TempDir(), "missing-workspace")
			},
		},
		{
			name: "environment failure",
			setup: func(fixture *sandboxRuntimeFixture, options *sandboxOpenOptions) {
				options.HostEntries = append(options.HostEntries, "BROKEN")
				fixture.hostEntries = append(fixture.hostEntries, "BROKEN")
			},
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			fixture := newSandboxRuntimeFixture(t)
			fixture.hostEntries = append(fixture.hostEntries,
				"AWS_SECRET_ACCESS_KEY=host-aws-secret",
				"HTTPS_PROXY=http://raw%20user:raw%2Fpass@[::1]:8443/path",
			)
			options := fixture.options()
			test.setup(fixture, &options)

			runtime := openSandboxRuntimeWithDependencies(context.Background(), options, fixture.dependencies())
			if runtime.Executor != nil || runtime.Environment != nil {
				t.Fatalf("unavailable runtime retained command state: %#v", runtime)
			}
			for _, value := range []string{
				"host-aws-secret",
				"raw%20user:raw%2Fpass",
				"raw%20user",
				"raw%2Fpass",
				"raw user:raw/pass",
				"raw user",
				"raw/pass",
			} {
				if !containsString(runtime.RedactionValues, value) {
					t.Fatalf("redactions omitted required host value: %#v", runtime.RedactionValues)
				}
			}
		})
	}
}

func TestOpenSandboxPropagatesImmutableRedactionCompleteness(t *testing.T) {
	tests := []struct {
		name         string
		hostEntries  []string
		wantComplete bool
	}{
		{name: "complete", hostEntries: []string{"ORDINARY=value"}, wantComplete: true},
		{name: "malformed entry", hostEntries: []string{"BROKEN"}, wantComplete: false},
		{name: "ambiguous malformed proxy", hostEntries: []string{"HTTPS_PROXY=http:///user:pass@example.test"}, wantComplete: false},
		{name: "513 sensitive values", hostEntries: sensitiveSandboxRuntimeEntries(513), wantComplete: false},
		{name: "over one MiB", hostEntries: []string{"LARGE_TOKEN=" + strings.Repeat("z", (1<<20)+1)}, wantComplete: false},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			fixture := newSandboxRuntimeFixture(t)
			fixture.hostEntries = append([]string(nil), test.hostEntries...)
			runtime := openSandboxRuntimeWithDependencies(context.Background(), fixture.options(), fixture.dependencies())
			t.Cleanup(func() { _ = runtime.Close() })
			if runtime.RedactionsComplete != test.wantComplete {
				t.Fatalf("RedactionsComplete = %t, want %t", runtime.RedactionsComplete, test.wantComplete)
			}
			if !test.wantComplete && (runtime.Executor != nil || runtime.Info.Reason != app.SandboxReasonEnvironmentRejected) {
				t.Fatalf("incomplete runtime = %#v, want environment-rejected without Executor", runtime)
			}
		})
	}
}

func TestOpenSandboxFailureDisablesBashWithoutDirectFallback(t *testing.T) {
	for _, test := range []struct {
		name     string
		platform string
		driver   sandbox.DriverMode
		openErr  error
		reason   app.SandboxReason
		calls    int32
	}{
		{name: "auto Darwin open failure", platform: "darwin", driver: sandbox.DriverAuto, openErr: &sandbox.UnavailableError{Reason: sandbox.ReasonSelfTestFailed}, reason: app.SandboxReasonSelfTestFailed, calls: 1},
		{name: "seatbelt Darwin open failure", platform: "darwin", driver: sandbox.DriverSeatbelt, openErr: errors.New("raw Seatbelt failure"), reason: app.SandboxReasonRuntimeFailure, calls: 1},
		{name: "auto non-Darwin", platform: "linux", driver: sandbox.DriverAuto, reason: app.SandboxReasonUnsupportedPlatform},
		{name: "seatbelt non-Darwin", platform: "linux", driver: sandbox.DriverSeatbelt, reason: app.SandboxReasonUnsupportedPlatform},
	} {
		t.Run(test.name, func(t *testing.T) {
			fixture := newSandboxRuntimeFixture(t)
			fixture.platform = test.platform
			fixture.settings.Driver = test.driver
			fixture.openErr = test.openErr

			runtime := openSandboxRuntimeWithDependencies(context.Background(), fixture.options(), fixture.dependencies())
			if runtime.Executor != nil || runtime.Environment != nil {
				t.Fatalf("unavailable runtime retained execution state: %#v", runtime)
			}
			wantInfo := app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: test.reason}
			if runtime.Info != wantInfo {
				t.Fatalf("Info = %#v, want %#v", runtime.Info, wantInfo)
			}
			if fixture.seatbeltCalls.Load() != test.calls || fixture.directCalls.Load() != 0 {
				t.Fatalf("driver calls = seatbelt %d direct %d, want %d and 0", fixture.seatbeltCalls.Load(), fixture.directCalls.Load(), test.calls)
			}
			if err := runtime.Close(); err != nil {
				t.Fatalf("Close() = %v", err)
			}
		})
	}
}

func TestOpenSandboxCanceledContextConstructsNothing(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	options := fixture.options()
	options.Settings.Driver = sandbox.DriverOff
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	runtime := openSandboxRuntimeWithDependencies(ctx, options, fixture.dependencies())
	if runtime.Executor != nil || runtime.Environment != nil {
		t.Fatalf("canceled open returned command state: %#v", runtime)
	}
	if fixture.seatbeltCalls.Load() != 0 || fixture.directCalls.Load() != 0 || fixture.executorCalls.Load() != 0 {
		t.Fatalf("canceled open constructed resources: seatbelt/direct/executor = %d/%d/%d", fixture.seatbeltCalls.Load(), fixture.directCalls.Load(), fixture.executorCalls.Load())
	}
}

func TestOpenSandboxOffDoesNotRequireSeatbeltPlatformOrOpener(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	options := fixture.options()
	options.Settings.Driver = sandbox.DriverOff
	dependencies := fixture.dependencies()
	dependencies.platform = ""
	dependencies.openSeatbelt = nil

	runtime := openSandboxRuntimeWithDependencies(context.Background(), options, dependencies)
	if runtime.Executor == nil || runtime.Info.Mode != app.SandboxOff {
		t.Fatalf("runtime = %#v, want available Direct runtime", runtime)
	}
	if err := runtime.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestOpenSandboxOffUsesOnlyDirect(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	fixture.platform = "linux"
	fixture.settings.Driver = sandbox.DriverOff

	runtime := openSandboxRuntimeWithDependencies(context.Background(), fixture.options(), fixture.dependencies())
	t.Cleanup(func() { _ = runtime.Close() })

	if fixture.seatbeltCalls.Load() != 0 || fixture.directCalls.Load() != 1 {
		t.Fatalf("driver calls = seatbelt %d direct %d, want 0 and 1", fixture.seatbeltCalls.Load(), fixture.directCalls.Load())
	}
	wantInfo := app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true, Reason: app.SandboxReasonNone}
	if runtime.Executor == nil || runtime.Environment == nil || runtime.Info != wantInfo {
		t.Fatalf("runtime = Executor %T Environment %#v Info %#v, want usable direct", runtime.Executor, runtime.Environment, runtime.Info)
	}
}

func TestOpenSandboxOffIgnoresNetworkDenyAndReadPathsAndReportsUnsafe(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	fixture.settings = sandbox.Settings{
		Driver: sandbox.DriverOff, Network: sandbox.NetworkDeny,
		ReadPaths: []string{"/must/not/be/enforced"}, AllowEnv: []string{"PROJECT_TOKEN"},
	}

	runtime := openSandboxRuntimeWithDependencies(context.Background(), fixture.options(), fixture.dependencies())
	t.Cleanup(func() { _ = runtime.Close() })

	wantPolicy := sandbox.Policy{Filesystem: sandbox.FilesystemUnconfined, Network: sandbox.NetworkAllow}
	if fixture.executorPolicy != wantPolicy {
		t.Fatalf("Executor policy = %#v, want immutable unconfined allow %#v", fixture.executorPolicy, wantPolicy)
	}
	if fixture.seatbeltCalls.Load() != 0 || runtime.Info.Summary() != "sandbox off · WARNING: bash is unsandboxed" {
		t.Fatalf("runtime selection/status = calls %d info %#v", fixture.seatbeltCalls.Load(), runtime.Info)
	}
	if fixture.environmentOptions.PrivateDirectories != nil {
		t.Fatal("off mode supplied private directories to environment resolution")
	}
}

func TestOpenSandboxEnvironmentFailureClosesDriverAndDisablesBash(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	fixture.resolveErr = errors.New("raw environment value must not escape")

	runtime := openSandboxRuntimeWithDependencies(context.Background(), fixture.options(), fixture.dependencies())
	if fixture.driver.closeCalls.Load() != 1 {
		t.Fatalf("Driver close calls = %d, want 1", fixture.driver.closeCalls.Load())
	}
	if fixture.executorCalls.Load() != 0 || fixture.directCalls.Load() != 0 {
		t.Fatalf("executor/direct calls = %d/%d, want 0/0", fixture.executorCalls.Load(), fixture.directCalls.Load())
	}
	wantInfo := app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonEnvironmentRejected}
	if runtime.Executor != nil || runtime.Environment != nil || runtime.Info != wantInfo {
		t.Fatalf("runtime = %#v, want fixed environment rejection", runtime)
	}
	if err := runtime.Close(); err != nil {
		t.Fatalf("Close() = %v", err)
	}
	if fixture.driver.closeCalls.Load() != 1 {
		t.Fatalf("Close() closed partial Driver again: %d calls", fixture.driver.closeCalls.Load())
	}
}

func TestOpenSandboxPartialCleanupErrorIsDeferredAsFixedCloseError(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	fixture.resolveErr = sandbox.ErrEnvironmentUnsafe
	fixture.driver.closeErr = errors.New("raw partial cleanup state")

	runtime := openSandboxRuntimeWithDependencies(context.Background(), fixture.options(), fixture.dependencies())
	if fixture.driver.closeCalls.Load() != 1 {
		t.Fatalf("Driver close calls = %d, want eager partial cleanup", fixture.driver.closeCalls.Load())
	}
	if err := runtime.Close(); err != errSandboxRuntimeClose {
		t.Fatalf("runtime.Close() = %v, want fixed cleanup error", err)
	}
	if fixture.driver.closeCalls.Load() != 1 {
		t.Fatalf("runtime.Close() retried partial resource close: %d calls", fixture.driver.closeCalls.Load())
	}
}

func TestOpenSandboxProviderCredentialsAreNonRestorable(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	fixture.hostEntries = []string{
		"ACTIVE_KEY=active-value",
		"FALLBACK_KEY=fallback-value",
		"INACTIVE_KEY=inactive-value",
		"OTTO_API_KEY=otto-value",
		"ORDINARY=preserved",
	}
	fixture.providerNames = []string{"OTTO_API_KEY", "ACTIVE_KEY", "FALLBACK_KEY", "INACTIVE_KEY"}
	fixture.settings.AllowEnv = []string{"ACTIVE_KEY", "FALLBACK_KEY", "INACTIVE_KEY", "OTTO_API_KEY"}

	runtime := openSandboxRuntimeWithDependencies(context.Background(), fixture.options(), fixture.dependencies())
	t.Cleanup(func() { _ = runtime.Close() })

	joined := bytes.Join(byteSlices(runtime.Environment), []byte("\n"))
	for _, forbidden := range []string{"ACTIVE_KEY", "FALLBACK_KEY", "INACTIVE_KEY", "OTTO_API_KEY", "active-value", "fallback-value", "inactive-value", "otto-value"} {
		if bytes.Contains(joined, []byte(forbidden)) {
			t.Fatalf("command environment restored protected value %q: %q", forbidden, joined)
		}
	}
	if !bytes.Contains(joined, []byte("ORDINARY=preserved")) {
		t.Fatalf("ordinary environment missing: %q", joined)
	}
	for _, value := range []string{"active-value", "fallback-value", "inactive-value", "otto-value"} {
		if !containsString(runtime.RedactionValues, value) {
			t.Fatalf("redactions = %#v, want %q", runtime.RedactionValues, value)
		}
	}
}

func TestOpenSandboxCloseWaitsForActiveExecutionAndCleansState(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	fixture.driver.execute = func(ctx context.Context, _ sandbox.Request, _ sandbox.Streams) (sandbox.ExitStatus, error) {
		close(fixture.executionStarted)
		<-ctx.Done()
		close(fixture.executionCanceled)
		return sandbox.ExitStatus{Code: -1, Signaled: true, Signal: "killed"}, context.Canceled
	}
	fixture.driver.close = func() error {
		close(fixture.driverCloseStarted)
		fixture.executionCancel()
		<-fixture.executionCanceled
		return nil
	}

	runtime := openSandboxRuntimeWithDependencies(context.Background(), fixture.options(), fixture.dependencies())
	ctx, cancel := context.WithCancel(context.Background())
	fixture.executionCancel = cancel
	executeDone := make(chan error, 1)
	go func() {
		_, err := runtime.Executor.Execute(ctx, sandbox.Request{
			Argv: []string{fixture.shell, "-lc", "wait"}, Dir: fixture.workspace, Env: append([]string{}, runtime.Environment...),
		}, sandbox.Streams{Stdout: io.Discard, Stderr: io.Discard})
		executeDone <- err
	}()
	awaitSandboxRuntimeEvent(t, fixture.executionStarted, "execution start")

	closeResults := make(chan error, 2)
	go func() { closeResults <- runtime.Close() }()
	go func() { closeResults <- runtime.Close() }()
	awaitSandboxRuntimeEvent(t, fixture.driverCloseStarted, "driver close")
	awaitSandboxRuntimeEvent(t, fixture.executionCanceled, "execution cancellation")
	if err := <-executeDone; !errors.Is(err, context.Canceled) {
		t.Fatalf("Execute() error = %v, want cancellation", err)
	}
	for range 2 {
		if err := <-closeResults; err != nil {
			t.Fatalf("Close() error = %v", err)
		}
	}
	if fixture.driver.closeCalls.Load() != 1 {
		t.Fatalf("Driver close calls = %d, want 1", fixture.driver.closeCalls.Load())
	}
}

func TestOpenSandboxInvalidShellFailsClosedBeforeConstructingDriver(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	fixture.shell = filepath.Join(t.TempDir(), "missing-shell")

	runtime := openSandboxRuntimeWithDependencies(context.Background(), fixture.options(), fixture.dependencies())
	if fixture.seatbeltCalls.Load() != 0 || fixture.directCalls.Load() != 0 || fixture.executorCalls.Load() != 0 {
		t.Fatalf("invalid shell constructed resources: seatbelt/direct/executor = %d/%d/%d", fixture.seatbeltCalls.Load(), fixture.directCalls.Load(), fixture.executorCalls.Load())
	}
	want := app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonInvalidShell}
	if runtime.Executor != nil || runtime.Environment != nil || runtime.Info != want {
		t.Fatalf("runtime = %#v, want invalid-shell unavailable", runtime)
	}
}

func TestMergeSandboxRuntimeRedactionsBoundsGlobalRetentionByValueCount(t *testing.T) {
	groupA := sensitiveSandboxRuntimeEntries(300)
	groupB := sensitiveSandboxRuntimeEntries(300)
	for index := range groupB {
		groupB[index] = fmt.Sprintf("EXTRA_%03d_TOKEN=disjoint-runtime-sensitive-value-%03d", index, index)
	}
	merged, complete := mergeSandboxRuntimeRedactions(groupA, groupB)
	if complete {
		t.Fatal("mergeSandboxRuntimeRedactions() completeness = true, want false after global overflow")
	}
	if len(merged) != safetext.MaxSecretValues {
		t.Fatalf("merged values = %d, want %d", len(merged), safetext.MaxSecretValues)
	}
}

func TestMergeSandboxRuntimeRedactionsBoundsGlobalRetentionByByteBudget(t *testing.T) {
	chunk := strings.Repeat("z", safetext.MaxSecretBytes/2)
	groupA := []string{"a-" + chunk}
	groupB := []string{"b-" + chunk}
	merged, complete := mergeSandboxRuntimeRedactions(groupA, groupB)
	if complete {
		t.Fatal("mergeSandboxRuntimeRedactions() completeness = true, want false after byte overflow")
	}
	total := 0
	for _, value := range merged {
		total += len(value)
	}
	if total > safetext.MaxSecretBytes {
		t.Fatalf("merged bytes = %d, want <= %d", total, safetext.MaxSecretBytes)
	}
}

func TestSandboxRuntimeCloseReturnsOnlyFixedError(t *testing.T) {
	fixture := newSandboxRuntimeFixture(t)
	fixture.driver.closeErr = errors.New("raw private close state")
	runtime := openSandboxRuntimeWithDependencies(context.Background(), fixture.options(), fixture.dependencies())

	first := runtime.Close()
	second := runtime.Close()
	if first != errSandboxRuntimeClose || second != errSandboxRuntimeClose {
		t.Fatalf("Close() errors = %v / %v, want fixed sentinel", first, second)
	}
	if fixture.driver.closeCalls.Load() != 1 {
		t.Fatalf("Driver close calls = %d, want 1", fixture.driver.closeCalls.Load())
	}
}

type sandboxRuntimeFixture struct {
	t                  *testing.T
	platform           string
	workspace          string
	home               string
	shell              string
	private            sandbox.PrivateDirectories
	settings           sandbox.Settings
	hostEntries        []string
	providerNames      []string
	driver             *sandboxRuntimeFakeDriver
	openErr            error
	resolveErr         error
	openOptions        seatbelt.Options
	environmentOptions sandbox.EnvironmentOptions
	executorPolicy     sandbox.Policy
	classifyCalls      atomic.Int32
	resolveCalls       atomic.Int32
	seatbeltCalls      atomic.Int32
	directCalls        atomic.Int32
	executorCalls      atomic.Int32
	executionStarted   chan struct{}
	executionCanceled  chan struct{}
	driverCloseStarted chan struct{}
	executionCancel    context.CancelFunc
}

func newSandboxRuntimeFixture(t *testing.T) *sandboxRuntimeFixture {
	t.Helper()
	workspace := mustCanonicalDirectory(t, t.TempDir())
	home := mustCanonicalDirectory(t, t.TempDir())
	privateRoot := secureSandboxRuntimeDirectory(t, filepath.Join(t.TempDir(), "private"))
	private := sandbox.PrivateDirectories{
		Root:  privateRoot,
		Home:  secureSandboxRuntimeDirectory(t, filepath.Join(privateRoot, "home")),
		Temp:  secureSandboxRuntimeDirectory(t, filepath.Join(privateRoot, "tmp")),
		Cache: secureSandboxRuntimeDirectory(t, filepath.Join(privateRoot, "cache")),
	}
	fixture := &sandboxRuntimeFixture{
		t: t, platform: "darwin", workspace: workspace, home: home, shell: canonicalSandboxRuntimeShell(t), private: private,
		settings:         sandbox.Settings{Driver: sandbox.DriverAuto, Network: sandbox.NetworkAllow, ReadPaths: []string{}, AllowEnv: []string{}},
		hostEntries:      []string{"HOME=" + home, "PATH=/usr/bin:/bin", "ORDINARY=preserved"},
		providerNames:    []string{"OTTO_API_KEY"},
		executionStarted: make(chan struct{}), executionCanceled: make(chan struct{}), driverCloseStarted: make(chan struct{}),
	}
	fixture.driver = &sandboxRuntimeFakeDriver{
		id: "seatbelt", capabilities: sandbox.Capabilities{ReadConfinement: true, WriteConfinement: true, NetworkAllow: true, NetworkDeny: true, UnixSocketDeny: true},
		private: private,
	}
	return fixture
}

func (f *sandboxRuntimeFixture) options() sandboxOpenOptions {
	return sandboxOpenOptions{
		Settings: f.settings.Clone(), Workspace: f.workspace, Shell: f.shell, Home: f.home,
		HostEntries: append([]string(nil), f.hostEntries...), ProviderNames: append([]string(nil), f.providerNames...),
	}
}

func (f *sandboxRuntimeFixture) dependencies() sandboxRuntimeDependencies {
	return sandboxRuntimeDependencies{
		platform: f.platform,
		openSeatbelt: func(_ context.Context, options seatbelt.Options) (sandboxSeatbeltDriver, error) {
			f.seatbeltCalls.Add(1)
			f.openOptions = options
			if f.openErr != nil {
				return nil, f.openErr
			}
			return f.driver, nil
		},
		newDirect: func() sandbox.Driver {
			f.directCalls.Add(1)
			return &sandboxRuntimeFakeDriver{id: "direct", capabilities: sandbox.Capabilities{NetworkAllow: true}}
		},
		classifyEnvironment: func(options sandbox.EnvironmentOptions) (sandbox.EnvironmentSnapshot, error) {
			f.classifyCalls.Add(1)
			return sandbox.ResolveEnvironment(options)
		},
		resolveEnvironment: func(options sandbox.EnvironmentOptions) (sandbox.EnvironmentSnapshot, error) {
			f.resolveCalls.Add(1)
			f.environmentOptions = options
			if f.resolveErr != nil {
				return sandbox.EnvironmentSnapshot{}, f.resolveErr
			}
			return sandbox.ResolveEnvironment(options)
		},
		newExecutor: func(driver sandbox.Driver, policy sandbox.Policy, workspace string) (sandboxExecutor, error) {
			f.executorCalls.Add(1)
			f.executorPolicy = policy
			return sandbox.NewExecutor(driver, policy, workspace)
		},
	}
}

type sandboxRuntimeFakeDriver struct {
	id           sandbox.DriverID
	capabilities sandbox.Capabilities
	private      sandbox.PrivateDirectories
	execute      func(context.Context, sandbox.Request, sandbox.Streams) (sandbox.ExitStatus, error)
	close        func() error
	closeErr     error
	closeCalls   atomic.Int32
}

func (d *sandboxRuntimeFakeDriver) ID() sandbox.DriverID                           { return d.id }
func (d *sandboxRuntimeFakeDriver) Capabilities() sandbox.Capabilities             { return d.capabilities }
func (d *sandboxRuntimeFakeDriver) PrivateDirectories() sandbox.PrivateDirectories { return d.private }
func (d *sandboxRuntimeFakeDriver) Execute(ctx context.Context, request sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
	if d.execute != nil {
		return d.execute(ctx, request, streams)
	}
	return sandbox.ExitStatus{Code: 0}, nil
}
func (d *sandboxRuntimeFakeDriver) Close() error {
	d.closeCalls.Add(1)
	if d.close != nil {
		return d.close()
	}
	return d.closeErr
}

func secureSandboxRuntimeDirectory(t *testing.T, path string) string {
	t.Helper()
	if err := os.Mkdir(path, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o700); err != nil {
		t.Fatal(err)
	}
	canonical, err := filepath.EvalSymlinks(path)
	if err != nil {
		t.Fatal(err)
	}
	return canonical
}

func canonicalSandboxRuntimeShell(t *testing.T) string {
	t.Helper()
	path, err := filepath.EvalSymlinks("/bin/sh")
	if err != nil {
		t.Fatal(err)
	}
	return path
}

func sensitiveSandboxRuntimeEntries(count int) []string {
	entries := make([]string, 0, count)
	for index := range count {
		entries = append(entries, fmt.Sprintf("VALUE_%03d_TOKEN=runtime-sensitive-value-%03d", index, index))
	}
	return entries
}

func byteSlices(values []string) [][]byte {
	result := make([][]byte, len(values))
	for index, value := range values {
		result[index] = []byte(value)
	}
	return result
}

func containsString(values []string, want string) bool {
	for _, value := range values {
		if value == want {
			return true
		}
	}
	return false
}

func awaitSandboxRuntimeEvent(t *testing.T, event <-chan struct{}, name string) {
	t.Helper()
	select {
	case <-event:
	case <-time.After(2 * time.Second):
		t.Fatalf("timed out waiting for %s", name)
	}
}

var _ sandboxSeatbeltDriver = (*sandboxRuntimeFakeDriver)(nil)
var _ sandbox.Driver = (*sandboxRuntimeFakeDriver)(nil)
