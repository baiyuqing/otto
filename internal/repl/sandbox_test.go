package repl

import (
	"bytes"
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
)

// fakeSandboxBackend adds the app.SandboxReloader capability to fakeBackend so
// /sandbox tests can drive a reload offline.
type fakeSandboxBackend struct {
	fakeBackend
	reload      func(context.Context) (app.SandboxInfo, error)
	reloadCalls int
}

func (f *fakeSandboxBackend) ReloadSandbox(ctx context.Context) (app.SandboxInfo, error) {
	f.reloadCalls++
	if f.reload == nil {
		return f.info.Sandbox, nil
	}
	return f.reload(ctx)
}

func seatbeltInfo(network app.SandboxNetwork) app.SandboxInfo {
	return app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: network, BashAvailable: true}
}

func TestREPLSandboxShowsCurrentStateWithoutReloading(t *testing.T) {
	backend := &fakeSandboxBackend{fakeBackend: fakeBackend{info: app.Info{Sandbox: seatbeltInfo(app.SandboxNetworkAllowed)}}}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/sandbox\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if out := stdout.String(); !strings.Contains(out, "Sandbox: seatbelt · workspace-write · network allowed") {
		t.Fatalf("output = %q, want the current sandbox summary", out)
	}
	if stderr.Len() != 0 {
		t.Fatalf("stderr = %q, want empty", stderr.String())
	}
	if backend.reloadCalls != 0 {
		t.Fatalf("reload calls = %d, want 0", backend.reloadCalls)
	}
}

func TestREPLSandboxReloadReportsNewState(t *testing.T) {
	backend := &fakeSandboxBackend{fakeBackend: fakeBackend{info: app.Info{Sandbox: seatbeltInfo(app.SandboxNetworkAllowed)}}}
	backend.reload = func(context.Context) (app.SandboxInfo, error) {
		backend.info.Sandbox = seatbeltInfo(app.SandboxNetworkDenied)
		return backend.info.Sandbox, nil
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/sandbox reload\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if backend.reloadCalls != 1 {
		t.Fatalf("reload calls = %d, want 1", backend.reloadCalls)
	}
	if out := stdout.String(); !strings.Contains(out, "network denied") {
		t.Fatalf("output = %q, want the reloaded sandbox summary", out)
	}
}

func TestREPLSandboxReloadReportsFailure(t *testing.T) {
	backend := &fakeSandboxBackend{fakeBackend: fakeBackend{info: app.Info{Sandbox: seatbeltInfo(app.SandboxNetworkAllowed)}}}
	backend.reload = func(context.Context) (app.SandboxInfo, error) {
		return app.SandboxInfo{}, errors.New("sandbox reload failed: self-test-failed")
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/sandbox reload\n"), &stdout, &stderr, backend)
	err := r.Run(context.Background())
	if err == nil || !strings.Contains(err.Error(), "self-test-failed") {
		t.Fatalf("Run() error = %v, want the reload failure", err)
	}
	if !IsCommandError(err, "/sandbox") {
		t.Fatalf("Run() error = %v, want /sandbox command error", err)
	}
}

func TestREPLSandboxReloadWithoutCapabilityIsReported(t *testing.T) {
	backend := &fakeBackend{info: app.Info{Sandbox: seatbeltInfo(app.SandboxNetworkAllowed)}}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/sandbox reload\n"), &stdout, &stderr, backend)
	err := r.Run(context.Background())
	if !errors.Is(err, app.ErrSandboxReloadUnavailable) {
		t.Fatalf("Run() error = %v, want ErrSandboxReloadUnavailable", err)
	}
}

func TestREPLSandboxRejectsUnknownSubcommand(t *testing.T) {
	backend := &fakeSandboxBackend{fakeBackend: fakeBackend{info: app.Info{Sandbox: seatbeltInfo(app.SandboxNetworkAllowed)}}}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/sandbox off\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stderr.String(), "unknown command: /sandbox off") {
		t.Fatalf("stderr = %q, want an unknown command report", stderr.String())
	}
	if backend.reloadCalls != 0 {
		t.Fatalf("reload calls = %d, want 0", backend.reloadCalls)
	}
}
