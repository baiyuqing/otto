package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"slices"
	"sync"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/sandbox"
)

var (
	errSandboxExecutionUnavailable = errors.New("sandbox execution unavailable")

	// A reload keeps the current session, runner, and bash tool in place, so
	// it can only replace a runtime that is already usable and whose
	// environment snapshot and redaction values stay identical. Changes that
	// move either of those still require a restart, and say so.
	errSandboxReloadUnavailable = errors.New("sandbox reload requires a usable sandbox; restart otto")
	errSandboxReloadEnvironment = errors.New("sandbox reload cannot apply allow_env changes; restart otto")
	errSandboxReloadFailed      = errors.New("sandbox reload failed")
)

// resolveSandboxSettings resolves the [sandbox] table together with the skill
// and agent roots that must stay readable. Startup and /sandbox reload share
// it so a reload grants exactly the roots startup would have granted for the
// same configuration.
func resolveSandboxSettings(file config.File, environment map[string]string, workspacePath string, driverOverride *string) (sandbox.Settings, error) {
	settings := file.Sandbox
	settings.ReadPaths = append([]string(nil), settings.ReadPaths...)
	agents, err := config.ResolveAgents(file, environment, workspacePath)
	if err != nil {
		return sandbox.Settings{}, err
	}
	roots := append(config.ResolveSkills(file, environment, workspacePath).Roots, agents.Roots...)
	for _, root := range roots {
		if info, err := os.Stat(root); err == nil && info.IsDir() {
			settings.ReadPaths = append(settings.ReadPaths, root)
		}
	}
	return config.ResolveSandbox(settings, driverOverride)
}

// sandboxReloader re-reads the configuration file and replaces the process
// sandbox with the result. Everything except the [sandbox] table is fixed at
// startup: the workspace, shell, home, host environment, and the provider key
// name the sandbox environment was resolved from.
type sandboxReloader struct {
	control        *sandboxSwitch
	loadConfig     func() (config.File, error)
	openSandbox    func(context.Context, sandboxOpenOptions) sandboxRuntime
	driverOverride *string
	environment    map[string]string
	workspace      string
	shell          string
	home           string
	hostEntries    []string
	apiKeyEnv      string
}

func (r *sandboxReloader) Reload(ctx context.Context) (app.SandboxInfo, error) {
	file, err := r.loadConfig()
	if err != nil {
		return app.SandboxInfo{}, err
	}
	settings, err := resolveSandboxSettings(file, r.environment, r.workspace, r.driverOverride)
	if err != nil {
		return app.SandboxInfo{}, err
	}
	return r.control.reload(normalizeSandboxRuntime(r.openSandbox(ctx, sandboxOpenOptions{
		Settings:      settings,
		Workspace:     r.workspace,
		Shell:         r.shell,
		Home:          r.home,
		HostEntries:   cloneSandboxRuntimeStrings(r.hostEntries),
		ProviderNames: sandboxProviderEnvironmentNames(file, r.apiKeyEnv),
	})))
}

// sandboxSwitch owns the process sandbox runtime behind a stable
// sandbox.CommandExecutor. The bash tool captures that executor when a runner
// is built, so replacing the runtime here applies new sandbox configuration
// without rebuilding the session, the runner, or the tool set.
//
// Reload and Execute are safe for concurrent use. Execute holds only a read
// lock for the duration of the command, so a reload that arrives while a
// command runs waits for it rather than closing the executor underneath it.
type sandboxSwitch struct {
	mu      sync.RWMutex
	current sandboxRuntime
	closed  bool
}

func newSandboxSwitch(runtime sandboxRuntime) *sandboxSwitch {
	return &sandboxSwitch{current: runtime}
}

func (s *sandboxSwitch) Execute(ctx context.Context, request sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	if s.closed || isNilSandboxRuntimeValue(s.current.Executor) {
		return sandbox.ExitStatus{}, errSandboxExecutionUnavailable
	}
	return s.current.Executor.Execute(ctx, request, streams)
}

func (s *sandboxSwitch) Info() app.SandboxInfo {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.current.Info
}

func (s *sandboxSwitch) Environment() []string {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return cloneSandboxRuntimeStrings(s.current.Environment)
}

// reload installs next when it can replace the current runtime in place. The
// rejected runtime is always closed, and the current one is left untouched, so
// a failed reload leaves bash working exactly as it did before.
func (s *sandboxSwitch) reload(next sandboxRuntime) (app.SandboxInfo, error) {
	s.mu.Lock()
	previous, err := s.replaceLocked(next)
	info := s.current.Info
	s.mu.Unlock()

	if err != nil {
		return app.SandboxInfo{}, errors.Join(err, next.Close())
	}
	if closeErr := previous.Close(); closeErr != nil {
		return info, closeErr
	}
	return info, nil
}

// replaceLocked validates next against the current runtime and swaps it in.
// It returns the runtime the caller must close after releasing the lock.
func (s *sandboxSwitch) replaceLocked(next sandboxRuntime) (sandboxRuntime, error) {
	switch {
	case s.closed || !usableSandboxRuntime(s.current):
		return sandboxRuntime{}, errSandboxReloadUnavailable
	case !usableSandboxRuntime(next):
		return sandboxRuntime{}, fmt.Errorf("%w: %s", errSandboxReloadFailed, sandboxReloadReason(next.Info))
	case !slices.Equal(next.Environment, s.current.Environment),
		!slices.Equal(next.RedactionValues, s.current.RedactionValues):
		return sandboxRuntime{}, errSandboxReloadEnvironment
	}
	previous := s.current
	s.current = next
	return previous, nil
}

func (s *sandboxSwitch) Close() error {
	s.mu.Lock()
	if s.closed {
		s.mu.Unlock()
		return nil
	}
	s.closed = true
	current := s.current
	s.mu.Unlock()
	return current.Close()
}

func usableSandboxRuntime(runtime sandboxRuntime) bool {
	return runtime.Info.BashAvailable &&
		!isNilSandboxRuntimeValue(runtime.Executor) &&
		runtime.Environment != nil &&
		runtime.RedactionsComplete
}

// sandboxReloadReason names why a replacement runtime is unusable. An
// otherwise-available runtime only reaches here with incomplete redactions,
// which the reason codes report as a runtime failure.
func sandboxReloadReason(info app.SandboxInfo) string {
	if code := info.ReasonCode(); code != "" {
		return code
	}
	return string(app.SandboxReasonRuntimeFailure)
}
