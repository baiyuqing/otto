package tui

import (
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
)

type sandboxTUIBackend struct {
	*fakeBackend
	reload      func(context.Context) (app.SandboxInfo, error)
	reloadCalls int
}

func (b *sandboxTUIBackend) ReloadSandbox(ctx context.Context) (app.SandboxInfo, error) {
	b.reloadCalls++
	if b.reload == nil {
		return b.info.Sandbox, nil
	}
	return b.reload(ctx)
}

func seatbeltTUIInfo(network app.SandboxNetwork) app.SandboxInfo {
	return app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: network, BashAvailable: true}
}

func newSandboxTUIBackend(network app.SandboxNetwork) *sandboxTUIBackend {
	return &sandboxTUIBackend{fakeBackend: &fakeBackend{info: app.Info{Sandbox: seatbeltTUIInfo(network)}}}
}

func TestSandboxCommandShowsCurrentStateWithoutReloading(t *testing.T) {
	backend := newSandboxTUIBackend(app.SandboxNetworkAllowed)
	got, cmd := submitCommand(t, newAuthModel(t, backend), "/sandbox")
	if cmd != nil {
		t.Fatalf("cmd = %v, want nil", cmd)
	}
	if content := strings.Join(got.pendingPrints, "\n"); !strings.Contains(content, "network allowed") {
		t.Fatalf("transcript = %q, want the current sandbox summary", content)
	}
	if backend.reloadCalls != 0 {
		t.Fatalf("reload calls = %d, want 0", backend.reloadCalls)
	}
}

func TestSandboxReloadCommandReportsNewState(t *testing.T) {
	backend := newSandboxTUIBackend(app.SandboxNetworkAllowed)
	backend.reload = func(context.Context) (app.SandboxInfo, error) {
		backend.info.Sandbox = seatbeltTUIInfo(app.SandboxNetworkDenied)
		return backend.info.Sandbox, nil
	}
	got, _ := submitCommand(t, newAuthModel(t, backend), "/sandbox reload")
	if backend.reloadCalls != 1 {
		t.Fatalf("reload calls = %d, want 1", backend.reloadCalls)
	}
	if content := strings.Join(got.pendingPrints, "\n"); !strings.Contains(content, "network denied") {
		t.Fatalf("transcript = %q, want the reloaded sandbox summary", content)
	}
}

func TestSandboxReloadCommandReportsFailure(t *testing.T) {
	backend := newSandboxTUIBackend(app.SandboxNetworkAllowed)
	backend.reload = func(context.Context) (app.SandboxInfo, error) {
		return app.SandboxInfo{}, errors.New("sandbox reload failed: self-test-failed")
	}
	got, _ := submitCommand(t, newAuthModel(t, backend), "/sandbox reload")
	if !strings.Contains(got.statusText, "self-test-failed") {
		t.Fatalf("statusText = %q, want the reload failure", got.statusText)
	}
}

// A reload replaces the executor a running bash command is using, so the TUI
// refuses it while a turn is in flight instead of blocking on the swap.
func TestSandboxReloadCommandRejectedWhileRunning(t *testing.T) {
	backend := newSandboxTUIBackend(app.SandboxNetworkAllowed)
	m := newAuthModel(t, backend)
	m.running = true
	got, _ := submitCommand(t, m, "/sandbox reload")
	if got.statusText != app.ErrPromptActive.Error() {
		t.Fatalf("statusText = %q, want %q", got.statusText, app.ErrPromptActive.Error())
	}
	if backend.reloadCalls != 0 {
		t.Fatalf("reload calls = %d, want 0", backend.reloadCalls)
	}
}

func TestSandboxReloadCommandWithoutCapabilityIsReported(t *testing.T) {
	backend := &fakeBackend{info: app.Info{Sandbox: seatbeltTUIInfo(app.SandboxNetworkAllowed)}}
	got, _ := submitCommand(t, newAuthModel(t, backend), "/sandbox reload")
	if content := strings.Join(got.pendingPrints, "\n"); !strings.Contains(content, app.ErrSandboxReloadUnavailable.Error()) {
		t.Fatalf("transcript = %q, want the unavailable report", content)
	}
}

func TestSandboxCommandRejectsUnknownArgument(t *testing.T) {
	backend := newSandboxTUIBackend(app.SandboxNetworkAllowed)
	got, _ := submitCommand(t, newAuthModel(t, backend), "/sandbox off")
	if !strings.Contains(got.statusText, "/sandbox") {
		t.Fatalf("statusText = %q, want a usage report", got.statusText)
	}
	if backend.reloadCalls != 0 {
		t.Fatalf("reload calls = %d, want 0", backend.reloadCalls)
	}
}

func TestSandboxCommandCompletesAndAppearsInHelp(t *testing.T) {
	m := typeEditorText(t, resizeModel(t, newTestModel(t), 80, 20), "/sand")
	found := false
	for _, suggestion := range m.commandSuggestions() {
		if suggestion.Name == "/sandbox" {
			found = true
		}
	}
	if !found {
		t.Fatalf("/sand suggestions = %#v", m.commandSuggestions())
	}

	updated, _ := resizeModel(t, newTestModel(t), 80, 24).Update(showHelpOverlayMsg{})
	if content := updated.(Model).View().Content; !strings.Contains(content, "/sandbox") {
		t.Fatalf("help overlay = %q", content)
	}
}
