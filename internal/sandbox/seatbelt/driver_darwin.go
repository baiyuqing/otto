//go:build darwin

package seatbelt

import (
	"bytes"
	"context"
	"errors"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/sandbox/internal/nativeprocess"
)

const ID sandbox.DriverID = "seatbelt"

const (
	sandboxExecPath = "/usr/bin/sandbox-exec"

	selfTestTimeout     = 5 * time.Second
	selfTestOutputLimit = 8 * 1024
	stderrDecisionLimit = 4 * 1024
)

type Options struct {
	Workspace   string
	Shell       string
	Home        string
	HostEntries []string
	ReadPaths   []string
	Network     sandbox.NetworkMode
}

type Driver struct {
	workspace   string
	shell       string
	network     sandbox.NetworkMode
	state       *state
	profilePath string
	profile     []byte
	processes   *nativeprocess.Manager

	mu        sync.Mutex
	closed    bool
	poisoned  bool
	closeDone chan struct{}
	closeErr  error
}

type sandboxExecIdentity struct {
	canonical string
	mode      fs.FileMode
	uid       int
}

type selfTestProbeKind uint8

const (
	selfTestProfileStart selfTestProbeKind = iota + 1
	selfTestAllowedRead
	selfTestAllowedWrite
	selfTestDeniedRead
	selfTestDeniedWrite
)

func (kind selfTestProbeKind) String() string {
	switch kind {
	case selfTestProfileStart:
		return "profile-start"
	case selfTestAllowedRead:
		return "allowed-read"
	case selfTestAllowedWrite:
		return "allowed-write"
	case selfTestDeniedRead:
		return "denied-read"
	case selfTestDeniedWrite:
		return "denied-write"
	default:
		return "invalid-probe"
	}
}

type selfTestProbe struct {
	kind        selfTestProbeKind
	spec        nativeprocess.Spec
	directories sandbox.PrivateDirectories
}

type selfTestProbeResult struct {
	result       nativeprocess.Result
	leaderReaped bool
}

type selfTestProbeRunner func(context.Context, *nativeprocess.Manager, selfTestProbe) (selfTestProbeResult, error)

type driverDependencies struct {
	inspectSandboxExec func(string) (sandboxExecIdentity, error)
	userCacheDir       func() (string, error)
	runProbe           selfTestProbeRunner
}

func defaultDriverDependencies() driverDependencies {
	return driverDependencies{
		inspectSandboxExec: inspectProductionSandboxExec,
		userCacheDir:       os.UserCacheDir,
		runProbe: func(ctx context.Context, manager *nativeprocess.Manager, probe selfTestProbe) (selfTestProbeResult, error) {
			result, err := manager.Run(ctx, probe.spec)
			return selfTestProbeResult{result: result, leaderReaped: true}, err
		},
	}
}

func Open(ctx context.Context, options Options) (*Driver, error) {
	return openWithDependencies(ctx, options, defaultDriverDependencies())
}

func openWithDependencies(ctx context.Context, options Options, dependencies driverDependencies) (*Driver, error) {
	if isNilDriverValue(ctx) {
		return nil, sandbox.ErrInvalidRequest
	}
	if err := driverContextError(ctx); err != nil {
		return nil, err
	}
	if dependencies.inspectSandboxExec == nil || dependencies.userCacheDir == nil || dependencies.runProbe == nil {
		return nil, unavailable(sandbox.ReasonRuntimeFailure)
	}
	identity, err := dependencies.inspectSandboxExec(sandboxExecPath)
	if err != nil || !validSandboxExecIdentity(identity) {
		return nil, unavailable(sandbox.ReasonSeatbeltMissing)
	}

	workspace, ok := resolveDriverDirectory(options.Workspace)
	if !ok {
		return nil, unavailable(sandbox.ReasonPolicyUnsupported)
	}
	shell, err := resolveProfilePath(options.Shell)
	if err != nil || !validResolvedProfilePath(shell) || shell.kind != profilePathRegular || !shell.executable {
		return nil, unavailable(sandbox.ReasonInvalidShell)
	}
	home, ok := resolveDriverDirectory(options.Home)
	if !ok {
		return nil, unavailable(sandbox.ReasonPolicyUnsupported)
	}
	if options.Network != sandbox.NetworkAllow && options.Network != sandbox.NetworkDeny {
		return nil, unavailable(sandbox.ReasonPolicyUnsupported)
	}
	if err := driverContextError(ctx); err != nil {
		return nil, err
	}

	cacheBase, err := dependencies.userCacheDir()
	if err != nil {
		return nil, unavailable(sandbox.ReasonSelfTestFailed)
	}
	privateState, err := createState(workspace, cacheBase)
	if err != nil {
		return nil, unavailable(sandbox.ReasonSelfTestFailed)
	}
	stateOwned := true
	defer func() {
		if stateOwned {
			_ = privateState.close()
		}
	}()

	profile, err := generateProfile(profileOptions{
		Workspace:   workspace,
		Directories: privateState.directories,
		Shell:       shell.path,
		Home:        home,
		HostEntries: append([]string(nil), options.HostEntries...),
		ReadPaths:   append([]string(nil), options.ReadPaths...),
		Network:     options.Network,
	})
	if err != nil || privateState.writeProfile(profile) != nil {
		return nil, unavailable(sandbox.ReasonSelfTestFailed)
	}
	if err := driverContextError(ctx); err != nil {
		return nil, err
	}

	manager := nativeprocess.New()
	driver := &Driver{
		workspace:   workspace,
		shell:       shell.path,
		network:     options.Network,
		state:       privateState,
		profilePath: privateState.profilePath,
		profile:     append([]byte(nil), profile...),
		processes:   manager,
		closeDone:   make(chan struct{}),
	}
	if err := runStartupSelfTest(ctx, driver, dependencies.runProbe); err != nil {
		_ = manager.Close()
		_ = privateState.close()
		stateOwned = false
		if callerErr := driverContextError(ctx); callerErr != nil {
			return nil, callerErr
		}
		return nil, unavailable(sandbox.ReasonSelfTestFailed)
	}
	if err := driverContextError(ctx); err != nil {
		_ = manager.Close()
		_ = privateState.close()
		stateOwned = false
		return nil, err
	}
	stateOwned = false
	return driver, nil
}

func inspectProductionSandboxExec(path string) (sandboxExecIdentity, error) {
	if path != sandboxExecPath {
		return sandboxExecIdentity{}, fs.ErrInvalid
	}
	canonical, err := canonicalFilesystemPath(path)
	if err != nil {
		return sandboxExecIdentity{}, err
	}
	info, err := os.Lstat(path)
	if err != nil {
		return sandboxExecIdentity{}, err
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		return sandboxExecIdentity{}, fs.ErrInvalid
	}
	return sandboxExecIdentity{canonical: canonical, mode: info.Mode(), uid: int(stat.Uid)}, nil
}

func validSandboxExecIdentity(identity sandboxExecIdentity) bool {
	return identity.canonical == sandboxExecPath && identity.uid == 0 && identity.mode.IsRegular() &&
		identity.mode&os.ModeSymlink == 0 && identity.mode.Perm()&0o111 != 0
}

func resolveDriverDirectory(path string) (string, bool) {
	resolved, err := resolveProfilePath(path)
	return resolved.path, err == nil && validResolvedProfilePath(resolved) && resolved.kind == profilePathDirectory
}

func runStartupSelfTest(caller context.Context, driver *Driver, run selfTestProbeRunner) error {
	ctx, cancel := context.WithTimeout(caller, selfTestTimeout)
	defer cancel()

	const (
		allowedReadContents  = "otto-seatbelt-allowed-read"
		allowedWriteContents = "otto-seatbelt-allowed-write"
		deniedContents       = "otto-seatbelt-denied-fixture"
	)
	allowedRead := filepath.Join(driver.state.directories.Home, ".otto-self-test-read")
	allowedWorkspaceWrite := filepath.Join(driver.workspace, ".otto-self-test-write")
	allowedPrivateWrite := filepath.Join(driver.state.directories.Temp, ".otto-self-test-write")
	deniedRead := filepath.Join(driver.state.profiles, ".otto-self-test-denied-read")
	deniedWrite := filepath.Join(driver.state.profiles, ".otto-self-test-denied-write")
	fixtures := []string{allowedRead, allowedWorkspaceWrite, allowedPrivateWrite, deniedRead, deniedWrite}
	defer func() {
		for _, path := range fixtures {
			_ = os.Remove(path)
		}
	}()
	for path, contents := range map[string]string{
		allowedRead: allowedReadContents,
		deniedRead:  deniedContents,
		deniedWrite: deniedContents,
	} {
		if err := os.WriteFile(path, []byte(contents), 0o600); err != nil {
			return sandbox.ErrUnavailable
		}
	}

	if result, stdout, err := executeSelfTestProbe(caller, ctx, driver, run, selfTestProfileStart, []string{"/bin/sh", "-c", "exit 0"}); err != nil || !successfulProbe(result) || len(stdout) != 0 {
		return sandbox.ErrUnavailable
	}
	if result, stdout, err := executeSelfTestProbe(caller, ctx, driver, run, selfTestAllowedRead, []string{"/bin/cat", allowedRead}); err != nil || !successfulProbe(result) || string(stdout) != allowedReadContents {
		return sandbox.ErrUnavailable
	}
	if result, stdout, err := executeSelfTestProbe(caller, ctx, driver, run, selfTestAllowedWrite, []string{
		"/bin/sh", "-c", `printf %s "$1" > "$2" && printf %s "$1" > "$3"`, "probe",
		allowedWriteContents, allowedWorkspaceWrite, allowedPrivateWrite,
	}); err != nil || !successfulProbe(result) || len(stdout) != 0 {
		return sandbox.ErrUnavailable
	}
	for _, path := range []string{allowedWorkspaceWrite, allowedPrivateWrite} {
		if contents, err := os.ReadFile(path); err != nil || string(contents) != allowedWriteContents {
			return sandbox.ErrUnavailable
		}
	}
	if result, stdout, err := executeSelfTestProbe(caller, ctx, driver, run, selfTestDeniedRead, []string{"/bin/cat", deniedRead}); err != nil || !deniedProbe(result) || len(stdout) != 0 {
		return sandbox.ErrUnavailable
	}
	if contents, err := os.ReadFile(deniedRead); err != nil || string(contents) != deniedContents {
		return sandbox.ErrUnavailable
	}
	if result, stdout, err := executeSelfTestProbe(caller, ctx, driver, run, selfTestDeniedWrite, []string{
		"/bin/sh", "-c", `printf changed > "$1"`, "probe", deniedWrite,
	}); err != nil || !deniedProbe(result) || len(stdout) != 0 {
		return sandbox.ErrUnavailable
	}
	if contents, err := os.ReadFile(deniedWrite); err != nil || string(contents) != deniedContents {
		return sandbox.ErrUnavailable
	}
	return nil
}

func executeSelfTestProbe(caller, child context.Context, driver *Driver, run selfTestProbeRunner, kind selfTestProbeKind, argv []string) (nativeprocess.Result, []byte, error) {
	if err := driverContextError(caller); err != nil {
		return nativeprocess.Result{}, nil, err
	}
	if err := driverContextError(child); err != nil {
		return nativeprocess.Result{}, nil, err
	}
	stdout := &boundedCollector{limit: selfTestOutputLimit}
	ordinaryStderr := &boundedCollector{limit: selfTestOutputLimit}
	filteredStderr := newInfrastructureStderrFilter(ordinaryStderr)
	args := make([]string, 0, len(argv)+3)
	args = append(args, "-f", driver.profilePath, "--")
	args = append(args, argv...)
	probe := selfTestProbe{
		kind:        kind,
		directories: driver.state.directories,
		spec: nativeprocess.Spec{
			Path:      sandboxExecPath,
			Args:      args,
			Directory: driver.workspace,
			Environment: []string{
				"PATH=/usr/bin:/bin",
				"HOME=" + driver.state.directories.Home,
				"TMPDIR=" + driver.state.directories.Temp,
				"LC_ALL=C",
			},
			Stdout: stdout,
			Stderr: filteredStderr,
		},
	}
	if err := driverContextError(caller); err != nil {
		return nativeprocess.Result{}, nil, err
	}
	if err := driverContextError(child); err != nil {
		return nativeprocess.Result{}, nil, err
	}
	outcome, err := run(child, driver.processes, probe)
	finishErr := filteredStderr.finish()
	if err != nil || finishErr != nil || !outcome.leaderReaped || stdout.overflowed() || ordinaryStderr.overflowed() || filteredStderr.infrastructureFailure() {
		return outcome.result, nil, sandbox.ErrUnavailable
	}
	return outcome.result, stdout.bytes(), nil
}

func successfulProbe(result nativeprocess.Result) bool {
	return result.Code == 0 && !result.Signaled && result.Signal == ""
}

func deniedProbe(result nativeprocess.Result) bool {
	return result.Code > 0 && !result.Signaled && result.Signal == ""
}

func (d *Driver) ID() sandbox.DriverID {
	return ID
}

func (d *Driver) PrivateDirectories() sandbox.PrivateDirectories {
	if d == nil || d.state == nil {
		return sandbox.PrivateDirectories{}
	}
	return d.state.directories
}

func (d *Driver) Capabilities() sandbox.Capabilities {
	capabilities := sandbox.Capabilities{
		ReadConfinement:  true,
		WriteConfinement: true,
		UnixSocketDeny:   true,
	}
	if d != nil && d.network == sandbox.NetworkAllow {
		capabilities.NetworkAllow = true
	} else if d != nil && d.network == sandbox.NetworkDeny {
		capabilities.NetworkDeny = true
	}
	return capabilities
}

func (d *Driver) Execute(ctx context.Context, request sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
	if d == nil || !validDriverRequest(ctx, request, streams, d.workspace) {
		return sandbox.ExitStatus{}, sandbox.ErrInvalidRequest
	}
	if err := driverContextError(ctx); err != nil {
		return sandbox.ExitStatus{}, err
	}

	d.mu.Lock()
	if d.closed {
		d.mu.Unlock()
		return sandbox.ExitStatus{}, sandbox.ErrClosed
	}
	if d.poisoned {
		d.mu.Unlock()
		return sandbox.ExitStatus{}, unavailable(sandbox.ReasonRuntimeFailure)
	}
	manager := d.processes
	profilePath := d.profilePath
	d.mu.Unlock()

	filteredStderr := newInfrastructureStderrFilter(streams.Stderr)
	args := make([]string, 0, len(request.Argv)+3)
	args = append(args, "-f", profilePath, "--")
	args = append(args, request.Argv...)
	result, err := manager.Run(ctx, nativeprocess.Spec{
		Path:        sandboxExecPath,
		Args:        args,
		Directory:   request.Dir,
		Environment: append([]string(nil), request.Env...),
		Stdout:      streams.Stdout,
		Stderr:      filteredStderr,
	})
	finishErr := filteredStderr.finish()
	if filteredStderr.infrastructureFailure() || errors.Is(err, sandbox.ErrChildLaunch) {
		d.latchPoisoned()
		return sandbox.ExitStatus{}, unavailable(sandbox.ReasonRuntimeFailure)
	}
	status := sandbox.ExitStatus{Code: result.Code, Signaled: result.Signaled, Signal: result.Signal}
	if finishErr != nil {
		return status, sandbox.ErrChildWait
	}
	return status, err
}

func (d *Driver) latchPoisoned() {
	d.mu.Lock()
	d.poisoned = true
	d.mu.Unlock()
}

func (d *Driver) Close() error {
	if d == nil {
		return nil
	}
	d.mu.Lock()
	if d.closed {
		done := d.closeDone
		d.mu.Unlock()
		if done == nil {
			return nil
		}
		<-done
		d.mu.Lock()
		err := d.closeErr
		d.mu.Unlock()
		return err
	}
	d.closed = true
	if d.closeDone == nil {
		d.closeDone = make(chan struct{})
	}
	done := d.closeDone
	manager := d.processes
	privateState := d.state
	d.mu.Unlock()

	var managerErr error
	if manager != nil {
		managerErr = manager.Close()
	}
	var stateErr error
	if privateState != nil {
		stateErr = privateState.close()
	}
	closeErr := boundedCloseError(managerErr, stateErr)

	d.mu.Lock()
	d.closeErr = closeErr
	close(done)
	d.mu.Unlock()
	return closeErr
}

func boundedCloseError(managerErr, stateErr error) error {
	var values []error
	for _, identity := range []error{sandbox.ErrChildWait, sandbox.ErrChildTerminate, errStateCleanup} {
		if errors.Is(managerErr, identity) || errors.Is(stateErr, identity) {
			values = append(values, identity)
		}
	}
	switch len(values) {
	case 0:
		return nil
	case 1:
		return values[0]
	default:
		return errors.Join(values...)
	}
}

func unavailable(reason sandbox.UnavailableReason) error {
	return &sandbox.UnavailableError{Reason: reason}
}

func validDriverRequest(ctx context.Context, request sandbox.Request, streams sandbox.Streams, workspace string) bool {
	if isNilDriverValue(ctx) || len(request.Argv) == 0 || isNilDriverValue(streams.Stdout) || isNilDriverValue(streams.Stderr) || request.Env == nil {
		return false
	}
	for _, arg := range request.Argv {
		if strings.IndexByte(arg, 0) >= 0 {
			return false
		}
	}
	if !validDriverDirectory(request.Dir, workspace) || !validDriverEnvironment(request.Env) {
		return false
	}
	return true
}

func validDriverDirectory(directory, workspace string) bool {
	if directory == "" || !filepath.IsAbs(directory) || filepath.Clean(directory) != directory || strings.IndexByte(directory, 0) >= 0 {
		return false
	}
	canonical, err := canonicalFilesystemPath(directory)
	if err != nil || canonical != directory {
		return false
	}
	info, err := os.Lstat(directory)
	return err == nil && info.IsDir() && info.Mode()&os.ModeSymlink == 0 && pathWithin(workspace, directory)
}

func validDriverEnvironment(environment []string) bool {
	names := make(map[string]struct{}, len(environment))
	for _, entry := range environment {
		if strings.IndexByte(entry, 0) >= 0 {
			return false
		}
		name, _, ok := strings.Cut(entry, "=")
		if !ok || !validDriverEnvironmentName(name) {
			return false
		}
		if _, duplicate := names[name]; duplicate {
			return false
		}
		names[name] = struct{}{}
	}
	return true
}

func validDriverEnvironmentName(name string) bool {
	if name == "" || !(name[0] == '_' || name[0] >= 'A' && name[0] <= 'Z' || name[0] >= 'a' && name[0] <= 'z') {
		return false
	}
	for index := 1; index < len(name); index++ {
		character := name[index]
		if character != '_' && (character < 'A' || character > 'Z') && (character < 'a' || character > 'z') && (character < '0' || character > '9') {
			return false
		}
	}
	return true
}

func isNilDriverValue(value any) bool {
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

func driverContextError(ctx context.Context) error {
	if isNilDriverValue(ctx) || ctx.Err() == nil {
		return nil
	}
	if errors.Is(ctx.Err(), context.DeadlineExceeded) || errors.Is(context.Cause(ctx), context.DeadlineExceeded) {
		return context.DeadlineExceeded
	}
	return context.Canceled
}

type boundedCollector struct {
	mu       sync.Mutex
	limit    int
	buffer   bytes.Buffer
	overflow bool
}

func (collector *boundedCollector) Write(data []byte) (int, error) {
	collector.mu.Lock()
	defer collector.mu.Unlock()
	original := len(data)
	remaining := collector.limit - collector.buffer.Len()
	if remaining < len(data) {
		collector.overflow = true
		if remaining < 0 {
			remaining = 0
		}
		data = data[:remaining]
	}
	_, _ = collector.buffer.Write(data)
	return original, nil
}

func (collector *boundedCollector) bytes() []byte {
	collector.mu.Lock()
	defer collector.mu.Unlock()
	return append([]byte(nil), collector.buffer.Bytes()...)
}

func (collector *boundedCollector) overflowed() bool {
	collector.mu.Lock()
	defer collector.mu.Unlock()
	return collector.overflow
}

var (
	sandboxExecDiagnosticPrefix = []byte("sandbox-exec:")
	sandboxExecPolicyExecPrefix = []byte("sandbox-exec: execvp()")
)

type infrastructureStderrFilter struct {
	mu             sync.Mutex
	destination    io.Writer
	pending        []byte
	ordinary       bool
	infrastructure bool
	writeFailed    bool
}

func newInfrastructureStderrFilter(destination io.Writer) *infrastructureStderrFilter {
	return &infrastructureStderrFilter{destination: destination}
}

func (filter *infrastructureStderrFilter) Write(data []byte) (int, error) {
	filter.mu.Lock()
	defer filter.mu.Unlock()
	original := len(data)
	if filter.infrastructure {
		return original, nil
	}
	if filter.ordinary {
		if err := filter.writeOrdinary(data); err != nil {
			return 0, err
		}
		return original, nil
	}

	for len(data) > 0 && !filter.ordinary && !filter.infrastructure {
		remaining := stderrDecisionLimit - len(filter.pending)
		if remaining <= 0 {
			filter.classifyPending()
			break
		}
		take := len(data)
		if take > remaining {
			take = remaining
		}
		if newline := bytes.IndexByte(data[:take], '\n'); newline >= 0 {
			take = newline + 1
		}
		filter.pending = append(filter.pending, data[:take]...)
		data = data[take:]

		if !couldBeginSandboxExecDiagnostic(filter.pending) {
			filter.ordinary = true
		} else if bytes.IndexByte(filter.pending, '\n') >= 0 || len(filter.pending) == stderrDecisionLimit {
			filter.classifyPending()
		}
	}
	if filter.infrastructure {
		return original, nil
	}
	if filter.ordinary {
		pending := filter.pending
		filter.pending = nil
		if err := filter.writeOrdinary(pending); err != nil {
			return 0, err
		}
		if err := filter.writeOrdinary(data); err != nil {
			return 0, err
		}
	}
	return original, nil
}

func (filter *infrastructureStderrFilter) finish() error {
	filter.mu.Lock()
	defer filter.mu.Unlock()
	if !filter.ordinary && !filter.infrastructure && len(filter.pending) > 0 {
		filter.classifyPending()
	}
	if filter.infrastructure {
		filter.pending = nil
		return nil
	}
	pending := filter.pending
	filter.pending = nil
	if err := filter.writeOrdinary(pending); err != nil {
		return sandbox.ErrChildWait
	}
	if filter.writeFailed {
		return sandbox.ErrChildWait
	}
	return nil
}

func (filter *infrastructureStderrFilter) classifyPending() {
	if bytes.HasPrefix(filter.pending, sandboxExecDiagnosticPrefix) &&
		!bytes.HasPrefix(filter.pending, sandboxExecPolicyExecPrefix) {
		filter.infrastructure = true
		filter.pending = nil
		return
	}
	filter.ordinary = true
}

func (filter *infrastructureStderrFilter) writeOrdinary(data []byte) error {
	if len(data) == 0 {
		return nil
	}
	if err := writeAll(filter.destination, data); err != nil {
		filter.writeFailed = true
		return err
	}
	return nil
}

func (filter *infrastructureStderrFilter) infrastructureFailure() bool {
	filter.mu.Lock()
	defer filter.mu.Unlock()
	return filter.infrastructure
}

func couldBeginSandboxExecDiagnostic(value []byte) bool {
	if len(value) <= len(sandboxExecDiagnosticPrefix) {
		return bytes.Equal(value, sandboxExecDiagnosticPrefix[:len(value)])
	}
	return bytes.HasPrefix(value, sandboxExecDiagnosticPrefix)
}

func writeAll(destination io.Writer, data []byte) error {
	for len(data) > 0 {
		written, err := destination.Write(data)
		if err != nil {
			return err
		}
		if written <= 0 || written > len(data) {
			return io.ErrShortWrite
		}
		data = data[written:]
	}
	return nil
}

var _ sandbox.Driver = (*Driver)(nil)
