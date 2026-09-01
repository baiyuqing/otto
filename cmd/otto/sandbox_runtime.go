package main

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	goruntime "runtime"
	"strings"
	"sync"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/sandbox/direct"
	"github.com/baiyuqing/otto/internal/sandbox/seatbelt"
)

var errSandboxRuntimeClose = errors.New("sandbox runtime cleanup failed")

type sandboxOpenOptions struct {
	Settings      sandbox.Settings
	Workspace     string
	Shell         string
	Home          string
	HostEntries   []string
	ProviderNames []string
}

type sandboxRuntime struct {
	Executor        sandbox.CommandExecutor
	Environment     []string
	Info            app.SandboxInfo
	RedactionValues []string
	close           func() error
}

type sandboxSeatbeltDriver interface {
	sandbox.Driver
	PrivateDirectories() sandbox.PrivateDirectories
}

type sandboxExecutor interface {
	sandbox.CommandExecutor
	Close() error
}

type sandboxRuntimeDependencies struct {
	platform           string
	openSeatbelt       func(context.Context, seatbelt.Options) (sandboxSeatbeltDriver, error)
	newDirect          func() sandbox.Driver
	resolveEnvironment func(sandbox.EnvironmentOptions) (sandbox.EnvironmentSnapshot, error)
	newExecutor        func(sandbox.Driver, sandbox.Policy, string) (sandboxExecutor, error)
}

func productionSandboxRuntimeDependencies() sandboxRuntimeDependencies {
	return sandboxRuntimeDependencies{
		platform: goruntime.GOOS,
		openSeatbelt: func(ctx context.Context, options seatbelt.Options) (sandboxSeatbeltDriver, error) {
			return seatbelt.Open(ctx, options)
		},
		newDirect:          direct.New,
		resolveEnvironment: sandbox.ResolveEnvironment,
		newExecutor: func(driver sandbox.Driver, policy sandbox.Policy, workspace string) (sandboxExecutor, error) {
			return sandbox.NewExecutor(driver, policy, workspace)
		},
	}
}

func openSandboxRuntime(ctx context.Context, options sandboxOpenOptions) sandboxRuntime {
	return openSandboxRuntimeWithDependencies(ctx, options, productionSandboxRuntimeDependencies())
}

func openSandboxRuntimeWithDependencies(ctx context.Context, options sandboxOpenOptions, dependencies sandboxRuntimeDependencies) sandboxRuntime {
	options = cloneSandboxOpenOptions(options)
	if isNilSandboxRuntimeValue(ctx) || dependencies.resolveEnvironment == nil || dependencies.newExecutor == nil {
		return unavailableSandboxRuntime(app.SandboxReasonRuntimeFailure, nil)
	}

	switch options.Settings.Driver {
	case sandbox.DriverAuto, sandbox.DriverSeatbelt:
		if dependencies.platform != "darwin" {
			return unavailableSandboxRuntime(app.SandboxReasonUnsupportedPlatform, nil)
		}
		if dependencies.openSeatbelt == nil {
			return unavailableSandboxRuntime(app.SandboxReasonRuntimeFailure, nil)
		}
		return openSeatbeltSandboxRuntime(ctx, options, dependencies)
	case sandbox.DriverOff:
		if dependencies.newDirect == nil {
			return unavailableSandboxRuntime(app.SandboxReasonRuntimeFailure, nil)
		}
		return openDirectSandboxRuntime(options, dependencies)
	default:
		return unavailableSandboxRuntime(app.SandboxReasonPolicyUnsupported, nil)
	}
}

func openSeatbeltSandboxRuntime(ctx context.Context, options sandboxOpenOptions, dependencies sandboxRuntimeDependencies) sandboxRuntime {
	if options.Settings.Network != sandbox.NetworkAllow && options.Settings.Network != sandbox.NetworkDeny {
		return unavailableSandboxRuntime(app.SandboxReasonPolicyUnsupported, nil)
	}
	workspace, err := canonicalDirectory(options.Workspace)
	if err != nil {
		return unavailableSandboxRuntime(app.SandboxReasonPolicyUnsupported, nil)
	}
	shell, err := canonicalExecutableFile(options.Shell)
	if err != nil {
		return unavailableSandboxRuntime(app.SandboxReasonInvalidShell, nil)
	}

	driver, openErr := dependencies.openSeatbelt(ctx, seatbelt.Options{
		Workspace:   strings.Clone(workspace),
		Shell:       strings.Clone(shell),
		Home:        strings.Clone(options.Home),
		HostEntries: cloneSandboxRuntimeStrings(options.HostEntries),
		ReadPaths:   cloneSandboxRuntimeStrings(options.Settings.ReadPaths),
		Network:     options.Settings.Network,
	})
	if openErr != nil || isNilSandboxRuntimeValue(driver) {
		var cleanupErr error
		if !isNilSandboxRuntimeValue(driver) {
			cleanupErr = driver.Close()
		}
		return unavailableSandboxRuntimeAfterCleanup(sandboxOpenReason(openErr), nil, cleanupErr)
	}

	privateDirectories := driver.PrivateDirectories()
	environment, environmentErr := dependencies.resolveEnvironment(sandbox.EnvironmentOptions{
		HostEntries:        cloneSandboxRuntimeStrings(options.HostEntries),
		ProviderNames:      cloneSandboxRuntimeStrings(options.ProviderNames),
		AllowNames:         cloneSandboxRuntimeStrings(options.Settings.AllowEnv),
		PrivateDirectories: &privateDirectories,
	})
	environmentEntries := environment.Entries()
	if environmentErr != nil || environmentEntries == nil {
		cleanupErr := driver.Close()
		return unavailableSandboxRuntimeAfterCleanup(app.SandboxReasonEnvironmentRejected, environment.RedactionValues(), cleanupErr)
	}

	policy := sandbox.Policy{Filesystem: sandbox.FilesystemWorkspaceWrite, Network: options.Settings.Network}
	executor, executorErr := dependencies.newExecutor(driver, policy, workspace)
	if executorErr != nil || isNilSandboxRuntimeValue(executor) {
		var cleanupErr error
		if !isNilSandboxRuntimeValue(executor) {
			cleanupErr = executor.Close()
		} else {
			cleanupErr = driver.Close()
		}
		return unavailableSandboxRuntimeAfterCleanup(sandboxExecutorReason(executorErr), environment.RedactionValues(), cleanupErr)
	}

	return sandboxRuntime{
		Executor:        executor,
		Environment:     cloneSandboxRuntimeStrings(environmentEntries),
		Info:            seatbeltSandboxInfo(options.Settings.Network),
		RedactionValues: cloneSandboxRuntimeStrings(environment.RedactionValues()),
		close:           newSandboxRuntimeCloser(executor.Close),
	}
}

func openDirectSandboxRuntime(options sandboxOpenOptions, dependencies sandboxRuntimeDependencies) sandboxRuntime {
	workspace, err := canonicalDirectory(options.Workspace)
	if err != nil {
		return unavailableSandboxRuntime(app.SandboxReasonPolicyUnsupported, nil)
	}
	if _, err := canonicalExecutableFile(options.Shell); err != nil {
		return unavailableSandboxRuntime(app.SandboxReasonInvalidShell, nil)
	}

	environment, environmentErr := dependencies.resolveEnvironment(sandbox.EnvironmentOptions{
		HostEntries:   cloneSandboxRuntimeStrings(options.HostEntries),
		ProviderNames: cloneSandboxRuntimeStrings(options.ProviderNames),
		AllowNames:    cloneSandboxRuntimeStrings(options.Settings.AllowEnv),
	})
	environmentEntries := environment.Entries()
	if environmentErr != nil || environmentEntries == nil {
		return unavailableSandboxRuntime(app.SandboxReasonEnvironmentRejected, environment.RedactionValues())
	}

	driver := dependencies.newDirect()
	if isNilSandboxRuntimeValue(driver) {
		return unavailableSandboxRuntime(app.SandboxReasonRuntimeFailure, environment.RedactionValues())
	}
	policy := sandbox.Policy{Filesystem: sandbox.FilesystemUnconfined, Network: sandbox.NetworkAllow}
	executor, executorErr := dependencies.newExecutor(driver, policy, workspace)
	if executorErr != nil || isNilSandboxRuntimeValue(executor) {
		var cleanupErr error
		if !isNilSandboxRuntimeValue(executor) {
			cleanupErr = executor.Close()
		} else {
			cleanupErr = driver.Close()
		}
		return unavailableSandboxRuntimeAfterCleanup(sandboxExecutorReason(executorErr), environment.RedactionValues(), cleanupErr)
	}

	return sandboxRuntime{
		Executor:        executor,
		Environment:     cloneSandboxRuntimeStrings(environmentEntries),
		Info:            app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true, Reason: app.SandboxReasonNone},
		RedactionValues: cloneSandboxRuntimeStrings(environment.RedactionValues()),
		close:           newSandboxRuntimeCloser(executor.Close),
	}
}

func (r sandboxRuntime) Close() error {
	if r.close == nil {
		return nil
	}
	return r.close()
}

func unavailableSandboxRuntime(reason app.SandboxReason, redactions []string) sandboxRuntime {
	return unavailableSandboxRuntimeAfterCleanup(reason, redactions, nil)
}

func unavailableSandboxRuntimeAfterCleanup(reason app.SandboxReason, redactions []string, cleanupErr error) sandboxRuntime {
	var reportCleanup func() error
	if cleanupErr != nil {
		reportCleanup = func() error { return cleanupErr }
	}
	return sandboxRuntime{
		Info: app.SandboxInfo{
			Mode:          app.SandboxUnavailable,
			BashAvailable: false,
			Reason:        safeSandboxReason(reason),
		},
		RedactionValues: cloneSandboxRuntimeStrings(redactions),
		close:           newSandboxRuntimeCloser(reportCleanup),
	}
}

func seatbeltSandboxInfo(network sandbox.NetworkMode) app.SandboxInfo {
	info := app.SandboxInfo{Mode: app.SandboxSeatbelt, BashAvailable: true, Reason: app.SandboxReasonNone}
	switch network {
	case sandbox.NetworkAllow:
		info.Network = app.SandboxNetworkAllowed
	case sandbox.NetworkDeny:
		info.Network = app.SandboxNetworkDenied
	default:
		return app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonPolicyUnsupported}
	}
	return info
}

func sandboxOpenReason(err error) app.SandboxReason {
	var unavailable *sandbox.UnavailableError
	if errors.As(err, &unavailable) {
		return appSandboxReason(unavailable.Reason)
	}
	if errors.Is(err, sandbox.ErrUnsupportedPolicy) {
		return app.SandboxReasonPolicyUnsupported
	}
	return app.SandboxReasonRuntimeFailure
}

func sandboxExecutorReason(err error) app.SandboxReason {
	if errors.Is(err, sandbox.ErrUnsupportedPolicy) {
		return app.SandboxReasonPolicyUnsupported
	}
	return sandboxOpenReason(err)
}

func appSandboxReason(reason sandbox.UnavailableReason) app.SandboxReason {
	switch reason {
	case sandbox.ReasonUnsupportedPlatform:
		return app.SandboxReasonUnsupportedPlatform
	case sandbox.ReasonSeatbeltMissing:
		return app.SandboxReasonSeatbeltMissing
	case sandbox.ReasonSelfTestFailed:
		return app.SandboxReasonSelfTestFailed
	case sandbox.ReasonRuntimeFailure:
		return app.SandboxReasonRuntimeFailure
	case sandbox.ReasonInvalidShell:
		return app.SandboxReasonInvalidShell
	case sandbox.ReasonEnvironmentRejected:
		return app.SandboxReasonEnvironmentRejected
	case sandbox.ReasonPolicyUnsupported:
		return app.SandboxReasonPolicyUnsupported
	default:
		return app.SandboxReasonRuntimeFailure
	}
}

func safeSandboxReason(reason app.SandboxReason) app.SandboxReason {
	switch reason {
	case app.SandboxReasonUnsupportedPlatform,
		app.SandboxReasonSeatbeltMissing,
		app.SandboxReasonSelfTestFailed,
		app.SandboxReasonRuntimeFailure,
		app.SandboxReasonInvalidShell,
		app.SandboxReasonEnvironmentRejected,
		app.SandboxReasonPolicyUnsupported:
		return reason
	default:
		return app.SandboxReasonRuntimeFailure
	}
}

func canonicalExecutableFile(path string) (string, error) {
	if path == "" || strings.IndexByte(path, 0) >= 0 {
		return "", sandbox.ErrInvalidRequest
	}
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", sandbox.ErrInvalidRequest
	}
	canonical, err := filepath.EvalSymlinks(absolute)
	if err != nil {
		return "", sandbox.ErrInvalidRequest
	}
	canonical = filepath.Clean(canonical)
	info, err := os.Lstat(canonical)
	if err != nil || !info.Mode().IsRegular() || info.Mode()&os.ModeSymlink != 0 || info.Mode().Perm()&0o111 == 0 {
		return "", sandbox.ErrInvalidRequest
	}
	return canonical, nil
}

func cloneSandboxOpenOptions(options sandboxOpenOptions) sandboxOpenOptions {
	options.Settings = options.Settings.Clone()
	options.Workspace = strings.Clone(options.Workspace)
	options.Shell = strings.Clone(options.Shell)
	options.Home = strings.Clone(options.Home)
	options.HostEntries = cloneSandboxRuntimeStrings(options.HostEntries)
	options.ProviderNames = cloneSandboxRuntimeStrings(options.ProviderNames)
	return options
}

func cloneSandboxRuntimeStrings(values []string) []string {
	if values == nil {
		return nil
	}
	cloned := make([]string, len(values))
	for index, value := range values {
		cloned[index] = strings.Clone(value)
	}
	return cloned
}

func newSandboxRuntimeCloser(closeResource func() error) func() error {
	var once sync.Once
	done := make(chan struct{})
	var result error
	return func() error {
		once.Do(func() {
			if closeResource != nil {
				if err := closeResource(); err != nil {
					result = errSandboxRuntimeClose
				}
			}
			close(done)
		})
		<-done
		return result
	}
}

func isNilSandboxRuntimeValue(value any) bool {
	if value == nil {
		return true
	}
	reflected := reflect.ValueOf(value)
	switch reflected.Kind() {
	case reflect.Chan, reflect.Func, reflect.Interface, reflect.Map, reflect.Pointer, reflect.Slice:
		return reflected.IsNil()
	default:
		return false
	}
}
