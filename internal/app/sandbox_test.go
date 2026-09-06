package app

import (
	"context"
	"errors"
	"testing"

	"github.com/baiyuqing/otto/internal/agent"
)

func sandboxSeatbeltInfo(network SandboxNetwork) SandboxInfo {
	return SandboxInfo{Mode: SandboxSeatbelt, Network: network, BashAvailable: true}
}

// sandboxControlStub stands in for the process sandbox the composition root
// owns: Info reports whatever the last reload installed.
type sandboxControlStub struct {
	info  SandboxInfo
	err   error
	calls int
}

func (s *sandboxControlStub) Info() SandboxInfo { return s.info }

func (s *sandboxControlStub) Reload(context.Context) (SandboxInfo, error) {
	s.calls++
	if s.err != nil {
		return SandboxInfo{}, s.err
	}
	s.info = sandboxSeatbeltInfo(SandboxNetworkDenied)
	return s.info, nil
}

func newSandboxControlController(t *testing.T, control *sandboxControlStub) *Controller {
	t.Helper()
	controller, err := New(
		SessionReplacement{Session: &fakeSession{header: testHeader("current")}, Runner: runnerFunc(noopRun)},
		WithRuntimeInfo(RuntimeInfo{Provider: "openai-compatible", Sandbox: sandboxSeatbeltInfo(SandboxNetworkAllowed)}),
		WithSandboxControl(control.Info, control.Reload),
	)
	if err != nil {
		t.Fatal(err)
	}
	return controller
}

func TestControllerReloadSandboxReportsAndPublishesNewInfo(t *testing.T) {
	control := &sandboxControlStub{info: sandboxSeatbeltInfo(SandboxNetworkAllowed)}
	controller := newSandboxControlController(t, control)

	if got := controller.Info().Sandbox; got != sandboxSeatbeltInfo(SandboxNetworkAllowed) {
		t.Fatalf("Info().Sandbox before reload = %#v", got)
	}

	reloaded, err := controller.ReloadSandbox(context.Background())
	if err != nil {
		t.Fatalf("ReloadSandbox() = %v", err)
	}
	want := sandboxSeatbeltInfo(SandboxNetworkDenied)
	if reloaded != want {
		t.Fatalf("ReloadSandbox() info = %#v, want %#v", reloaded, want)
	}
	if got := controller.Info().Sandbox; got != want {
		t.Fatalf("Info().Sandbox after reload = %#v, want %#v", got, want)
	}
}

// A reload replaces the executor the running bash tool holds, so it must not
// start while a turn owns that executor.
func TestControllerReloadSandboxRejectsActivePrompt(t *testing.T) {
	control := &sandboxControlStub{info: sandboxSeatbeltInfo(SandboxNetworkAllowed)}
	controller := newSandboxControlController(t, control)

	started := make(chan struct{})
	release := make(chan struct{})
	controller.runner = runnerFunc(func(context.Context, string, func(agent.Event)) error {
		close(started)
		<-release
		return nil
	})

	done := make(chan error, 1)
	go func() { done <- controller.Prompt(context.Background(), "busy", func(agent.Event) {}) }()
	<-started
	if _, err := controller.ReloadSandbox(context.Background()); !errors.Is(err, ErrPromptActive) {
		t.Fatalf("ReloadSandbox() error = %v, want ErrPromptActive", err)
	}
	if control.calls != 0 {
		t.Fatalf("reload calls = %d, want 0", control.calls)
	}
	close(release)
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func TestControllerReloadSandboxWithoutCapabilityIsUnavailable(t *testing.T) {
	controller, err := New(SessionReplacement{Session: &fakeSession{header: testHeader("current")}, Runner: runnerFunc(noopRun)})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := controller.ReloadSandbox(context.Background()); !errors.Is(err, ErrSandboxReloadUnavailable) {
		t.Fatalf("ReloadSandbox() error = %v, want ErrSandboxReloadUnavailable", err)
	}
}

func TestControllerReloadSandboxAfterCloseIsRejected(t *testing.T) {
	control := &sandboxControlStub{info: sandboxSeatbeltInfo(SandboxNetworkAllowed)}
	controller := newSandboxControlController(t, control)
	if err := controller.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := controller.ReloadSandbox(context.Background()); !errors.Is(err, ErrClosed) {
		t.Fatalf("ReloadSandbox() error = %v, want ErrClosed", err)
	}
	if control.calls != 0 {
		t.Fatalf("reload calls = %d, want 0", control.calls)
	}
}

// A failed reload leaves the process sandbox untouched, so the reported info
// must still be the live one rather than a zero value.
func TestControllerReloadSandboxFailureKeepsReportingLiveInfo(t *testing.T) {
	control := &sandboxControlStub{info: sandboxSeatbeltInfo(SandboxNetworkAllowed), err: errors.New("reload failed")}
	controller := newSandboxControlController(t, control)

	if _, err := controller.ReloadSandbox(context.Background()); err == nil || err.Error() != "reload failed" {
		t.Fatalf("ReloadSandbox() error = %v, want reload failed", err)
	}
	if got := controller.Info().Sandbox; got != sandboxSeatbeltInfo(SandboxNetworkAllowed) {
		t.Fatalf("Info().Sandbox = %#v, want unchanged", got)
	}
}

func TestControllerImplementsSandboxReloader(t *testing.T) {
	control := &sandboxControlStub{info: sandboxSeatbeltInfo(SandboxNetworkAllowed)}
	var backend any = newSandboxControlController(t, control)
	if _, ok := backend.(SandboxReloader); !ok {
		t.Fatal("Controller does not implement SandboxReloader")
	}
}
