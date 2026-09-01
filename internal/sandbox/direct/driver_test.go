//go:build darwin || linux

package direct

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
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/sandbox/sandboxtest"
)

func TestDirectIdentityCapabilitiesAndPolicyAuthority(t *testing.T) {
	driver := New()
	if driver.ID() != ID || ID != "direct" {
		t.Fatalf("Driver ID = %q, want direct", driver.ID())
	}
	wantCapabilities := sandbox.Capabilities{NetworkAllow: true}
	if got := driver.Capabilities(); got != wantCapabilities {
		t.Fatalf("Capabilities() = %+v, want %+v", got, wantCapabilities)
	}
	if err := driver.Close(); err != nil {
		t.Fatalf("Close() error = %v", err)
	}

	workspace := canonicalTempDir(t)
	accepted := New()
	executor, err := sandbox.NewExecutor(accepted, sandbox.Policy{
		Filesystem: sandbox.FilesystemUnconfined,
		Network:    sandbox.NetworkAllow,
	}, workspace)
	if err != nil {
		_ = accepted.Close()
		t.Fatalf("NewExecutor(explicit unconfined allow) error = %v", err)
	}
	if err := executor.Close(); err != nil {
		t.Fatalf("Executor.Close() error = %v", err)
	}

	for _, policy := range []sandbox.Policy{
		{Filesystem: sandbox.FilesystemWorkspaceWrite, Network: sandbox.NetworkAllow},
		{Filesystem: sandbox.FilesystemWorkspaceWrite, Network: sandbox.NetworkDeny},
		{Filesystem: sandbox.FilesystemUnconfined, Network: sandbox.NetworkDeny},
	} {
		driver := New()
		executor, err := sandbox.NewExecutor(driver, policy, workspace)
		if !errors.Is(err, sandbox.ErrUnsupportedPolicy) || executor != nil {
			_ = driver.Close()
			t.Fatalf("NewExecutor(policy=%+v) = (%v, %v), want ErrUnsupportedPolicy", policy, executor, err)
		}
		if err := driver.Close(); err != nil {
			t.Fatalf("rejected Driver.Close() error = %v", err)
		}
	}
}

func TestDirectDriverContract(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	executable, err = filepath.EvalSymlinks(executable)
	if err != nil {
		t.Fatal(err)
	}

	sandboxtest.RunDriverContract(t, sandboxtest.Case{
		NewDriver: func(testing.TB, sandboxtest.Fixture) sandbox.Driver {
			return New()
		},
		Request: func(t testing.TB, fixture sandboxtest.Fixture, argv []string) sandbox.Request {
			t.Helper()
			environment := append([]string{}, fixture.Environment...)
			environment = append(environment, "OTTO_DIRECT_HELPER_PROCESS=1")
			return sandbox.Request{
				Argv: slices.Clone(argv),
				Dir:  fixture.Workspace,
				Env:  environment,
			}
		},
		ShellCommand: func(t testing.TB, script string) []string {
			t.Helper()
			return []string{"/bin/sh", "-c", script}
		},
		TCPClient: func(t testing.TB, address string) []string {
			t.Helper()
			return []string{executable, "-test.run=^TestDirectHelperProcess$", "--", "tcp", address}
		},
		UnixClient: func(t testing.TB, address string) []string {
			t.Helper()
			return []string{executable, "-test.run=^TestDirectHelperProcess$", "--", "unix", address}
		},
	})
}

func TestDirectMapsSignalsAndSanitizesLaunchErrors(t *testing.T) {
	workspace := canonicalTempDir(t)
	driver := New()
	executor, err := sandbox.NewExecutor(driver, sandbox.Policy{
		Filesystem: sandbox.FilesystemUnconfined,
		Network:    sandbox.NetworkAllow,
	}, workspace)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = executor.Close() })

	status, err := executor.Execute(context.Background(), sandbox.Request{
		Argv: []string{"/bin/sh", "-c", "kill -TERM $$"},
		Dir:  workspace,
		Env:  []string{"PATH=/usr/bin:/bin", "LC_ALL=C"},
	}, sandbox.Streams{Stdout: io.Discard, Stderr: io.Discard})
	if err != nil {
		t.Fatalf("signal Execute() error = %v", err)
	}
	if status.Code != -1 || !status.Signaled || status.Signal == "" {
		t.Fatalf("signal status = %+v", status)
	}

	sensitivePath := filepath.Join(workspace, "missing executable with secret")
	_, err = executor.Execute(context.Background(), sandbox.Request{
		Argv: []string{sensitivePath, "secret-argument-value"},
		Dir:  workspace,
		Env:  []string{"SECRET_VALUE=secret-environment-value"},
	}, sandbox.Streams{Stdout: &bytes.Buffer{}, Stderr: &bytes.Buffer{}})
	if !errors.Is(err, sandbox.ErrChildLaunch) || err.Error() != sandbox.ErrChildLaunch.Error() {
		t.Fatalf("missing executable error = %v, want fixed ErrChildLaunch", err)
	}
	for _, secret := range []string{sensitivePath, "secret-argument-value", "secret-environment-value"} {
		if strings.Contains(err.Error(), secret) {
			t.Fatalf("launch error exposed request data: %v", err)
		}
	}
}

func TestDirectConcurrentExecuteAndClose(t *testing.T) {
	workspace := canonicalTempDir(t)
	driver := New()
	request := sandbox.Request{
		Argv: []string{"/bin/sh", "-c", "exit 0"},
		Dir:  workspace,
		Env:  []string{"PATH=/usr/bin:/bin"},
	}
	streams := sandbox.Streams{Stdout: io.Discard, Stderr: io.Discard}

	const executeCallers = 20
	const closeCallers = 12
	start := make(chan struct{})
	executeResults := make(chan error, executeCallers)
	closeResults := make(chan error, closeCallers)
	for range executeCallers {
		go func() {
			<-start
			_, err := driver.Execute(context.Background(), request, streams)
			executeResults <- err
		}()
	}
	for range closeCallers {
		go func() {
			<-start
			closeResults <- driver.Close()
		}()
	}
	close(start)

	for range executeCallers {
		select {
		case err := <-executeResults:
			if err != nil && !errors.Is(err, sandbox.ErrClosed) {
				t.Fatalf("Execute() error = %v", err)
			}
		case <-time.After(10 * time.Second):
			t.Fatal("timed out waiting for concurrent Execute")
		}
	}
	for range closeCallers {
		select {
		case err := <-closeResults:
			if err != nil {
				t.Fatalf("Close() error = %v", err)
			}
		case <-time.After(10 * time.Second):
			t.Fatal("timed out waiting for concurrent Close")
		}
	}
	if _, err := driver.Execute(context.Background(), request, streams); !errors.Is(err, sandbox.ErrClosed) {
		t.Fatalf("Execute() after Close error = %v, want ErrClosed", err)
	}
}

func TestDirectDoesNotRetainRequestOrWriters(t *testing.T) {
	workspace := canonicalTempDir(t)
	driver := New()
	t.Cleanup(func() { _ = driver.Close() })
	request := sandbox.Request{
		Argv: []string{"/bin/sh", "-c", `printf '%s' "$DIRECT_VALUE"`},
		Dir:  workspace,
		Env:  []string{"DIRECT_VALUE=original"},
	}
	var stdout bytes.Buffer
	streams := sandbox.Streams{Stdout: &stdout, Stderr: io.Discard}
	status, err := driver.Execute(context.Background(), request, streams)
	if err != nil || status.Code != 0 {
		t.Fatalf("Execute() = (%+v, %v)", status, err)
	}
	if stdout.String() != "original" {
		t.Fatalf("stdout = %q", stdout.String())
	}

	originalArgv := slices.Clone(request.Argv)
	originalEnv := slices.Clone(request.Env)
	request.Argv[2] = "exit 91"
	request.Env[0] = "DIRECT_VALUE=mutated"
	if reflect.DeepEqual(request.Argv, originalArgv) || reflect.DeepEqual(request.Env, originalEnv) {
		t.Fatal("test mutation did not change request backing slices")
	}
	if stdout.String() != "original" {
		t.Fatal("completed Execute retained and changed writer output")
	}
}

func TestDirectHelperProcess(t *testing.T) {
	if os.Getenv("OTTO_DIRECT_HELPER_PROCESS") != "1" {
		return
	}
	separator := slices.Index(os.Args, "--")
	if separator < 0 || len(os.Args) != separator+3 {
		os.Exit(64)
	}
	network, address := os.Args[separator+1], os.Args[separator+2]
	dialer := net.Dialer{Timeout: 5 * time.Second}
	connection, err := dialer.Dial(network, address)
	if err != nil {
		os.Exit(65)
	}
	if _, err := connection.Write([]byte("contract")); err != nil {
		_ = connection.Close()
		os.Exit(66)
	}
	if err := connection.Close(); err != nil {
		os.Exit(67)
	}
	os.Exit(0)
}

func canonicalTempDir(t *testing.T) string {
	t.Helper()
	directory, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	return directory
}
