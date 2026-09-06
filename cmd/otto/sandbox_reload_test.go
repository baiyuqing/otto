package main

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/sandbox"
)

type sandboxReloadFakeExecutor struct {
	id    string
	calls int
}

func (e *sandboxReloadFakeExecutor) Execute(_ context.Context, _ sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
	e.calls++
	if streams.Stdout != nil {
		_, _ = streams.Stdout.Write([]byte(e.id))
	}
	return sandbox.ExitStatus{Code: 0}, nil
}

func sandboxReloadRuntime(executor sandbox.CommandExecutor, environment []string, info app.SandboxInfo, closed *bool) sandboxRuntime {
	return sandboxRuntime{
		Executor:           executor,
		Environment:        environment,
		Info:               info,
		RedactionValues:    []string{"secret"},
		RedactionsComplete: true,
		close: func() error {
			*closed = true
			return nil
		},
	}
}

func sandboxReloadSeatbeltInfo(network app.SandboxNetwork) app.SandboxInfo {
	return app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: network, BashAvailable: true, Reason: app.SandboxReasonNone}
}

func sandboxReloadUnavailableRuntime(reason app.SandboxReason, closed *bool) sandboxRuntime {
	return sandboxRuntime{
		Info: app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: reason},
		close: func() error {
			*closed = true
			return nil
		},
	}
}

func TestSandboxSwitchExecutesThroughReplacedRuntime(t *testing.T) {
	before, after := &sandboxReloadFakeExecutor{id: "before"}, &sandboxReloadFakeExecutor{id: "after"}
	environment := []string{"HOME=/tmp", "PATH=/usr/bin"}
	beforeClosed, afterClosed := false, false
	control := newSandboxSwitch(sandboxReloadRuntime(before, environment, sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), &beforeClosed))

	if _, err := control.Execute(context.Background(), sandbox.Request{Argv: []string{"true"}}, sandbox.Streams{}); err != nil {
		t.Fatalf("Execute before reload: %v", err)
	}

	next := sandboxReloadRuntime(after, environment, sandboxReloadSeatbeltInfo(app.SandboxNetworkDenied), &afterClosed)
	info, err := control.reload(next)
	if err != nil {
		t.Fatalf("reload: %v", err)
	}
	if info != sandboxReloadSeatbeltInfo(app.SandboxNetworkDenied) || control.Info() != info {
		t.Fatalf("Info = %#v (switch %#v), want the replacement info", info, control.Info())
	}
	if !beforeClosed || afterClosed {
		t.Fatalf("closed = before %v after %v, want the previous runtime closed only", beforeClosed, afterClosed)
	}

	if _, err := control.Execute(context.Background(), sandbox.Request{Argv: []string{"true"}}, sandbox.Streams{}); err != nil {
		t.Fatalf("Execute after reload: %v", err)
	}
	if before.calls != 1 || after.calls != 1 {
		t.Fatalf("executor calls = before %d after %d, want 1 and 1", before.calls, after.calls)
	}
}

func TestSandboxSwitchReloadRejectsEnvironmentChange(t *testing.T) {
	before, after := &sandboxReloadFakeExecutor{id: "before"}, &sandboxReloadFakeExecutor{id: "after"}
	beforeClosed, afterClosed := false, false
	info := sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed)
	control := newSandboxSwitch(sandboxReloadRuntime(before, []string{"HOME=/tmp"}, info, &beforeClosed))

	next := sandboxReloadRuntime(after, []string{"HOME=/tmp", "GH_TOKEN=x"}, info, &afterClosed)
	if _, err := control.reload(next); !errors.Is(err, errSandboxReloadEnvironment) {
		t.Fatalf("reload error = %v, want errSandboxReloadEnvironment", err)
	}
	if beforeClosed || !afterClosed {
		t.Fatalf("closed = before %v after %v, want the rejected runtime closed only", beforeClosed, afterClosed)
	}

	if _, err := control.Execute(context.Background(), sandbox.Request{Argv: []string{"true"}}, sandbox.Streams{}); err != nil {
		t.Fatalf("Execute after rejected reload: %v", err)
	}
	if before.calls != 1 || after.calls != 0 {
		t.Fatalf("executor calls = before %d after %d, want the previous runtime still serving", before.calls, after.calls)
	}
}

func TestSandboxSwitchReloadRejectsUnusableReplacement(t *testing.T) {
	before := &sandboxReloadFakeExecutor{id: "before"}
	beforeClosed, afterClosed := false, false
	control := newSandboxSwitch(sandboxReloadRuntime(before, []string{"HOME=/tmp"}, sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), &beforeClosed))

	next := sandboxReloadUnavailableRuntime(app.SandboxReasonSelfTestFailed, &afterClosed)
	_, err := control.reload(next)
	if !errors.Is(err, errSandboxReloadFailed) {
		t.Fatalf("reload error = %v, want errSandboxReloadFailed", err)
	}
	if err.Error() != "sandbox reload failed: self-test-failed" {
		t.Fatalf("reload error text = %q, want the reason code", err.Error())
	}
	if beforeClosed || !afterClosed {
		t.Fatalf("closed = before %v after %v, want the rejected runtime closed only", beforeClosed, afterClosed)
	}
	if control.Info() != sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed) {
		t.Fatalf("Info = %#v, want the previous runtime info", control.Info())
	}
}

func TestSandboxSwitchReloadRequiresUsableCurrentRuntime(t *testing.T) {
	currentClosed, nextClosed := false, false
	control := newSandboxSwitch(sandboxReloadUnavailableRuntime(app.SandboxReasonSeatbeltMissing, &currentClosed))

	next := sandboxReloadRuntime(&sandboxReloadFakeExecutor{id: "after"}, []string{"HOME=/tmp"}, sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), &nextClosed)
	if _, err := control.reload(next); !errors.Is(err, errSandboxReloadUnavailable) {
		t.Fatalf("reload error = %v, want errSandboxReloadUnavailable", err)
	}
	if currentClosed || !nextClosed {
		t.Fatalf("closed = current %v next %v, want the rejected runtime closed only", currentClosed, nextClosed)
	}
}

func TestSandboxSwitchReloadRejectsIncompleteRedactions(t *testing.T) {
	beforeClosed, afterClosed := false, false
	control := newSandboxSwitch(sandboxReloadRuntime(&sandboxReloadFakeExecutor{id: "before"}, []string{"HOME=/tmp"}, sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), &beforeClosed))

	next := sandboxReloadRuntime(&sandboxReloadFakeExecutor{id: "after"}, []string{"HOME=/tmp"}, sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), &afterClosed)
	next.RedactionsComplete = false
	if _, err := control.reload(next); !errors.Is(err, errSandboxReloadFailed) {
		t.Fatalf("reload error = %v, want errSandboxReloadFailed", err)
	}
	if beforeClosed || !afterClosed {
		t.Fatalf("closed = before %v after %v, want the rejected runtime closed only", beforeClosed, afterClosed)
	}
}

func TestSandboxSwitchReloadRejectsRedactionValueChange(t *testing.T) {
	beforeClosed, afterClosed := false, false
	control := newSandboxSwitch(sandboxReloadRuntime(&sandboxReloadFakeExecutor{id: "before"}, []string{"HOME=/tmp"}, sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), &beforeClosed))

	next := sandboxReloadRuntime(&sandboxReloadFakeExecutor{id: "after"}, []string{"HOME=/tmp"}, sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), &afterClosed)
	next.RedactionValues = []string{"secret", "another"}
	if _, err := control.reload(next); !errors.Is(err, errSandboxReloadEnvironment) {
		t.Fatalf("reload error = %v, want errSandboxReloadEnvironment", err)
	}
	if beforeClosed || !afterClosed {
		t.Fatalf("closed = before %v after %v, want the rejected runtime closed only", beforeClosed, afterClosed)
	}
}

func TestSandboxSwitchCloseClosesCurrentRuntimeOnce(t *testing.T) {
	closes := 0
	runtime := sandboxRuntime{
		Executor:           &sandboxReloadFakeExecutor{id: "current"},
		Environment:        []string{"HOME=/tmp"},
		Info:               sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed),
		RedactionsComplete: true,
		close: func() error {
			closes++
			return nil
		},
	}
	control := newSandboxSwitch(runtime)

	if err := control.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}
	if err := control.Close(); err != nil {
		t.Fatalf("second Close: %v", err)
	}
	if closes != 1 {
		t.Fatalf("close calls = %d, want 1", closes)
	}
}

func TestSandboxSwitchExecuteFailsWhenClosed(t *testing.T) {
	control := newSandboxSwitch(sandboxRuntime{
		Executor:           &sandboxReloadFakeExecutor{id: "current"},
		Environment:        []string{"HOME=/tmp"},
		Info:               sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed),
		RedactionsComplete: true,
	})
	if err := control.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}

	if _, err := control.Execute(context.Background(), sandbox.Request{Argv: []string{"true"}}, sandbox.Streams{}); err == nil {
		t.Fatal("Execute after Close = nil error, want failure")
	}
	if _, err := control.reload(sandboxReloadRuntime(&sandboxReloadFakeExecutor{id: "after"}, []string{"HOME=/tmp"}, sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), new(bool))); !errors.Is(err, errSandboxReloadUnavailable) {
		t.Fatalf("reload after Close = %v, want errSandboxReloadUnavailable", err)
	}
}

func TestSandboxReloaderAppliesUpdatedSandboxTable(t *testing.T) {
	dir := t.TempDir()
	readable := filepath.Join(dir, "extra")
	if err := os.MkdirAll(readable, 0o755); err != nil {
		t.Fatalf("create readable directory: %v", err)
	}
	configPath := filepath.Join(dir, "config.toml")
	if err := os.WriteFile(configPath, []byte("[sandbox]\nnetwork = 'deny'\nread_paths = ['"+readable+"']\n"), 0o600); err != nil {
		t.Fatalf("write config: %v", err)
	}

	environment := []string{"HOME=" + dir}
	previousClosed := false
	control := newSandboxSwitch(sandboxReloadRuntime(&sandboxReloadFakeExecutor{id: "before"}, environment, sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), &previousClosed))

	var captured sandboxOpenOptions
	reloader := &sandboxReloader{
		control:     control,
		loadConfig:  func() (config.File, error) { return config.LoadRequired(configPath) },
		workspace:   dir,
		shell:       "/bin/zsh",
		home:        dir,
		hostEntries: environment,
		openSandbox: func(_ context.Context, options sandboxOpenOptions) sandboxRuntime {
			captured = options
			return sandboxReloadRuntime(&sandboxReloadFakeExecutor{id: "after"}, environment, sandboxReloadSeatbeltInfo(app.SandboxNetworkDenied), new(bool))
		},
	}

	info, err := reloader.Reload(context.Background())
	if err != nil {
		t.Fatalf("Reload: %v", err)
	}
	if info != sandboxReloadSeatbeltInfo(app.SandboxNetworkDenied) || control.Info() != info {
		t.Fatalf("Info = %#v (switch %#v), want the reloaded network", info, control.Info())
	}
	if captured.Settings.Network != sandbox.NetworkDeny {
		t.Fatalf("Network = %v, want deny", captured.Settings.Network)
	}
	if !slices.Contains(captured.Settings.ReadPaths, readable) {
		t.Fatalf("ReadPaths = %v, want %s", captured.Settings.ReadPaths, readable)
	}
	if captured.Workspace != dir || captured.Shell != "/bin/zsh" || captured.Home != dir {
		t.Fatalf("open options = %#v, want the startup workspace, shell, and home", captured)
	}
	if !previousClosed {
		t.Fatal("previous runtime was not closed")
	}
}

func TestSandboxReloaderRejectsInvalidConfigurationWithoutOpeningSandbox(t *testing.T) {
	dir := t.TempDir()
	configPath := filepath.Join(dir, "config.toml")
	if err := os.WriteFile(configPath, []byte("[sandbox]\nnetwork = 'sometimes'\n"), 0o600); err != nil {
		t.Fatalf("write config: %v", err)
	}

	environment := []string{"HOME=" + dir}
	previousClosed := false
	control := newSandboxSwitch(sandboxReloadRuntime(&sandboxReloadFakeExecutor{id: "before"}, environment, sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), &previousClosed))

	opens := 0
	reloader := &sandboxReloader{
		control:     control,
		loadConfig:  func() (config.File, error) { return config.LoadRequired(configPath) },
		workspace:   dir,
		shell:       "/bin/zsh",
		home:        dir,
		hostEntries: environment,
		openSandbox: func(context.Context, sandboxOpenOptions) sandboxRuntime {
			opens++
			return sandboxRuntime{}
		},
	}

	if _, err := reloader.Reload(context.Background()); err == nil {
		t.Fatal("Reload = nil error, want an invalid configuration failure")
	}
	if opens != 0 {
		t.Fatalf("openSandbox calls = %d, want 0", opens)
	}
	if previousClosed || control.Info() != sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed) {
		t.Fatalf("sandbox changed after a rejected reload: closed %v info %#v", previousClosed, control.Info())
	}
}

// normalizeSandboxRuntime is the single place that decides a runtime cannot
// run bash, so startup and every later reload classify one identically.
func TestNormalizeSandboxRuntimeDisablesBashOnIncompleteRedactions(t *testing.T) {
	closed := false
	runtime := sandboxReloadRuntime(&sandboxReloadFakeExecutor{id: "current"}, []string{"HOME=/tmp"}, sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), &closed)
	runtime.RedactionsComplete = false

	normalized := normalizeSandboxRuntime(runtime)

	want := app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonEnvironmentRejected}
	if normalized.Info != want {
		t.Fatalf("Info = %#v, want %#v", normalized.Info, want)
	}
	if !isNilSandboxRuntimeValue(normalized.Executor) || normalized.Environment != nil {
		t.Fatalf("executor = %#v environment = %#v, want both cleared", normalized.Executor, normalized.Environment)
	}
}

func TestNormalizeSandboxRuntimeDisablesBashOnMissingExecutor(t *testing.T) {
	closed := false
	runtime := sandboxReloadRuntime(nil, []string{"HOME=/tmp"}, sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), &closed)

	normalized := normalizeSandboxRuntime(runtime)

	want := app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonRuntimeFailure}
	if normalized.Info != want {
		t.Fatalf("Info = %#v, want %#v", normalized.Info, want)
	}
	if normalized.Environment != nil {
		t.Fatalf("environment = %#v, want nil", normalized.Environment)
	}
}

func TestNormalizeSandboxRuntimeKeepsUsableRuntime(t *testing.T) {
	closed := false
	runtime := sandboxReloadRuntime(&sandboxReloadFakeExecutor{id: "current"}, []string{"HOME=/tmp"}, sandboxReloadSeatbeltInfo(app.SandboxNetworkDenied), &closed)

	if normalized := normalizeSandboxRuntime(runtime); !usableSandboxRuntime(normalized) {
		t.Fatalf("normalized = %#v, want usable", normalized)
	}
}

// The REPL/TUI controllers and otto serve share one process sandbox, so both
// take their reload from the same builder helper.
func TestRuntimeBuilderSandboxReloadFollowsEffectiveSandbox(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("active"))
	builder.commandExecutor = &sandboxReloadFakeExecutor{id: "current"}
	builder.sandboxEnvironment = []string{}
	if builder.sandboxReload() != nil {
		t.Fatal("sandboxReload() without a reloader is non-nil")
	}

	builder.sandboxReloader = &sandboxReloader{control: newSandboxSwitch(sandboxRuntime{Info: sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed), RedactionsComplete: true})}
	builder.sandboxInfo = app.SandboxInfo{Mode: app.SandboxUnavailable}
	if builder.sandboxReload() != nil {
		t.Fatal("sandboxReload() without a usable sandbox is non-nil")
	}

	builder.sandboxInfo = sandboxReloadSeatbeltInfo(app.SandboxNetworkAllowed)
	if builder.sandboxReload() == nil {
		t.Fatal("sandboxReload() with a usable sandbox is nil")
	}
}
