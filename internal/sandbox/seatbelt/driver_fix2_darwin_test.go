//go:build darwin

package seatbelt

import (
	"context"
	"errors"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/sandbox/internal/nativeprocess"
	"golang.org/x/sys/unix"
)

func TestOpenSelfTestRejectsSameMetadataInodeSubstitutionAtEveryHostEvent(t *testing.T) {
	for _, stage := range []selfTestFixtureEventStage{
		selfTestFixtureBeforeDispatch,
		selfTestFixtureBeforeReadback,
		selfTestFixtureBeforeCleanup,
	} {
		t.Run(stage.String(), func(t *testing.T) {
			workspace, home, cache, dependencies := driverOpenFixture(t)
			marker := filepath.Join(workspace, "preexisting-must-not-change")
			const markerContents = "preexisting-content-must-survive"
			if err := os.WriteFile(marker, []byte(markerContents), 0o600); err != nil {
				t.Fatal(err)
			}
			markerBefore, err := os.Lstat(marker)
			if err != nil {
				t.Fatal(err)
			}

			var fixturePath, movedPath string
			var originalBefore, replacementBefore fs.FileInfo
			var expectedContents []byte
			dependencies.selfTestEvent = func(event selfTestFixtureEvent) {
				if event.kind == selfTestAllowedWorkspaceWriteFixture && event.stage == selfTestFixtureCandidate {
					fixturePath = event.path
				}
				if event.kind != selfTestAllowedWorkspaceWriteFixture || event.stage != stage || movedPath != "" {
					return
				}
				if fixturePath == "" || event.path != fixturePath {
					t.Fatalf("fixture event path = %q, candidate = %q", event.path, fixturePath)
				}
				expectedContents, err = os.ReadFile(event.path)
				if err != nil {
					t.Fatal(err)
				}
				originalBefore, err = os.Lstat(event.path)
				if err != nil || !originalBefore.Mode().IsRegular() || originalBefore.Mode().Perm() != selfTestFixtureMode {
					t.Fatalf("original fixture = (%v, %v)", originalBefore, err)
				}
				movedPath = filepath.Join(workspace, "moved-original-"+stage.String())
				if err := os.Rename(event.path, movedPath); err != nil {
					t.Fatal(err)
				}
				if err := os.WriteFile(event.path, expectedContents, selfTestFixtureMode); err != nil {
					t.Fatal(err)
				}
				if err := os.Chmod(event.path, selfTestFixtureMode); err != nil {
					t.Fatal(err)
				}
				replacementBefore, err = os.Lstat(event.path)
				if err != nil || !replacementBefore.Mode().IsRegular() || replacementBefore.Mode().Perm() != originalBefore.Mode().Perm() || os.SameFile(originalBefore, replacementBefore) {
					t.Fatalf("same-metadata replacement = (%v, %v), original = %v", replacementBefore, err, originalBefore)
				}
			}

			driver, openErr := openWithDependencies(context.Background(), driverOptions(workspace, home, sandbox.NetworkDeny), dependencies)
			assertUnavailableReason(t, driver, openErr, sandbox.ReasonSelfTestFailed)
			if fixturePath == "" || movedPath == "" {
				t.Fatalf("substitution events = fixture %q moved %q", fixturePath, movedPath)
			}
			assertFix2FileIdentityAndContents(t, fixturePath, replacementBefore, expectedContents)
			assertFix2FileIdentityAndContents(t, movedPath, originalBefore, expectedContents)
			assertFix2FileIdentityAndContents(t, marker, markerBefore, []byte(markerContents))
			assertNoStateLeaves(t, cache)
		})
	}
}

func TestOpenSelfTestChildValidatesTrustedFixtureBeforeWriting(t *testing.T) {
	workspace, home, cache, dependencies := driverOpenFixture(t)
	productionProbe := dependencies.runProbe
	var fixturePath, movedPath string
	var originalBefore, replacementBefore fs.FileInfo
	dependencies.selfTestEvent = func(event selfTestFixtureEvent) {
		if event.stage == selfTestFixtureCandidate && event.kind == selfTestAllowedWorkspaceWriteFixture {
			fixturePath = event.path
		}
	}
	dependencies.runProbe = func(ctx context.Context, manager *nativeprocess.Manager, probe selfTestProbe) (selfTestProbeResult, error) {
		if probe.kind != selfTestAllowedWrite {
			return productionProbe(ctx, manager, probe)
		}
		if fixturePath == "" {
			t.Fatal("workspace fixture candidate was not observed")
		}
		contents, err := os.ReadFile(fixturePath)
		if err != nil || len(contents) != 0 {
			t.Fatalf("host-created fixture contents = %q, %v; want empty", contents, err)
		}
		originalBefore, err = os.Lstat(fixturePath)
		if err != nil {
			t.Fatal(err)
		}
		movedPath = filepath.Join(workspace, "moved-before-child-open")
		if err := os.Rename(fixturePath, movedPath); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(fixturePath, contents, selfTestFixtureMode); err != nil {
			t.Fatal(err)
		}
		if err := os.Chmod(fixturePath, selfTestFixtureMode); err != nil {
			t.Fatal(err)
		}
		replacementBefore, err = os.Lstat(fixturePath)
		if err != nil || os.SameFile(originalBefore, replacementBefore) {
			t.Fatalf("replacement fixture = (%v, %v), original = %v", replacementBefore, err, originalBefore)
		}
		return productionProbe(ctx, manager, probe)
	}

	driver, err := openWithDependencies(context.Background(), driverOptions(workspace, home, sandbox.NetworkDeny), dependencies)
	assertUnavailableReason(t, driver, err, sandbox.ReasonSelfTestFailed)
	assertFix2FileIdentityAndContents(t, fixturePath, replacementBefore, nil)
	assertFix2FileIdentityAndContents(t, movedPath, originalBefore, nil)
	assertNoStateLeaves(t, cache)
}

func TestOpenSelfTestCleansEveryFixtureAfterFirstWriteFailureOrCancellation(t *testing.T) {
	for _, outcome := range []string{"failure", "cancellation"} {
		t.Run(outcome, func(t *testing.T) {
			workspace, home, cache, dependencies := driverOpenFixture(t)
			ctx, cancel := context.WithCancel(context.Background())
			marker := filepath.Join(workspace, "preexisting-first-write-marker")
			const markerContents = "first-write-marker-must-survive"
			if err := os.WriteFile(marker, []byte(markerContents), 0o600); err != nil {
				t.Fatal(err)
			}
			markerBefore, err := os.Lstat(marker)
			if err != nil {
				t.Fatal(err)
			}
			paths := make(map[selfTestFixtureKind]string)
			productionProbe := dependencies.runProbe
			dependencies.selfTestEvent = func(event selfTestFixtureEvent) {
				if event.stage == selfTestFixtureCandidate {
					paths[event.kind] = event.path
				}
			}
			dependencies.runProbe = func(probeCtx context.Context, manager *nativeprocess.Manager, probe selfTestProbe) (selfTestProbeResult, error) {
				if probe.kind != selfTestAllowedWrite {
					return productionProbe(probeCtx, manager, probe)
				}
				path := paths[selfTestAllowedWorkspaceWriteFixture]
				if err := writeFix2ExistingFixture(path, []byte("otto-seatbelt-allowed-write")); err != nil {
					t.Fatalf("write first trusted fixture: %v", err)
				}
				if outcome == "cancellation" {
					cancel()
					return selfTestProbeResult{leaderReaped: true}, context.Canceled
				}
				return selfTestProbeResult{leaderReaped: true}, errors.New("raw first-write failure detail")
			}

			driver, openErr := openWithDependencies(ctx, driverOptions(workspace, home, sandbox.NetworkDeny), dependencies)
			if outcome == "cancellation" {
				if driver != nil || !errors.Is(openErr, context.Canceled) || openErr.Error() != context.Canceled.Error() {
					t.Fatalf("cancelled Open = (driver %t, %v)", driver != nil, openErr)
				}
			} else {
				assertUnavailableReason(t, driver, openErr, sandbox.ReasonSelfTestFailed)
			}
			for _, kind := range []selfTestFixtureKind{selfTestAllowedWorkspaceWriteFixture, selfTestAllowedPrivateWriteFixture} {
				path := paths[kind]
				if path == "" {
					t.Fatalf("fixture %d was not prepared", kind)
				}
				if _, statErr := os.Lstat(path); !errors.Is(statErr, fs.ErrNotExist) {
					t.Fatalf("partial fixture %q remains: %v", path, statErr)
				}
			}
			assertFix2FileIdentityAndContents(t, marker, markerBefore, []byte(markerContents))
			assertSafeDriverError(t, openErr, "raw first-write", workspace, home, cache)
			assertNoStateLeaves(t, cache)
		})
	}
}

func TestOpenSelfTestRetiresEveryTrustedFixtureFDOnceOnCloseErrors(t *testing.T) {
	workspace, home, cache, dependencies := driverOpenFixture(t)
	calls := make(map[int]int)
	var callsMu sync.Mutex
	dependencies.selfTestCloseFD = func(fd int) error {
		callsMu.Lock()
		calls[fd]++
		callsMu.Unlock()
		closeErr := unix.Close(fd)
		if closeErr != nil {
			return closeErr
		}
		return errors.New("raw retained fixture close detail")
	}
	var workspaceFixture string
	dependencies.selfTestEvent = func(event selfTestFixtureEvent) {
		if event.stage == selfTestFixtureCandidate && event.kind == selfTestAllowedWorkspaceWriteFixture {
			workspaceFixture = event.path
		}
	}

	driver, err := openWithDependencies(context.Background(), driverOptions(workspace, home, sandbox.NetworkDeny), dependencies)
	assertUnavailableReason(t, driver, err, sandbox.ReasonSelfTestFailed)
	callsMu.Lock()
	gotCalls := make(map[int]int, len(calls))
	for fd, count := range calls {
		gotCalls[fd] = count
	}
	callsMu.Unlock()
	if len(gotCalls) != 5 {
		t.Fatalf("retained fixture descriptors closed = %v, want five", gotCalls)
	}
	for fd, count := range gotCalls {
		if count != 1 {
			t.Fatalf("retained fixture descriptor %d close calls = %d, want one", fd, count)
		}
	}
	if _, statErr := os.Lstat(workspaceFixture); !errors.Is(statErr, fs.ErrNotExist) {
		t.Fatalf("workspace fixture remains after close failure: %v", statErr)
	}
	assertSafeDriverError(t, err, "raw retained", workspace, home, cache)
	assertNoStateLeaves(t, cache)
}

func TestDriverInfrastructureClassificationAndPoisonAreAtomic(t *testing.T) {
	for _, failure := range []string{"filter", "manager", "filter-finish"} {
		t.Run(failure, func(t *testing.T) {
			workspace, home, _, dependencies := driverOpenFixture(t)
			var callsMu sync.Mutex
			calls := 0
			var activeFilter *infrastructureStderrFilter
			dependencies.runExecution = func(_ context.Context, _ *nativeprocess.Manager, spec nativeprocess.Spec) (nativeprocess.Result, error) {
				callsMu.Lock()
				calls++
				call := calls
				callsMu.Unlock()
				if call != 1 {
					return nativeprocess.Result{Code: 0}, nil
				}
				activeFilter, _ = spec.Stderr.(*infrastructureStderrFilter)
				switch failure {
				case "filter":
					_, _ = io.WriteString(spec.Stderr, "sandbox-exec: atomic infrastructure classification\n")
					return nativeprocess.Result{Code: 65}, nil
				case "manager":
					return nativeprocess.Result{}, sandbox.ErrChildWait
				default:
					_, _ = io.WriteString(spec.Stderr, "ordinary stderr that fails")
					return nativeprocess.Result{Code: 0}, nil
				}
			}
			driver, err := openWithDependencies(context.Background(), driverOptions(workspace, home, sandbox.NetworkDeny), dependencies)
			if err != nil {
				t.Fatal(err)
			}
			defer driver.Close()

			type classificationSnapshot struct {
				poisoned      bool
				filterVisible bool
			}
			classified := make(chan classificationSnapshot, 1)
			releaseClassification := make(chan struct{})
			secondAtAdmission := make(chan struct{})
			var admissionMu sync.Mutex
			admissions := 0
			driver.executionEvent = func(event driverExecutionEvent) {
				switch event.stage {
				case driverExecutionBeforeAdmission:
					admissionMu.Lock()
					admissions++
					if admissions == 2 {
						close(secondAtAdmission)
					}
					admissionMu.Unlock()
				case driverExecutionInfrastructureClassified:
					snapshot := classificationSnapshot{poisoned: driver.poisoned}
					if activeFilter != nil {
						snapshot.filterVisible = activeFilter.infrastructure
					}
					classified <- snapshot
					<-releaseClassification
				}
			}

			firstResult := make(chan driverExecution, 1)
			go func() {
				stderr := io.Writer(io.Discard)
				if failure == "filter-finish" {
					stderr = driverWriterFunc(func([]byte) (int, error) {
						return 0, errors.New("raw filter finish detail")
					})
				}
				status, executeErr := driver.Execute(context.Background(), basicDriverRequest(driver, []string{"/bin/sh", "-c", "exit 0"}), sandbox.Streams{Stdout: io.Discard, Stderr: stderr})
				firstResult <- driverExecution{status: status, err: executeErr}
			}()
			snapshot := awaitFix2Classification(t, classified)
			if !snapshot.poisoned {
				t.Fatal("infrastructure classification was published before poison")
			}
			if failure == "filter" && snapshot.filterVisible {
				t.Fatal("filter classification became visible before poison callback completed")
			}

			secondResult := make(chan driverExecution, 1)
			go func() {
				status, executeErr := driver.Execute(context.Background(), basicDriverRequest(driver, []string{"/bin/sh", "-c", "exit 0"}), driverDiscardStreams())
				secondResult <- driverExecution{status: status, err: executeErr}
			}()
			awaitDriverSignal(t, secondAtAdmission, "second execution admission")
			close(releaseClassification)
			first := awaitDriverExecution(t, firstResult, "atomic infrastructure execution")
			second := awaitDriverExecution(t, secondResult, "post-classification admission")
			assertUnavailableRuntime(t, first.err)
			assertUnavailableRuntime(t, second.err)
			callsMu.Lock()
			gotCalls := calls
			callsMu.Unlock()
			if gotCalls != 1 {
				t.Fatalf("runner calls after classification = %d, want one", gotCalls)
			}
		})
	}
}

func TestOpenCleanupCancellationWinsAfterManagerAndStateCleanup(t *testing.T) {
	for _, cancellation := range []struct {
		name  string
		cause error
		want  error
	}{
		{name: "canceled", cause: context.Canceled, want: context.Canceled},
		{name: "deadline", cause: context.DeadlineExceeded, want: context.DeadlineExceeded},
	} {
		for _, triggerAt := range []string{"manager-cleanup", "state-cleanup"} {
			t.Run(cancellation.name+"/"+triggerAt, func(t *testing.T) {
				workspace, home, cache, dependencies := driverOpenFixture(t)
				ctx, cancel := context.WithCancelCause(context.Background())
				var order []string
				productionManagerClose := dependencies.closeManager
				dependencies.closeManager = func(manager *nativeprocess.Manager) error {
					order = append(order, "manager")
					closeErr := productionManagerClose(manager)
					if triggerAt == "manager-cleanup" {
						cancel(cancellation.cause)
						return errors.Join(closeErr, sandbox.ErrChildTerminate, errors.New("raw manager cleanup cancellation detail"))
					}
					return closeErr
				}
				productionStateClose := dependencies.closeState
				dependencies.closeState = func(privateState *state) error {
					order = append(order, "state")
					closeErr := productionStateClose(privateState)
					if triggerAt == "state-cleanup" {
						cancel(cancellation.cause)
						return errors.Join(closeErr, errors.New("raw state cleanup cancellation detail"))
					}
					return closeErr
				}
				dependencies.runProbe = func(context.Context, *nativeprocess.Manager, selfTestProbe) (selfTestProbeResult, error) {
					return selfTestProbeResult{leaderReaped: true}, errors.New("raw self-test trigger detail")
				}

				driver, openErr := openWithDependencies(ctx, driverOptions(workspace, home, sandbox.NetworkDeny), dependencies)
				if driver != nil || !errors.Is(openErr, cancellation.want) || openErr.Error() != cancellation.want.Error() {
					t.Fatalf("Open cleanup cancellation = (driver %t, %v), want %v", driver != nil, openErr, cancellation.want)
				}
				if !errors.Is(openErr, sandbox.ErrUnavailable) {
					t.Fatalf("Open cleanup cancellation lost self-test identity: %v", openErr)
				}
				if triggerAt == "manager-cleanup" && !errors.Is(openErr, sandbox.ErrChildTerminate) {
					t.Fatalf("manager cleanup identity missing: %v", openErr)
				}
				if triggerAt == "state-cleanup" && !errors.Is(openErr, errStateCleanup) {
					t.Fatalf("state cleanup identity missing: %v", openErr)
				}
				if !slices.Equal(order, []string{"manager", "state"}) {
					t.Fatalf("cleanup order = %v, want manager then state", order)
				}
				assertSafeDriverError(t, openErr, "raw manager", "raw state", "raw self-test", workspace, home, cache)
				assertNoStateLeaves(t, cache)
			})
		}
	}
}

func TestDriverAdmissionCancellationWinsClosedOrPoisonedRace(t *testing.T) {
	for _, cancellation := range []struct {
		name  string
		cause error
		want  error
	}{
		{name: "canceled", cause: context.Canceled, want: context.Canceled},
		{name: "deadline", cause: context.DeadlineExceeded, want: context.DeadlineExceeded},
	} {
		for _, state := range []string{"closed", "poisoned"} {
			t.Run(cancellation.name+"/"+state, func(t *testing.T) {
				driver := openDriverForTest(t, sandbox.NetworkDeny)
				if state == "closed" {
					if err := driver.Close(); err != nil {
						t.Fatal(err)
					}
				} else {
					driver.mu.Lock()
					driver.poisoned = true
					driver.mu.Unlock()
					defer driver.Close()
				}
				ctx, cancel := context.WithCancelCause(context.Background())
				var once sync.Once
				driver.executionEvent = func(event driverExecutionEvent) {
					if event.stage == driverExecutionBeforeAdmission {
						once.Do(func() { cancel(cancellation.cause) })
					}
				}
				status, executeErr := driver.Execute(ctx, basicDriverRequest(driver, []string{"/bin/sh", "-c", "exit 0"}), driverDiscardStreams())
				if status != (sandbox.ExitStatus{}) || !errors.Is(executeErr, cancellation.want) || executeErr.Error() != cancellation.want.Error() {
					t.Fatalf("admission cancellation = (%+v, %v), want %v", status, executeErr, cancellation.want)
				}
				if state == "closed" && !errors.Is(executeErr, sandbox.ErrClosed) {
					t.Fatalf("closed identity missing from cancellation race: %v", executeErr)
				}
				if state == "poisoned" && !errors.Is(executeErr, sandbox.ErrUnavailable) {
					t.Fatalf("runtime-unavailable identity missing from cancellation race: %v", executeErr)
				}
			})
		}
	}
}

func (stage selfTestFixtureEventStage) String() string {
	switch stage {
	case selfTestFixtureBeforeDispatch:
		return "before-dispatch"
	case selfTestFixtureBeforeReadback:
		return "before-readback"
	case selfTestFixtureBeforeCleanup:
		return "before-cleanup"
	default:
		return "unknown"
	}
}

func assertFix2FileIdentityAndContents(t *testing.T, path string, before fs.FileInfo, want []byte) {
	t.Helper()
	after, err := os.Lstat(path)
	if err != nil || before == nil || !os.SameFile(before, after) || after.Mode().Perm() != selfTestFixtureMode {
		t.Fatalf("file identity %q changed: before=%v after=%v error=%v", path, before, after, err)
	}
	contents, err := os.ReadFile(path)
	if err != nil || !slices.Equal(contents, want) {
		t.Fatalf("file contents %q = %q, %v; want %q", path, contents, err, want)
	}
}

func writeFix2ExistingFixture(path string, contents []byte) error {
	fd, err := unix.Open(path, unix.O_WRONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return err
	}
	if err := unix.Ftruncate(fd, 0); err != nil {
		_ = unix.Close(fd)
		return err
	}
	for len(contents) > 0 {
		written, writeErr := unix.Write(fd, contents)
		if errors.Is(writeErr, unix.EINTR) {
			continue
		}
		if writeErr != nil || written <= 0 {
			_ = unix.Close(fd)
			if writeErr != nil {
				return writeErr
			}
			return io.ErrShortWrite
		}
		contents = contents[written:]
	}
	return unix.Close(fd)
}

func awaitFix2Classification[T any](t *testing.T, result <-chan T) T {
	t.Helper()
	select {
	case value := <-result:
		return value
	case <-time.After(driverTestDeadlockDeadline):
		t.Fatal("timed out waiting for infrastructure classification")
		var zero T
		return zero
	}
}
