//go:build darwin

package seatbelt

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/sandbox/internal/nativeprocess"
	"github.com/baiyuqing/otto/internal/sandbox/sandboxtest"
	"golang.org/x/sys/unix"
)

const driverTestDeadlockDeadline = 15 * time.Second

func TestOpenUsesFixedSandboxExecAndPrivateProfile(t *testing.T) {
	workspace := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "workspace space ü 'quote;()[]"))
	home := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "resolved-home"))
	cache := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "user-cache"))
	dependencies := defaultDriverDependencies()
	inspectedPath := ""
	productionInspect := dependencies.inspectSandboxExec
	dependencies.inspectSandboxExec = func(path string) (sandboxExecIdentity, error) {
		inspectedPath = path
		return productionInspect(path)
	}
	dependencies.userCacheDir = func() (string, error) { return cache, nil }
	productionProbe := dependencies.runProbe
	var probePaths []string
	dependencies.runProbe = func(ctx context.Context, manager *nativeprocess.Manager, probe selfTestProbe) (selfTestProbeResult, error) {
		probePaths = append(probePaths, probe.spec.Path)
		return productionProbe(ctx, manager, probe)
	}

	driver, err := openWithDependencies(context.Background(), Options{
		Workspace: workspace,
		Shell:     "/bin/sh",
		Home:      home,
		HostEntries: []string{
			"PATH=/usr/bin:/bin",
			"OTTO_SELF_TEST_MUST_NOT_INHERIT=host-only-value",
		},
		Network: sandbox.NetworkDeny,
	}, dependencies)
	if err != nil {
		t.Fatalf("Open() error = %v", err)
	}
	t.Cleanup(func() {
		if err := driver.Close(); err != nil {
			t.Errorf("Driver.Close() error = %v", err)
		}
	})

	if inspectedPath != sandboxExecPath || len(probePaths) < 5 {
		t.Fatalf("launcher inspection/probes = (%q, %v), want only fixed sandbox-exec", inspectedPath, probePaths)
	}
	for _, path := range probePaths {
		if path != sandboxExecPath {
			t.Fatalf("self-test launcher = %q, want %q", path, sandboxExecPath)
		}
	}
	if driver.ID() != "seatbelt" || driver.ID() != ID {
		t.Fatalf("ID() = %q, want seatbelt", driver.ID())
	}
	private := driver.PrivateDirectories()
	if private.Root == "" || pathWithin(workspace, private.Root) || pathWithin(private.Root, workspace) || filepath.Dir(private.Root) != cache {
		t.Fatal("private state is empty, outside the selected cache, or overlaps the workspace")
	}
	if driver.profilePath != filepath.Join(private.Root, "profiles", "profile.sb") {
		t.Fatal("Driver did not bind the private profile path")
	}
	profileInfo, err := os.Lstat(driver.profilePath)
	if err != nil || !profileInfo.Mode().IsRegular() || profileInfo.Mode().Perm() != 0o600 {
		t.Fatalf("private profile info = (%v, %v)", profileInfo, err)
	}
	if data, err := os.ReadFile(driver.profilePath); err != nil || !bytes.Equal(data, driver.profile) {
		t.Fatalf("private profile does not match the bound production profile: bytes=%d error=%v", len(data), err)
	}

	pathTrap := canonicalDriverTestDirectory(t, filepath.Join(workspace, "path-trap"))
	trapMarker := filepath.Join(workspace, "sandbox-exec-path-trap-ran")
	writeDriverTestExecutable(t, filepath.Join(pathTrap, "sandbox-exec"), "printf trap > "+shellQuoteForTest(trapMarker))
	status, err := driver.Execute(context.Background(), sandbox.Request{
		Argv: []string{"/bin/sh", "-c", "exit 0"},
		Dir:  workspace,
		Env:  []string{"PATH=" + pathTrap, "LC_ALL=C"},
	}, sandbox.Streams{Stdout: io.Discard, Stderr: io.Discard})
	if err != nil || status.Code != 0 || status.Signaled {
		t.Fatalf("Execute() = (%+v, %v)", status, err)
	}
	if _, err := os.Lstat(trapMarker); !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("request PATH sandbox-exec trap ran: %v", err)
	}
}

func TestOpenRejectsMissingOrNonExecutableSandboxExec(t *testing.T) {
	workspace := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "workspace"))
	home := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "home"))
	cache := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "cache"))
	production := defaultDriverDependencies()
	valid, err := production.inspectSandboxExec(sandboxExecPath)
	if err != nil {
		t.Fatalf("inspect production sandbox-exec: %v", err)
	}

	tests := []struct {
		name     string
		identity sandboxExecIdentity
		err      error
	}{
		{name: "missing", err: fs.ErrNotExist},
		{name: "non executable", identity: sandboxExecIdentity{canonical: sandboxExecPath, mode: 0o600, uid: 0}},
		{name: "directory", identity: sandboxExecIdentity{canonical: sandboxExecPath, mode: fs.ModeDir | 0o755, uid: 0}},
		{name: "symlink", identity: sandboxExecIdentity{canonical: sandboxExecPath, mode: fs.ModeSymlink | 0o777, uid: 0}},
		{name: "non root owner", identity: sandboxExecIdentity{canonical: sandboxExecPath, mode: 0o755, uid: os.Geteuid()}},
		{name: "non canonical spelling", identity: sandboxExecIdentity{canonical: "/private/usr/bin/sandbox-exec", mode: 0o755, uid: 0}},
		{name: "special executable", identity: sandboxExecIdentity{canonical: sandboxExecPath, mode: fs.ModeDevice | 0o755, uid: 0}},
	}
	if os.Geteuid() == 0 {
		tests[4].identity.uid = 501
	}
	_ = valid

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			dependencies := defaultDriverDependencies()
			dependencies.inspectSandboxExec = func(path string) (sandboxExecIdentity, error) {
				if path != sandboxExecPath {
					t.Fatalf("inspected launcher %q", path)
				}
				return test.identity, test.err
			}
			dependencies.userCacheDir = func() (string, error) { return cache, nil }
			probeCalled := false
			dependencies.runProbe = func(context.Context, *nativeprocess.Manager, selfTestProbe) (selfTestProbeResult, error) {
				probeCalled = true
				return selfTestProbeResult{}, nil
			}

			driver, openErr := openWithDependencies(context.Background(), Options{
				Workspace: workspace,
				Shell:     "/bin/sh",
				Home:      home,
				Network:   sandbox.NetworkDeny,
			}, dependencies)
			assertUnavailableReason(t, driver, openErr, sandbox.ReasonSeatbeltMissing)
			if probeCalled {
				t.Fatal("self-test ran after launcher identity rejection")
			}
			assertNoStateLeaves(t, cache)
		})
	}
}

func TestOpenRejectsInvalidConfiguredShell(t *testing.T) {
	workspace := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "workspace"))
	home := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "home"))
	cache := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "cache"))
	nonExecutable := filepath.Join(t.TempDir(), "non-executable-shell")
	if err := os.WriteFile(nonExecutable, []byte("not a shell"), 0o600); err != nil {
		t.Fatal(err)
	}
	for _, shell := range []string{filepath.Join(t.TempDir(), "missing-shell"), nonExecutable, workspace} {
		t.Run(filepath.Base(shell), func(t *testing.T) {
			dependencies := defaultDriverDependencies()
			dependencies.userCacheDir = func() (string, error) { return cache, nil }
			driver, err := openWithDependencies(context.Background(), Options{
				Workspace: workspace,
				Shell:     shell,
				Home:      home,
				Network:   sandbox.NetworkDeny,
			}, dependencies)
			assertUnavailableReason(t, driver, err, sandbox.ReasonInvalidShell)
			assertNoStateLeaves(t, cache)
		})
	}
}

func TestOpenRejectsProfileParseFailure(t *testing.T) {
	workspace, home, cache, dependencies := driverOpenFixture(t)
	privateDiagnostic := filepath.Join(cache, "private-profile-diagnostic")
	dependencies.runProbe = func(_ context.Context, _ *nativeprocess.Manager, probe selfTestProbe) (selfTestProbeResult, error) {
		if probe.kind != selfTestProfileStart {
			t.Fatalf("first probe kind = %v", probe.kind)
		}
		_, _ = io.WriteString(probe.spec.Stderr, "sandbox-exec: unbound variable: private at "+privateDiagnostic+", line 2\n\nBacktrace: "+privateDiagnostic+"\n")
		return selfTestProbeResult{result: nativeprocess.Result{Code: 65}, leaderReaped: true}, nil
	}

	driver, err := openWithDependencies(context.Background(), driverOptions(workspace, home, sandbox.NetworkDeny), dependencies)
	assertUnavailableReason(t, driver, err, sandbox.ReasonSelfTestFailed)
	assertSafeDriverError(t, err, workspace, home, cache, privateDiagnostic, "profile.sb")
	assertNoStateLeaves(t, cache)
}

func TestOpenRejectsAllowedProbeFailure(t *testing.T) {
	workspace, home, cache, dependencies := driverOpenFixture(t)
	productionProbe := dependencies.runProbe
	dependencies.runProbe = func(ctx context.Context, manager *nativeprocess.Manager, probe selfTestProbe) (selfTestProbeResult, error) {
		if probe.kind == selfTestAllowedRead {
			return selfTestProbeResult{result: nativeprocess.Result{Code: 1}, leaderReaped: true}, nil
		}
		return productionProbe(ctx, manager, probe)
	}

	driver, err := openWithDependencies(context.Background(), driverOptions(workspace, home, sandbox.NetworkDeny), dependencies)
	assertUnavailableReason(t, driver, err, sandbox.ReasonSelfTestFailed)
	assertSafeDriverError(t, err, workspace, home, cache)
	assertNoStateLeaves(t, cache)
}

func TestOpenRejectsDeniedReadOrWriteProbeSuccess(t *testing.T) {
	for _, kind := range []selfTestProbeKind{selfTestDeniedRead, selfTestDeniedWrite} {
		t.Run(kind.String(), func(t *testing.T) {
			workspace, home, cache, dependencies := driverOpenFixture(t)
			productionProbe := dependencies.runProbe
			dependencies.runProbe = func(ctx context.Context, manager *nativeprocess.Manager, probe selfTestProbe) (selfTestProbeResult, error) {
				if probe.kind == kind {
					return selfTestProbeResult{result: nativeprocess.Result{Code: 0}, leaderReaped: true}, nil
				}
				return productionProbe(ctx, manager, probe)
			}

			driver, err := openWithDependencies(context.Background(), driverOptions(workspace, home, sandbox.NetworkDeny), dependencies)
			assertUnavailableReason(t, driver, err, sandbox.ReasonSelfTestFailed)
			assertSafeDriverError(t, err, workspace, home, cache)
			assertNoStateLeaves(t, cache)
		})
	}
}

func TestOpenRejectsUnreapedOrOversizedSelfTestProbe(t *testing.T) {
	for _, test := range []struct {
		name string
		run  func(selfTestProbe) selfTestProbeResult
	}{
		{
			name: "leader not reaped",
			run: func(selfTestProbe) selfTestProbeResult {
				return selfTestProbeResult{result: nativeprocess.Result{Code: 0}, leaderReaped: false}
			},
		},
		{
			name: "output exceeds cap",
			run: func(probe selfTestProbe) selfTestProbeResult {
				_, _ = probe.spec.Stdout.Write(bytes.Repeat([]byte("x"), selfTestOutputLimit+1))
				return selfTestProbeResult{result: nativeprocess.Result{Code: 0}, leaderReaped: true}
			},
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			workspace, home, cache, dependencies := driverOpenFixture(t)
			dependencies.runProbe = func(_ context.Context, _ *nativeprocess.Manager, probe selfTestProbe) (selfTestProbeResult, error) {
				return test.run(probe), nil
			}
			driver, err := openWithDependencies(context.Background(), driverOptions(workspace, home, sandbox.NetworkDeny), dependencies)
			assertUnavailableReason(t, driver, err, sandbox.ReasonSelfTestFailed)
			assertNoStateLeaves(t, cache)
		})
	}
}

func TestOpenSelfTestUsesOnlyMinimalPrivateEnvironment(t *testing.T) {
	workspace, home, _, dependencies := driverOpenFixture(t)
	productionProbe := dependencies.runProbe
	seen := 0
	dependencies.runProbe = func(ctx context.Context, manager *nativeprocess.Manager, probe selfTestProbe) (selfTestProbeResult, error) {
		seen++
		want := []string{
			"PATH=/usr/bin:/bin",
			"HOME=" + probe.directories.Home,
			"TMPDIR=" + probe.directories.Temp,
			"LC_ALL=C",
		}
		if !slices.Equal(probe.spec.Environment, want) {
			t.Fatalf("self-test environment = %q, want minimal fixed %q", probe.spec.Environment, want)
		}
		for _, entry := range probe.spec.Environment {
			if strings.Contains(entry, "HOST_ONLY") {
				t.Fatal("self-test inherited HostEntries")
			}
		}
		if probe.kind == selfTestAllowedWrite {
			argv := strings.Join(probe.spec.Args, "\x00")
			if !strings.Contains(argv, workspace) || !strings.Contains(argv, probe.directories.Temp) {
				t.Fatal("allowed-write self-test did not cover both workspace and private temporary storage")
			}
		}
		return productionProbe(ctx, manager, probe)
	}
	options := driverOptions(workspace, home, sandbox.NetworkDeny)
	options.HostEntries = []string{"PATH=/host/trap", "HOST_ONLY=must-not-reach-probe"}
	driver, err := openWithDependencies(context.Background(), options, dependencies)
	if err != nil {
		t.Fatalf("Open() error = %v", err)
	}
	if seen < 5 {
		t.Fatalf("self-test subprocess count = %d, want enforcement probes", seen)
	}
	if err := driver.Close(); err != nil {
		t.Fatalf("Close() error = %v", err)
	}
}

func TestOpenCancellationCleansState(t *testing.T) {
	workspace, home, cache, dependencies := driverOpenFixture(t)
	entered := make(chan struct{})
	dependencies.runProbe = func(ctx context.Context, _ *nativeprocess.Manager, _ selfTestProbe) (selfTestProbeResult, error) {
		close(entered)
		<-ctx.Done()
		return selfTestProbeResult{leaderReaped: true}, ctx.Err()
	}
	ctx, cancel := context.WithCancel(context.Background())
	result := make(chan struct {
		driver *Driver
		err    error
	}, 1)
	go func() {
		driver, err := openWithDependencies(ctx, driverOptions(workspace, home, sandbox.NetworkDeny), dependencies)
		result <- struct {
			driver *Driver
			err    error
		}{driver: driver, err: err}
	}()
	awaitDriverSignal(t, entered, "self-test entry")
	cancel()
	completed := awaitDriverOpen(t, result, "cancelled Open")
	if completed.driver != nil || !errors.Is(completed.err, context.Canceled) || completed.err.Error() != context.Canceled.Error() {
		t.Fatalf("cancelled Open() returned driver=%t, error=%v", completed.driver != nil, completed.err)
	}
	assertNoStateLeaves(t, cache)
}

func TestOpenCancellationAfterFinalProbeCleansState(t *testing.T) {
	workspace, home, cache, dependencies := driverOpenFixture(t)
	ctx, cancel := context.WithCancel(context.Background())
	productionProbe := dependencies.runProbe
	dependencies.runProbe = func(probeCtx context.Context, manager *nativeprocess.Manager, probe selfTestProbe) (selfTestProbeResult, error) {
		result, err := productionProbe(probeCtx, manager, probe)
		if probe.kind == selfTestDeniedWrite {
			cancel()
		}
		return result, err
	}
	driver, err := openWithDependencies(ctx, driverOptions(workspace, home, sandbox.NetworkDeny), dependencies)
	if driver != nil || !errors.Is(err, context.Canceled) {
		if driver != nil {
			_ = driver.Close()
		}
		t.Fatalf("Open() after final-probe cancellation returned driver=%t, error=%v", driver != nil, err)
	}
	assertNoStateLeaves(t, cache)
}

func TestDriverCloseCleansStateAndIsIdempotent(t *testing.T) {
	driver := openDriverForTest(t, sandbox.NetworkDeny)
	root := driver.PrivateDirectories().Root
	if _, err := os.Lstat(root); err != nil {
		t.Fatal(err)
	}

	const callers = 16
	start := make(chan struct{})
	results := make(chan error, callers)
	for range callers {
		go func() {
			<-start
			results <- driver.Close()
		}()
	}
	close(start)
	for range callers {
		if err := awaitDriverError(t, results, "concurrent Driver.Close"); err != nil {
			t.Fatalf("Driver.Close() error = %v", err)
		}
	}
	if _, err := os.Lstat(root); !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("private root remains after Close: %v", err)
	}
	if err := driver.Close(); err != nil {
		t.Fatalf("idempotent Close() error = %v", err)
	}
	_, err := driver.Execute(context.Background(), basicDriverRequest(driver, []string{"/bin/sh", "-c", "exit 0"}), driverDiscardStreams())
	if !errors.Is(err, sandbox.ErrClosed) || err.Error() != sandbox.ErrClosed.Error() {
		t.Fatalf("Execute() after Close error = %v", err)
	}
}

func TestDriverInfrastructureStderrFilterIsBoundedAndFailClosed(t *testing.T) {
	t.Run("future infrastructure diagnostic is suppressed", func(t *testing.T) {
		var destination bytes.Buffer
		filter := newInfrastructureStderrFilter(&destination)
		diagnostic := "sandbox-exec: future refusal at /private/profile/path\nBacktrace: /private/profile/path\n"
		for _, part := range []string{diagnostic[:8], diagnostic[8:31], diagnostic[31:]} {
			if _, err := filter.Write([]byte(part)); err != nil {
				t.Fatal(err)
			}
		}
		if err := filter.finish(); err != nil {
			t.Fatal(err)
		}
		if !filter.infrastructureFailure() || destination.Len() != 0 {
			t.Fatalf("future infrastructure diagnostic was exposed: %q", destination.String())
		}
	})

	t.Run("exec policy diagnostic is ordinary child stderr", func(t *testing.T) {
		var destination bytes.Buffer
		filter := newInfrastructureStderrFilter(&destination)
		diagnostic := "sandbox-exec: execvp() of '/workspace/denied' failed: Operation not permitted\n"
		if _, err := filter.Write([]byte(diagnostic)); err != nil {
			t.Fatal(err)
		}
		if err := filter.finish(); err != nil {
			t.Fatal(err)
		}
		if filter.infrastructureFailure() || destination.String() != diagnostic {
			t.Fatalf("ordinary exec policy diagnostic changed: %q", destination.String())
		}
	})

	t.Run("only first line is classified", func(t *testing.T) {
		var destination bytes.Buffer
		filter := newInfrastructureStderrFilter(&destination)
		ordinary := "ordinary first line\nsandbox-exec: sandbox_apply: child text\n"
		if _, err := filter.Write([]byte(ordinary)); err != nil {
			t.Fatal(err)
		}
		if err := filter.finish(); err != nil {
			t.Fatal(err)
		}
		if filter.infrastructureFailure() || destination.String() != ordinary {
			t.Fatalf("later child stderr signature changed: %q", destination.String())
		}
	})

	t.Run("first-line buffering never exceeds four KiB", func(t *testing.T) {
		var filter *infrastructureStderrFilter
		maxPending := 0
		var destination bytes.Buffer
		writer := driverWriterFunc(func(data []byte) (int, error) {
			if pending := len(filter.pending); pending > maxPending {
				maxPending = pending
			}
			return destination.Write(data)
		})
		filter = newInfrastructureStderrFilter(writer)
		ordinary := append([]byte("ordinary:"), bytes.Repeat([]byte("x"), stderrDecisionLimit*4)...)
		if _, err := filter.Write(ordinary); err != nil {
			t.Fatal(err)
		}
		if err := filter.finish(); err != nil {
			t.Fatal(err)
		}
		if maxPending > stderrDecisionLimit || !bytes.Equal(destination.Bytes(), ordinary) {
			t.Fatalf("peak first-line buffer = %d, output bytes = %d", maxPending, destination.Len())
		}
	})
}

func TestDriverSuppressesSandboxExecInfrastructureDiagnostics(t *testing.T) {
	driver := openDriverForTest(t, sandbox.NetworkDeny)
	t.Cleanup(func() { _ = driver.Close() })
	private := driver.PrivateDirectories()
	malformed := []byte("(version 1)\n(otto-private-invalid-form)\n")
	if err := driver.state.writeProfile(malformed); err != nil {
		t.Fatal(err)
	}
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	status, err := driver.Execute(context.Background(), basicDriverRequest(driver, []string{"/bin/sh", "-c", "printf child-should-not-start"}), sandbox.Streams{
		Stdout: &stdout,
		Stderr: &stderr,
	})
	assertUnavailableRuntime(t, err)
	if status != (sandbox.ExitStatus{}) || stdout.Len() != 0 || stderr.Len() != 0 {
		t.Fatalf("infrastructure failure = (%+v, stdout %q, stderr %q)", status, stdout.String(), stderr.String())
	}
	assertSafeDriverError(t, err, private.Root, driver.profilePath, string(malformed), "unbound variable", "Backtrace", "line 2")
}

func TestDriverMissingProfileFailsClosedWithoutExposingItsPath(t *testing.T) {
	driver := openDriverForTest(t, sandbox.NetworkDeny)
	t.Cleanup(func() { _ = driver.Close() })
	private := driver.PrivateDirectories()
	if err := os.Remove(driver.profilePath); err != nil {
		t.Fatal(err)
	}
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	status, err := driver.Execute(context.Background(), basicDriverRequest(driver, []string{"/bin/sh", "-c", "printf child-should-not-start"}), sandbox.Streams{
		Stdout: &stdout,
		Stderr: &stderr,
	})
	assertUnavailableRuntime(t, err)
	if status != (sandbox.ExitStatus{}) || stdout.Len() != 0 || stderr.Len() != 0 {
		t.Fatalf("missing-profile failure = (%+v, stdout %q, stderr %q)", status, stdout.String(), stderr.String())
	}
	assertSafeDriverError(t, err, private.Root, driver.profilePath, "No such file", "sandbox-exec:")
}

func TestDriverInfrastructureFailurePoisonsLaterCalls(t *testing.T) {
	driver := openDriverForTest(t, sandbox.NetworkDeny)
	t.Cleanup(func() { _ = driver.Close() })
	validProfile := slices.Clone(driver.profile)
	if err := driver.state.writeProfile([]byte("(version 1)\n(otto-invalid-form)\n")); err != nil {
		t.Fatal(err)
	}
	_, firstErr := driver.Execute(context.Background(), basicDriverRequest(driver, []string{"/bin/sh", "-c", "exit 0"}), driverDiscardStreams())
	assertUnavailableRuntime(t, firstErr)
	if err := driver.state.writeProfile(validProfile); err != nil {
		t.Fatal(err)
	}
	marker := filepath.Join(driver.workspace, "poisoned-driver-must-not-start")
	_, secondErr := driver.Execute(context.Background(), basicDriverRequest(driver, []string{"/bin/sh", "-c", `printf started > "$1"`, "child", marker}), driverDiscardStreams())
	assertUnavailableRuntime(t, secondErr)
	if firstErr.Error() != secondErr.Error() {
		t.Fatalf("poison errors differ: %q != %q", firstErr, secondErr)
	}
	if _, err := os.Lstat(marker); !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("poisoned Driver started a later child: %v", err)
	}
}

func TestDriverOrdinaryPolicyExitAndChildStderrRemainUnchanged(t *testing.T) {
	driver := openDriverForTest(t, sandbox.NetworkDeny)
	t.Cleanup(func() { _ = driver.Close() })
	outside := filepath.Join(filepath.Dir(driver.workspace), "ordinary-policy-denied")
	if err := os.WriteFile(outside, []byte("private"), 0o600); err != nil {
		t.Fatal(err)
	}
	var stderr bytes.Buffer
	status, err := driver.Execute(context.Background(), basicDriverRequest(driver, []string{
		"/bin/sh", "-c", `printf 'ordinary child stderr\n' >&2; /bin/cat "$1" >/dev/null`, "child", outside,
	}), sandbox.Streams{Stdout: io.Discard, Stderr: &stderr})
	if err != nil || status.Code == 0 || status.Signaled {
		t.Fatalf("ordinary policy Execute() = (%+v, %v)", status, err)
	}
	if !strings.HasPrefix(stderr.String(), "ordinary child stderr\n") {
		t.Fatalf("ordinary child stderr was changed: %q", stderr.String())
	}
}

func TestDriverCancellationReapsSameGroupDescendant(t *testing.T) {
	driver := openDriverForTest(t, sandbox.NetworkDeny)
	t.Cleanup(func() { _ = driver.Close() })
	fifo := filepath.Join(driver.workspace, "cancel-blocked-fifo")
	if err := unix.Mkfifo(fifo, 0o600); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	stdout := newDriverLineWriter()
	result := make(chan driverExecution, 1)
	go func() {
		request := basicDriverRequest(driver, []string{
			"/bin/sh", "-c", `/bin/sh -c 'echo "$$"; exec /bin/cat "$1"' descendant "$1" & wait`, "parent", fifo,
		})
		status, err := driver.Execute(ctx, request, sandbox.Streams{Stdout: stdout, Stderr: io.Discard})
		result <- driverExecution{status: status, err: err}
	}()
	pid, err := strconv.Atoi(strings.TrimSpace(awaitDriverLine(t, stdout.lines, "cancel descendant PID")))
	if err != nil || pid <= 0 {
		t.Fatalf("invalid descendant PID: %d, %v", pid, err)
	}
	cancel()
	completed := awaitDriverExecution(t, result, "cancelled Driver.Execute")
	if !errors.Is(completed.err, context.Canceled) || !completed.status.Signaled || completed.status.Code != -1 {
		t.Fatalf("cancelled Execute() = (%+v, %v)", completed.status, completed.err)
	}
	if err := unix.Kill(pid, 0); !errors.Is(err, unix.ESRCH) {
		if err == nil {
			_ = unix.Kill(pid, unix.SIGKILL)
		}
		t.Fatalf("cancelled descendant signal-0 = %v, want ESRCH", err)
	}
}

func TestDriverCapabilitiesBindExactlyOneNetworkMode(t *testing.T) {
	for _, test := range []struct {
		mode sandbox.NetworkMode
		want sandbox.Capabilities
	}{
		{
			mode: sandbox.NetworkAllow,
			want: sandbox.Capabilities{ReadConfinement: true, WriteConfinement: true, NetworkAllow: true, UnixSocketDeny: true},
		},
		{
			mode: sandbox.NetworkDeny,
			want: sandbox.Capabilities{ReadConfinement: true, WriteConfinement: true, NetworkDeny: true, UnixSocketDeny: true},
		},
	} {
		t.Run(fmt.Sprint(test.mode), func(t *testing.T) {
			driver := openDriverForTest(t, test.mode)
			defer driver.Close()
			if got := driver.Capabilities(); got != test.want {
				t.Fatalf("Capabilities() = %+v, want %+v", got, test.want)
			}
			if got := driver.PrivateDirectories(); got != driver.state.directories {
				t.Fatalf("PrivateDirectories() = %+v, want bound state", got)
			}
		})
	}
}

func TestDriverRejectsDirectoryOutsideBoundWorkspace(t *testing.T) {
	driver := openDriverForTest(t, sandbox.NetworkDeny)
	t.Cleanup(func() { _ = driver.Close() })
	outside := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "outside"))
	marker := filepath.Join(outside, "must-not-start")
	request := basicDriverRequest(driver, []string{"/bin/sh", "-c", `printf started > "$1"`, "child", marker})
	request.Dir = outside
	_, err := driver.Execute(context.Background(), request, driverDiscardStreams())
	if !errors.Is(err, sandbox.ErrInvalidRequest) || err.Error() != sandbox.ErrInvalidRequest.Error() {
		t.Fatalf("outside Dir error = %v, want fixed ErrInvalidRequest", err)
	}
	if _, err := os.Lstat(marker); !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("invalid request started: %v", err)
	}
}

func TestSeatbeltDriverContract(t *testing.T) {
	executable := driverTestExecutable(t)
	for _, mode := range []sandbox.NetworkMode{sandbox.NetworkAllow, sandbox.NetworkDeny} {
		mode := mode
		t.Run(driverNetworkName(mode), func(t *testing.T) {
			sandboxtest.RunDriverContract(t, sandboxtest.Case{
				NewDriver: func(tb testing.TB, fixture sandboxtest.Fixture) sandbox.Driver {
					home := canonicalDriverTestDirectory(tb, filepath.Join(tb.TempDir(), "host-home"))
					cache := canonicalDriverTestDirectory(tb, filepath.Join(tb.TempDir(), "user-cache"))
					dependencies := defaultDriverDependencies()
					dependencies.userCacheDir = func() (string, error) { return cache, nil }
					driver, err := openWithDependencies(context.Background(), Options{
						Workspace:   fixture.Workspace,
						Shell:       "/bin/sh",
						Home:        home,
						HostEntries: slices.Clone(fixture.Environment),
						ReadPaths:   []string{fixture.AllowedRead, executable},
						Network:     mode,
					}, dependencies)
					if err != nil {
						tb.Fatalf("seatbelt Open() error = %v", err)
					}
					return driver
				},
				Request: func(t testing.TB, fixture sandboxtest.Fixture, argv []string) sandbox.Request {
					t.Helper()
					environment := slices.Clone(fixture.Environment)
					environment = append(environment, "OTTO_SEATBELT_HELPER_PROCESS=1")
					return sandbox.Request{Argv: slices.Clone(argv), Dir: fixture.Workspace, Env: environment}
				},
				ShellCommand: func(t testing.TB, script string) []string {
					t.Helper()
					return []string{"/bin/sh", "-c", script}
				},
				TCPClient: func(t testing.TB, address string) []string {
					t.Helper()
					return driverHelperCommand(executable, "dial", "tcp4", address)
				},
				UnixClient: func(t testing.TB, address string) []string {
					t.Helper()
					return driverHelperCommand(executable, "dial", "unix", address)
				},
			})
		})
	}
}

func TestDriverWorkspacePrivateAndExternalFilesystemPolicy(t *testing.T) {
	base := t.TempDir()
	workspace := canonicalDriverTestDirectory(t, filepath.Join(base, "workspace"))
	hostHome := canonicalDriverTestDirectory(t, filepath.Join(base, "host-home"))
	hostCache := canonicalDriverTestDirectory(t, filepath.Join(base, "host-cache"))
	cacheBase := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "user-cache"))
	hostSecret := filepath.Join(hostHome, "secret")
	cacheSecret := filepath.Join(hostCache, "secret")
	for _, path := range []string{hostSecret, cacheSecret} {
		if err := os.WriteFile(path, []byte("must-not-read"), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	dependencies := defaultDriverDependencies()
	dependencies.userCacheDir = func() (string, error) { return cacheBase, nil }
	driver, err := openWithDependencies(context.Background(), Options{
		Workspace: workspace,
		Shell:     "/bin/sh",
		Home:      hostHome,
		Network:   sandbox.NetworkDeny,
	}, dependencies)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = driver.Close() })
	private := driver.PrivateDirectories()
	workspaceFile := filepath.Join(workspace, "workspace-file")
	homeFile := filepath.Join(private.Home, "home-file")
	tempFile := filepath.Join(private.Temp, "temp-file")
	cacheFile := filepath.Join(private.Cache, "cache-file")

	status, err := driver.Execute(context.Background(), basicDriverRequest(driver, []string{
		"/bin/sh", "-c", `
set -e
printf workspace > "$1"
printf home > "$2"
printf temp > "$3"
printf cache > "$4"
/bin/cat "$1" "$2" "$3" "$4" >/dev/null
`, "child", workspaceFile, homeFile, tempFile, cacheFile,
	}), driverDiscardStreams())
	if err != nil || status.Code != 0 {
		t.Fatalf("allowed writes Execute() = (%+v, %v)", status, err)
	}
	for path, want := range map[string]string{workspaceFile: "workspace", homeFile: "home", tempFile: "temp", cacheFile: "cache"} {
		data, readErr := os.ReadFile(path)
		if readErr != nil || string(data) != want {
			t.Fatalf("allowed file %q = (%q, %v)", filepath.Base(path), data, readErr)
		}
	}

	for name, path := range map[string]string{
		"host home read":  hostSecret,
		"host cache read": cacheSecret,
		"profile read":    driver.profilePath,
	} {
		t.Run(name, func(t *testing.T) {
			status, err := driver.Execute(context.Background(), basicDriverRequest(driver, []string{"/bin/cat", path}), driverDiscardStreams())
			if err != nil || status.Code == 0 {
				t.Fatalf("denied read Execute() = (%+v, %v)", status, err)
			}
		})
	}
	for name, path := range map[string]string{
		"host home write":  filepath.Join(hostHome, "must-not-write"),
		"host cache write": filepath.Join(hostCache, "must-not-write"),
		"state root write": filepath.Join(private.Root, "must-not-write"),
		"profiles write":   filepath.Join(filepath.Dir(driver.profilePath), "must-not-write"),
	} {
		t.Run(name, func(t *testing.T) {
			status, err := driver.Execute(context.Background(), basicDriverRequest(driver, []string{"/bin/sh", "-c", `printf denied > "$1"`, "child", path}), driverDiscardStreams())
			if err != nil || status.Code == 0 {
				t.Fatalf("denied write Execute() = (%+v, %v)", status, err)
			}
			if _, statErr := os.Lstat(path); !errors.Is(statErr, fs.ErrNotExist) {
				t.Fatalf("denied write target exists: %v", statErr)
			}
		})
	}
}

func TestDriverDeniesAbsoluteAndRelativeSymlinkEscapes(t *testing.T) {
	driver := openDriverForTest(t, sandbox.NetworkDeny)
	t.Cleanup(func() { _ = driver.Close() })
	outside := filepath.Join(filepath.Dir(driver.workspace), "symlink-outside")
	if err := os.WriteFile(outside, []byte("unchanged"), 0o600); err != nil {
		t.Fatal(err)
	}
	relative, err := filepath.Rel(driver.workspace, outside)
	if err != nil {
		t.Fatal(err)
	}
	for name, target := range map[string]string{"absolute": outside, "relative": relative} {
		t.Run(name, func(t *testing.T) {
			link := filepath.Join(driver.workspace, name+"-escape")
			if err := os.Symlink(target, link); err != nil {
				t.Fatal(err)
			}
			status, err := driver.Execute(context.Background(), basicDriverRequest(driver, []string{"/bin/cat", link}), driverDiscardStreams())
			if err != nil || status.Code == 0 {
				t.Fatalf("symlink read Execute() = (%+v, %v), want denial", status, err)
			}
			status, err = driver.Execute(context.Background(), basicDriverRequest(driver, []string{"/bin/sh", "-c", `printf changed > "$1"`, "child", link}), driverDiscardStreams())
			if err != nil || status.Code == 0 {
				t.Fatalf("symlink write Execute() = (%+v, %v), want denial", status, err)
			}
			contents, readErr := os.ReadFile(outside)
			if readErr != nil || string(contents) != "unchanged" {
				t.Fatalf("outside symlink target = (%q, %v)", contents, readErr)
			}
		})
	}
}

func TestDriverLocalIPAllowDenyAndBindMatrix(t *testing.T) {
	executable := driverTestExecutable(t)
	for _, network := range []string{"tcp4", "tcp6"} {
		network := network
		t.Run(network+" outbound", func(t *testing.T) {
			address := "127.0.0.1:0"
			if network == "tcp6" {
				address = "[::1]:0"
			}
			for _, mode := range []sandbox.NetworkMode{sandbox.NetworkAllow, sandbox.NetworkDeny} {
				mode := mode
				t.Run(driverNetworkName(mode), func(t *testing.T) {
					listener, err := net.Listen(network, address)
					if err != nil {
						t.Fatal(err)
					}
					accepted := driverAcceptOne(listener)
					driver := openDriverForTest(t, mode, executable)
					defer driver.Close()
					var stderr bytes.Buffer
					status, executeErr := driver.Execute(context.Background(), helperDriverRequest(driver, executable, "dial", network, listener.Addr().String()), sandbox.Streams{Stdout: io.Discard, Stderr: &stderr})
					if mode == sandbox.NetworkAllow {
						if executeErr != nil || status.Code != 0 {
							_ = listener.Close()
							t.Fatalf("allowed dial Execute() = (%+v, %v, stderr %q)", status, executeErr, stderr.String())
						}
						if err := awaitDriverError(t, accepted, "allowed IP accept"); err != nil {
							t.Fatalf("allowed accept error = %v", err)
						}
						_ = listener.Close()
						return
					}
					if executeErr != nil || status.Code == 0 {
						_ = listener.Close()
						t.Fatalf("denied dial Execute() = (%+v, %v)", status, executeErr)
					}
					if err := listener.Close(); err != nil {
						t.Fatal(err)
					}
					if err := awaitDriverError(t, accepted, "denied IP accept"); err == nil {
						t.Fatal("NetworkDeny listener accepted a connection")
					}
				})
			}
		})

		t.Run(network+" bind", func(t *testing.T) {
			address := "127.0.0.1:0"
			if network == "tcp6" {
				address = "[::1]:0"
			}
			for _, mode := range []sandbox.NetworkMode{sandbox.NetworkAllow, sandbox.NetworkDeny} {
				mode := mode
				t.Run(driverNetworkName(mode), func(t *testing.T) {
					driver := openDriverForTest(t, mode, executable)
					defer driver.Close()
					stdout := newDriverLineWriter()
					result := make(chan driverExecution, 1)
					go func() {
						status, err := driver.Execute(context.Background(), helperDriverRequest(driver, executable, "listen", network, address), sandbox.Streams{Stdout: stdout, Stderr: io.Discard})
						result <- driverExecution{status: status, err: err}
					}()
					if mode == sandbox.NetworkDeny {
						completed := awaitDriverExecution(t, result, "denied IP bind")
						if completed.err != nil || completed.status.Code == 0 {
							t.Fatalf("denied bind Execute() = (%+v, %v)", completed.status, completed.err)
						}
						return
					}
					boundAddress := awaitDriverLine(t, stdout.lines, "allowed bind ready address")
					connection, err := net.DialTimeout(network, boundAddress, 5*time.Second)
					if err != nil {
						t.Fatal(err)
					}
					if _, err := connection.Write([]byte("ready")); err != nil {
						_ = connection.Close()
						t.Fatal(err)
					}
					if err := connection.Close(); err != nil {
						t.Fatal(err)
					}
					completed := awaitDriverExecution(t, result, "allowed IP bind")
					if completed.err != nil || completed.status.Code != 0 {
						t.Fatalf("allowed bind Execute() = (%+v, %v)", completed.status, completed.err)
					}
				})
			}
		})
	}
}

func TestDriverDeniesDockerAndSSHAgentUnixSocketsInsideWorkspace(t *testing.T) {
	executable := driverTestExecutable(t)
	shortBase, err := os.MkdirTemp("/tmp", "otto-seatbelt-unix-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(shortBase) })
	workspace := canonicalDriverTestDirectory(t, filepath.Join(shortBase, "workspace"))
	home := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "home"))
	cache := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "cache"))
	dependencies := defaultDriverDependencies()
	dependencies.userCacheDir = func() (string, error) { return cache, nil }
	driver, err := openWithDependencies(context.Background(), driverOptions(workspace, home, sandbox.NetworkAllow, executable), dependencies)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = driver.Close() })
	for _, name := range []string{"docker.sock", "ssh-agent.sock"} {
		t.Run(name, func(t *testing.T) {
			path := filepath.Join(driver.workspace, name)
			listener, err := net.Listen("unix", path)
			if err != nil {
				t.Fatal(err)
			}
			accepted := driverAcceptOne(listener)
			status, executeErr := driver.Execute(context.Background(), helperDriverRequest(driver, executable, "dial", "unix", path), driverDiscardStreams())
			if executeErr != nil || status.Code == 0 {
				_ = listener.Close()
				t.Fatalf("Unix socket Execute() = (%+v, %v), want policy denial", status, executeErr)
			}
			if err := listener.Close(); err != nil {
				t.Fatal(err)
			}
			if err := awaitDriverError(t, accepted, "denied Unix accept"); err == nil {
				t.Fatal("sandboxed child connected to a workspace Unix socket")
			}
		})
	}
}

func TestDriverDeniesOpenOsaScriptAndNestedSandboxWidening(t *testing.T) {
	driver := openDriverForTest(t, sandbox.NetworkAllow)
	t.Cleanup(func() { _ = driver.Close() })
	outside := filepath.Join(filepath.Dir(driver.workspace), "nested-outside-secret")
	if err := os.WriteFile(outside, []byte("nested-secret-must-not-escape"), 0o600); err != nil {
		t.Fatal(err)
	}

	for _, test := range []struct {
		name string
		argv []string
	}{
		{
			name: "open Launch Services",
			argv: []string{"/usr/bin/open", "-Ra", "Finder"},
		},
		{
			name: "osascript Apple Events",
			argv: []string{"/usr/bin/osascript", "-e", `tell application "Finder" to get name`},
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			var stdout, stderr bytes.Buffer
			status, err := driver.Execute(context.Background(), basicDriverRequest(driver, test.argv), sandbox.Streams{Stdout: &stdout, Stderr: &stderr})
			if err != nil || status.Code == 0 {
				t.Fatalf("escape broker Execute() = (%+v, %v, stdout %q, stderr %q), want policy denial", status, err, stdout.String(), stderr.String())
			}
		})
	}

	var stdout bytes.Buffer
	status, err := driver.Execute(context.Background(), basicDriverRequest(driver, []string{
		sandboxExecPath,
		"-p", "(version 1)\n(allow default)",
		"--", "/bin/cat", outside,
	}), sandbox.Streams{Stdout: &stdout, Stderr: io.Discard})
	if err != nil || status.Code == 0 || strings.Contains(stdout.String(), "nested-secret") {
		t.Fatalf("nested permissive sandbox Execute() = (%+v, %v, stdout %q)", status, err, stdout.String())
	}
}

func TestDriverCopiedEscapeBrokersCannotBypassOperations(t *testing.T) {
	for _, test := range []struct {
		name   string
		source string
		argv   func(string, string) []string
	}{
		{
			name:   "Launch Services operation",
			source: "/usr/bin/open",
			argv: func(copy, _ string) []string {
				return []string{copy, "-Ra", "Finder"}
			},
		},
		{
			name:   "Apple Event operation",
			source: "/usr/bin/osascript",
			argv: func(copy, _ string) []string {
				return []string{copy, "-e", `tell application "Finder" to get name`}
			},
		},
		{
			name:   "nested permissive sandbox",
			source: sandboxExecPath,
			argv: func(copy, outside string) []string {
				return []string{copy, "-p", "(version 1)\n(allow default)", "--", "/bin/cat", outside}
			},
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			driver := openDriverForTest(t, sandbox.NetworkAllow)
			defer driver.Close()
			contents, err := os.ReadFile(test.source)
			if err != nil {
				t.Fatal(err)
			}
			copyPath := filepath.Join(driver.workspace, "copied-"+filepath.Base(test.source))
			if err := os.WriteFile(copyPath, contents, 0o700); err != nil {
				t.Fatal(err)
			}
			outside := filepath.Join(filepath.Dir(driver.workspace), "copied-broker-outside")
			if err := os.WriteFile(outside, []byte("must-not-escape"), 0o600); err != nil {
				t.Fatal(err)
			}
			var stdout bytes.Buffer
			status, executeErr := driver.Execute(context.Background(), basicDriverRequest(driver, test.argv(copyPath, outside)), sandbox.Streams{Stdout: &stdout, Stderr: io.Discard})
			if executeErr == nil && status.Code == 0 || strings.Contains(stdout.String(), "must-not-escape") || strings.TrimSpace(stdout.String()) == "Finder" {
				t.Fatalf("copied broker bypassed operation policy: status=%+v error=%v stdout=%q", status, executeErr, stdout.String())
			}
			if executeErr != nil {
				assertUnavailableRuntime(t, executeErr)
			}
		})
	}
}

func TestDriverSandboxChildCanSignalOwnChildButNotHostProcess(t *testing.T) {
	driver := openDriverForTest(t, sandbox.NetworkDeny)
	t.Cleanup(func() { _ = driver.Close() })

	status, err := driver.Execute(context.Background(), basicDriverRequest(driver, []string{
		"/bin/sh", "-c", `/bin/sleep 30 & child=$!; kill -TERM "$child"; wait "$child"; code=$?; test "$code" -gt 0`,
	}), driverDiscardStreams())
	if err != nil || status.Code != 0 {
		t.Fatalf("same-sandbox signal Execute() = (%+v, %v)", status, err)
	}

	fifo := filepath.Join(t.TempDir(), "host-process-fifo")
	if err := unix.Mkfifo(fifo, 0o600); err != nil {
		t.Fatal(err)
	}
	host := exec.Command("/bin/cat", fifo)
	host.Stdout = io.Discard
	host.Stderr = io.Discard
	if err := host.Start(); err != nil {
		t.Fatal(err)
	}
	hostPID := host.Process.Pid
	t.Cleanup(func() {
		_ = host.Process.Kill()
		_ = host.Wait()
	})
	status, err = driver.Execute(context.Background(), basicDriverRequest(driver, []string{
		"/bin/sh", "-c", `kill -TERM "$1"`, "child", strconv.Itoa(hostPID),
	}), driverDiscardStreams())
	if err != nil || status.Code == 0 {
		t.Fatalf("cross-sandbox signal Execute() = (%+v, %v), want denial", status, err)
	}
	if err := unix.Kill(hostPID, 0); err != nil {
		t.Fatalf("test-owned host process was signaled: %v", err)
	}
}

func TestDriverShellAndGitCompatibilityWithoutHostConfig(t *testing.T) {
	gitPreflightHome := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "git-preflight-home"))
	gitPreflightTemp := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "git-preflight-temp"))
	gitPreflight := exec.Command("/usr/bin/git", "--version")
	gitPreflight.Env = []string{
		"PATH=/usr/bin:/bin",
		"HOME=" + gitPreflightHome,
		"TMPDIR=" + gitPreflightTemp,
		"LC_ALL=C",
		"GIT_CONFIG_NOSYSTEM=1",
		"GIT_CONFIG_GLOBAL=/dev/null",
		"GIT_CONFIG_COUNT=0",
		"xcrun_nocache=1",
	}
	if err := gitPreflight.Run(); err != nil {
		t.Skipf("Git toolchain is absent: %v", err)
	}

	driver := openDriverForTest(t, sandbox.NetworkDeny)
	t.Cleanup(func() { _ = driver.Close() })
	private := driver.PrivateDirectories()

	var stdout bytes.Buffer
	status, err := driver.Execute(context.Background(), basicDriverRequest(driver, []string{"/bin/sh", "-c", "printf shell-compatible"}), sandbox.Streams{Stdout: &stdout, Stderr: io.Discard})
	if err != nil || status.Code != 0 || stdout.String() != "shell-compatible" {
		t.Fatalf("/bin/sh compatibility = (%+v, %v, %q)", status, err, stdout.String())
	}
	stdout.Reset()
	var stderr bytes.Buffer
	versionRequest := basicDriverRequest(driver, []string{"/usr/bin/git", "--version"})
	versionRequest.Env = append(versionRequest.Env,
		"GIT_CONFIG_NOSYSTEM=1",
		"GIT_CONFIG_GLOBAL=/dev/null",
		"GIT_CONFIG_COUNT=0",
		"xcrun_nocache=1",
	)
	status, err = driver.Execute(context.Background(), versionRequest, sandbox.Streams{Stdout: &stdout, Stderr: &stderr})
	if err != nil || status.Code != 0 || !strings.HasPrefix(stdout.String(), "git version ") {
		t.Fatalf("git --version compatibility = (%+v, %v, stdout %q, stderr %q)", status, err, stdout.String(), stderr.String())
	}

	hostGitHome := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "host-git-home"))
	initCommand := exec.Command("/usr/bin/git", "init", "-q", driver.workspace)
	initCommand.Env = []string{
		"PATH=/usr/bin:/bin",
		"HOME=" + hostGitHome,
		"LC_ALL=C",
		"GIT_CONFIG_NOSYSTEM=1",
		"GIT_CONFIG_GLOBAL=/dev/null",
		"xcrun_nocache=1",
	}
	if output, err := initCommand.CombinedOutput(); err != nil {
		t.Fatalf("create test-owned Git repository: %v (%q)", err, output)
	}
	if err := os.WriteFile(filepath.Join(driver.workspace, "tracked.txt"), []byte("fixture"), 0o600); err != nil {
		t.Fatal(err)
	}
	request := basicDriverRequest(driver, []string{"/usr/bin/git", "status", "--porcelain"})
	request.Env = []string{
		"PATH=/usr/bin:/bin",
		"HOME=" + private.Home,
		"TMPDIR=" + private.Temp,
		"LC_ALL=C",
		"GIT_CONFIG_NOSYSTEM=1",
		"GIT_CONFIG_GLOBAL=/dev/null",
		"GIT_CONFIG_COUNT=0",
		"xcrun_nocache=1",
	}
	stdout.Reset()
	status, err = driver.Execute(context.Background(), request, sandbox.Streams{Stdout: &stdout, Stderr: io.Discard})
	if err != nil || status.Code != 0 || !strings.Contains(stdout.String(), "tracked.txt") {
		t.Fatalf("git status compatibility = (%+v, %v, %q)", status, err, stdout.String())
	}
}

func TestDriverXcrunAndClangCompatibility(t *testing.T) {
	if info, err := os.Lstat("/usr/bin/xcrun"); err != nil || !info.Mode().IsRegular() || info.Mode().Perm()&0o111 == 0 {
		t.Skip("xcrun is absent")
	}
	preflightHome := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "xcrun-preflight-home"))
	preflightTemp := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "xcrun-preflight-temp"))
	find := exec.Command("/usr/bin/xcrun", "--find", "clang")
	find.Env = []string{
		"PATH=/usr/bin:/bin",
		"HOME=" + preflightHome,
		"TMPDIR=" + preflightTemp,
		"LC_ALL=C",
		"xcrun_nocache=1",
	}
	output, err := find.Output()
	if err != nil {
		t.Skipf("Clang toolchain is absent: %v", err)
	}
	clang := strings.TrimSpace(string(output))
	canonicalClang, err := filepath.EvalSymlinks(clang)
	if err != nil {
		t.Fatalf("canonicalize xcrun Clang result: %v", err)
	}

	driver := openDriverForTest(t, sandbox.NetworkDeny)
	t.Cleanup(func() { _ = driver.Close() })
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	xcrunRequest := basicDriverRequest(driver, []string{"/usr/bin/xcrun", "--find", "clang"})
	xcrunRequest.Env = append(xcrunRequest.Env, "xcrun_nocache=1")
	status, err := driver.Execute(context.Background(), xcrunRequest, sandbox.Streams{Stdout: &stdout, Stderr: &stderr})
	if err != nil || status.Code != 0 {
		t.Fatalf("sandboxed xcrun --find clang = (%+v, %v, stdout %q, stderr %q)", status, err, stdout.String(), stderr.String())
	}
	gotClang, err := filepath.EvalSymlinks(strings.TrimSpace(stdout.String()))
	if err != nil || gotClang != canonicalClang {
		t.Fatalf("sandboxed xcrun result = %q (%v), want %q", stdout.String(), err, canonicalClang)
	}

	source := filepath.Join(driver.workspace, "trivial.c")
	binary := filepath.Join(driver.workspace, "trivial-clang-output")
	if err := os.WriteFile(source, []byte("int main(void) { return 0; }\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	compileRequest := basicDriverRequest(driver, []string{"/usr/bin/xcrun", "clang", source, "-o", binary})
	compileRequest.Env = append(compileRequest.Env, "xcrun_nocache=1")
	status, err = driver.Execute(context.Background(), compileRequest, driverDiscardStreams())
	if err != nil || status.Code != 0 {
		t.Fatalf("sandboxed Clang compile = (%+v, %v)", status, err)
	}
	if info, err := os.Lstat(binary); err != nil || !info.Mode().IsRegular() || info.Mode().Perm()&0o111 == 0 {
		t.Fatalf("Clang output = (%v, %v)", info, err)
	}
}

func TestDriverCanonicalHomebrewGoCompatibility(t *testing.T) {
	const homebrewGo = "/opt/homebrew/bin/go"
	info, err := os.Lstat(homebrewGo)
	if errors.Is(err, fs.ErrNotExist) {
		t.Skip("canonical Apple Silicon Homebrew Go is absent")
	}
	if err != nil || info.Mode().Perm()&0o111 == 0 {
		t.Fatalf("Homebrew Go is present but invalid: info=%v error=%v", info, err)
	}
	canonicalGo, err := filepath.EvalSymlinks(homebrewGo)
	if err != nil || !pathWithin("/opt/homebrew/Cellar/go", canonicalGo) {
		t.Fatalf("Homebrew Go canonical target = %q, %v", canonicalGo, err)
	}

	driver := openDriverForTest(t, sandbox.NetworkDeny)
	t.Cleanup(func() { _ = driver.Close() })
	private := driver.PrivateDirectories()
	goBuild := canonicalDriverTestDirectory(t, filepath.Join(private.Cache, "go-build"))
	goMod := canonicalDriverTestDirectory(t, filepath.Join(private.Cache, "go-mod"))
	goPath := canonicalDriverTestDirectory(t, filepath.Join(private.Home, "go"))
	goEnvironment := []string{
		"PATH=/opt/homebrew/bin:/usr/bin:/bin",
		"HOME=" + private.Home,
		"TMPDIR=" + private.Temp,
		"LC_ALL=C",
		"GOCACHE=" + goBuild,
		"GOMODCACHE=" + goMod,
		"GOPATH=" + goPath,
		"GOTOOLCHAIN=local",
		"GOPROXY=off",
		"GOSUMDB=off",
		"GOTELEMETRY=off",
	}
	request := basicDriverRequest(driver, []string{homebrewGo, "version"})
	request.Env = slices.Clone(goEnvironment)
	var stdout bytes.Buffer
	status, err := driver.Execute(context.Background(), request, sandbox.Streams{Stdout: &stdout, Stderr: io.Discard})
	if err != nil || status.Code != 0 || !strings.HasPrefix(stdout.String(), "go version go") {
		t.Fatalf("Homebrew go version = (%+v, %v, %q)", status, err, stdout.String())
	}

	if err := os.WriteFile(filepath.Join(driver.workspace, "go.mod"), []byte("module example.invalid/seatbelttest\n\ngo 1.20\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(driver.workspace, "answer.go"), []byte("package answer\n\nfunc Value() int { return 42 }\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(driver.workspace, "answer_test.go"), []byte("package answer\n\nimport \"testing\"\n\nfunc TestValue(t *testing.T) { if Value() != 42 { t.Fatal(Value()) } }\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	request = basicDriverRequest(driver, []string{homebrewGo, "test", "./..."})
	request.Env = slices.Clone(goEnvironment)
	status, err = driver.Execute(context.Background(), request, driverDiscardStreams())
	if err != nil || status.Code != 0 {
		t.Fatalf("dependency-free Homebrew go test = (%+v, %v)", status, err)
	}
}

func TestSeatbeltHelperProcess(t *testing.T) {
	if os.Getenv("OTTO_SEATBELT_HELPER_PROCESS") != "1" {
		return
	}
	separator := slices.Index(os.Args, "--")
	if separator < 0 || len(os.Args) < separator+4 {
		os.Exit(64)
	}
	mode := os.Args[separator+1]
	network := os.Args[separator+2]
	address := os.Args[separator+3]
	switch mode {
	case "dial":
		dialer := net.Dialer{Timeout: 5 * time.Second}
		connection, err := dialer.Dial(network, address)
		if err != nil {
			os.Exit(65)
		}
		if _, err := connection.Write([]byte("seatbelt-test")); err != nil {
			_ = connection.Close()
			os.Exit(66)
		}
		if err := connection.Close(); err != nil {
			os.Exit(67)
		}
		os.Exit(0)
	case "listen":
		listener, err := net.Listen(network, address)
		if err != nil {
			os.Exit(68)
		}
		if _, err := fmt.Fprintln(os.Stdout, listener.Addr().String()); err != nil {
			_ = listener.Close()
			os.Exit(69)
		}
		connection, err := listener.Accept()
		if err != nil {
			_ = listener.Close()
			os.Exit(70)
		}
		buffer := make([]byte, 16)
		_, _ = connection.Read(buffer)
		if err := connection.Close(); err != nil {
			_ = listener.Close()
			os.Exit(71)
		}
		if err := listener.Close(); err != nil {
			os.Exit(72)
		}
		os.Exit(0)
	default:
		os.Exit(73)
	}
}

type driverWriterFunc func([]byte) (int, error)

func (write driverWriterFunc) Write(data []byte) (int, error) {
	return write(data)
}

type driverExecution struct {
	status sandbox.ExitStatus
	err    error
}

type driverLineWriter struct {
	mu       sync.Mutex
	buffer   bytes.Buffer
	reported int
	lines    chan string
}

func newDriverLineWriter() *driverLineWriter {
	return &driverLineWriter{lines: make(chan string, 4)}
}

func (w *driverLineWriter) Write(data []byte) (int, error) {
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

func driverOpenFixture(t *testing.T) (string, string, string, driverDependencies) {
	t.Helper()
	workspace := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "workspace"))
	home := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "home"))
	cache := canonicalDriverTestDirectory(t, filepath.Join(t.TempDir(), "cache"))
	dependencies := defaultDriverDependencies()
	dependencies.userCacheDir = func() (string, error) { return cache, nil }
	return workspace, home, cache, dependencies
}

func driverOptions(workspace, home string, network sandbox.NetworkMode, readPaths ...string) Options {
	return Options{
		Workspace:   workspace,
		Shell:       "/bin/sh",
		Home:        home,
		HostEntries: []string{"PATH=/usr/bin:/bin", "LC_ALL=C"},
		ReadPaths:   slices.Clone(readPaths),
		Network:     network,
	}
}

func openDriverForTest(t *testing.T, network sandbox.NetworkMode, readPaths ...string) *Driver {
	t.Helper()
	workspace, home, _, dependencies := driverOpenFixture(t)
	driver, err := openWithDependencies(context.Background(), driverOptions(workspace, home, network, readPaths...), dependencies)
	if err != nil {
		t.Fatalf("Open() error = %v", err)
	}
	return driver
}

func basicDriverRequest(driver *Driver, argv []string) sandbox.Request {
	private := driver.PrivateDirectories()
	return sandbox.Request{
		Argv: slices.Clone(argv),
		Dir:  driver.workspace,
		Env: []string{
			"PATH=/usr/bin:/bin",
			"HOME=" + private.Home,
			"TMPDIR=" + private.Temp,
			"LC_ALL=C",
		},
	}
}

func helperDriverRequest(driver *Driver, executable string, values ...string) sandbox.Request {
	request := basicDriverRequest(driver, driverHelperCommand(executable, values...))
	request.Env = append(request.Env, "OTTO_SEATBELT_HELPER_PROCESS=1")
	return request
}

func driverHelperCommand(executable string, values ...string) []string {
	return append([]string{executable, "-test.run=^TestSeatbeltHelperProcess$", "--"}, values...)
}

func driverTestExecutable(t testing.TB) string {
	t.Helper()
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	executable, err = filepath.EvalSymlinks(executable)
	if err != nil {
		t.Fatal(err)
	}
	return executable
}

func driverDiscardStreams() sandbox.Streams {
	return sandbox.Streams{Stdout: io.Discard, Stderr: io.Discard}
}

func driverNetworkName(mode sandbox.NetworkMode) string {
	if mode == sandbox.NetworkAllow {
		return "allow"
	}
	return "deny"
}

func assertUnavailableReason(t *testing.T, driver *Driver, err error, reason sandbox.UnavailableReason) {
	t.Helper()
	if driver != nil {
		_ = driver.Close()
		t.Fatal("Open() returned a Driver on failure")
	}
	var unavailable *sandbox.UnavailableError
	if !errors.Is(err, sandbox.ErrUnavailable) || !errors.As(err, &unavailable) || unavailable.Reason != reason {
		t.Fatalf("Open() error = %v, want unavailable reason %q", err, reason)
	}
	want := (&sandbox.UnavailableError{Reason: reason}).Error()
	if err.Error() != want {
		t.Fatalf("Open() error = %q, want fixed %q", err, want)
	}
}

func assertUnavailableRuntime(t *testing.T, err error) {
	t.Helper()
	var unavailable *sandbox.UnavailableError
	if !errors.Is(err, sandbox.ErrUnavailable) || !errors.As(err, &unavailable) || unavailable.Reason != sandbox.ReasonRuntimeFailure ||
		err.Error() != (&sandbox.UnavailableError{Reason: sandbox.ReasonRuntimeFailure}).Error() {
		t.Fatalf("runtime error = %v, want fixed runtime unavailable", err)
	}
}

func assertSafeDriverError(t *testing.T, err error, forbidden ...string) {
	t.Helper()
	if err == nil || len(err.Error()) > 128 {
		t.Fatalf("Driver error is nil or unbounded: %v", err)
	}
	for _, value := range forbidden {
		if value != "" && strings.Contains(err.Error(), value) {
			t.Fatalf("Driver error exposes private value %q: %v", value, err)
		}
	}
}

func driverAcceptOne(listener net.Listener) <-chan error {
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

func awaitDriverSignal(t *testing.T, signal <-chan struct{}, description string) {
	t.Helper()
	select {
	case <-signal:
	case <-time.After(driverTestDeadlockDeadline):
		t.Fatalf("timed out waiting for %s", description)
	}
}

func awaitDriverLine(t *testing.T, lines <-chan string, description string) string {
	t.Helper()
	select {
	case line := <-lines:
		return line
	case <-time.After(driverTestDeadlockDeadline):
		t.Fatalf("timed out waiting for %s", description)
		return ""
	}
}

func awaitDriverExecution(t *testing.T, result <-chan driverExecution, description string) driverExecution {
	t.Helper()
	select {
	case completed := <-result:
		return completed
	case <-time.After(driverTestDeadlockDeadline):
		t.Fatalf("timed out waiting for %s", description)
		return driverExecution{}
	}
}

func awaitDriverError(t *testing.T, result <-chan error, description string) error {
	t.Helper()
	select {
	case err := <-result:
		return err
	case <-time.After(driverTestDeadlockDeadline):
		t.Fatalf("timed out waiting for %s", description)
		return nil
	}
}

func awaitDriverOpen(t *testing.T, result <-chan struct {
	driver *Driver
	err    error
}, description string) struct {
	driver *Driver
	err    error
} {
	t.Helper()
	select {
	case completed := <-result:
		return completed
	case <-time.After(driverTestDeadlockDeadline):
		t.Fatalf("timed out waiting for %s", description)
		return struct {
			driver *Driver
			err    error
		}{}
	}
}

func canonicalDriverTestDirectory(t testing.TB, path string) string {
	t.Helper()
	if err := os.MkdirAll(path, 0o700); err != nil {
		t.Fatal(err)
	}
	canonical, err := filepath.EvalSymlinks(path)
	if err != nil {
		t.Fatal(err)
	}
	return canonical
}

func writeDriverTestExecutable(t testing.TB, path, command string) {
	t.Helper()
	if err := os.WriteFile(path, []byte("#!/bin/sh\n"+command+"\n"), 0o700); err != nil {
		t.Fatal(err)
	}
}

func shellQuoteForTest(value string) string {
	return "'" + strings.ReplaceAll(value, "'", "'\\''") + "'"
}
