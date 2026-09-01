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
	CacheBase   string
	HostEntries []string
	ReadPaths   []string
	Network     sandbox.NetworkMode
}

type Driver struct {
	workspace    string
	shell        string
	network      sandbox.NetworkMode
	state        *state
	profilePath  string
	profile      []byte
	processes    *nativeprocess.Manager
	runExecution func(context.Context, *nativeprocess.Manager, nativeprocess.Spec) (nativeprocess.Result, error)
	closeManager func(*nativeprocess.Manager) error
	closeState   func(*state) error

	mu             sync.Mutex
	closed         bool
	poisoned       bool
	closeDone      chan struct{}
	closeErr       error
	executionEvent func(driverExecutionEvent)
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

type driverExecutionEventStage uint8

const (
	driverExecutionBeforeAdmission driverExecutionEventStage = iota + 1
	driverExecutionInfrastructureClassified
)

type driverExecutionEvent struct {
	stage driverExecutionEventStage
}

type selfTestProbeRunner func(context.Context, *nativeprocess.Manager, selfTestProbe) (selfTestProbeResult, error)

type driverDependencies struct {
	inspectSandboxExec  func(string) (sandboxExecIdentity, error)
	userCacheDir        func() (string, error)
	createState         func(string, string) (*state, error)
	generateProfile     func(profileOptions) ([]byte, error)
	writeProfile        func(*state, []byte) error
	runProbe            selfTestProbeRunner
	runExecution        func(context.Context, *nativeprocess.Manager, nativeprocess.Spec) (nativeprocess.Result, error)
	closeManager        func(*nativeprocess.Manager) error
	closeState          func(*state) error
	selfTestRandomBytes func(selfTestFixtureKind, []byte) error
	selfTestCloseFD     func(int) error
	selfTestEvent       func(selfTestFixtureEvent)
}

func defaultDriverDependencies() driverDependencies {
	return driverDependencies{
		inspectSandboxExec: inspectProductionSandboxExec,
		userCacheDir:       os.UserCacheDir,
		createState:        createState,
		generateProfile:    generateProfile,
		writeProfile: func(privateState *state, profile []byte) error {
			return privateState.writeProfile(profile)
		},
		closeManager: func(manager *nativeprocess.Manager) error {
			return manager.Close()
		},
		closeState: func(privateState *state) error {
			return privateState.close()
		},
		selfTestRandomBytes: productionSelfTestRandomBytes,
		selfTestCloseFD:     syscall.Close,
		runProbe: func(ctx context.Context, manager *nativeprocess.Manager, probe selfTestProbe) (selfTestProbeResult, error) {
			result, err := manager.Run(ctx, probe.spec)
			return selfTestProbeResult{result: result, leaderReaped: true}, err
		},
		runExecution: func(ctx context.Context, manager *nativeprocess.Manager, spec nativeprocess.Spec) (nativeprocess.Result, error) {
			return manager.Run(ctx, spec)
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
	if !validDriverDependencies(dependencies, options.CacheBase == "") {
		return nil, unavailable(sandbox.ReasonRuntimeFailure)
	}

	identity, operationErr := dependencies.inspectSandboxExec(sandboxExecPath)
	if err := driverContextError(ctx); err != nil {
		return nil, err
	}
	if operationErr != nil || !validSandboxExecIdentity(identity) {
		return nil, unavailable(sandbox.ReasonSeatbeltMissing)
	}

	workspace, ok := resolveDriverDirectory(options.Workspace)
	if err := driverContextError(ctx); err != nil {
		return nil, err
	}
	if !ok {
		return nil, unavailable(sandbox.ReasonPolicyUnsupported)
	}
	shell, operationErr := resolveProfilePath(options.Shell)
	if err := driverContextError(ctx); err != nil {
		return nil, err
	}
	if operationErr != nil || !validResolvedProfilePath(shell) || shell.kind != profilePathRegular || !shell.executable {
		return nil, unavailable(sandbox.ReasonInvalidShell)
	}
	home, ok := resolveDriverDirectory(options.Home)
	if err := driverContextError(ctx); err != nil {
		return nil, err
	}
	if !ok {
		return nil, unavailable(sandbox.ReasonPolicyUnsupported)
	}
	if options.Network != sandbox.NetworkAllow && options.Network != sandbox.NetworkDeny {
		return nil, unavailable(sandbox.ReasonPolicyUnsupported)
	}
	if err := driverContextError(ctx); err != nil {
		return nil, err
	}

	cacheBase := strings.Clone(options.CacheBase)
	if cacheBase == "" {
		cacheBase, operationErr = dependencies.userCacheDir()
		if err := driverContextError(ctx); err != nil {
			return nil, err
		}
		if operationErr != nil {
			return nil, unavailable(sandbox.ReasonSelfTestFailed)
		}
	}

	privateState, operationErr := dependencies.createState(workspace, cacheBase)
	if operationErr != nil || privateState == nil {
		return failOpen(ctx, dependencies, nil, privateState, unavailable(sandbox.ReasonSelfTestFailed))
	}
	if err := driverContextError(ctx); err != nil {
		return failOpen(ctx, dependencies, nil, privateState, err)
	}

	profile, operationErr := dependencies.generateProfile(profileOptions{
		Workspace:   workspace,
		Directories: privateState.directories,
		Shell:       shell.path,
		Home:        home,
		HostEntries: append([]string(nil), options.HostEntries...),
		ReadPaths:   append([]string(nil), options.ReadPaths...),
		Network:     options.Network,
	})
	if operationErr != nil {
		return failOpen(ctx, dependencies, nil, privateState, unavailable(sandbox.ReasonSelfTestFailed))
	}
	if err := driverContextError(ctx); err != nil {
		return failOpen(ctx, dependencies, nil, privateState, err)
	}
	operationErr = dependencies.writeProfile(privateState, profile)
	if operationErr != nil {
		return failOpen(ctx, dependencies, nil, privateState, unavailable(sandbox.ReasonSelfTestFailed))
	}
	if err := driverContextError(ctx); err != nil {
		return failOpen(ctx, dependencies, nil, privateState, err)
	}

	manager := nativeprocess.New()
	driver := &Driver{
		workspace:    workspace,
		shell:        shell.path,
		network:      options.Network,
		state:        privateState,
		profilePath:  privateState.profilePath,
		profile:      append([]byte(nil), profile...),
		processes:    manager,
		runExecution: dependencies.runExecution,
		closeManager: dependencies.closeManager,
		closeState:   dependencies.closeState,
		closeDone:    make(chan struct{}),
	}
	selfTestErr := runStartupSelfTest(ctx, driver, dependencies)
	if selfTestErr != nil {
		return failOpen(ctx, dependencies, manager, privateState, unavailable(sandbox.ReasonSelfTestFailed))
	}
	if err := driverContextError(ctx); err != nil {
		return failOpen(ctx, dependencies, manager, privateState, err)
	}
	return driver, nil
}

func validDriverDependencies(dependencies driverDependencies, requireCacheResolver bool) bool {
	return dependencies.inspectSandboxExec != nil && (!requireCacheResolver || dependencies.userCacheDir != nil) &&
		dependencies.createState != nil && dependencies.generateProfile != nil && dependencies.writeProfile != nil &&
		dependencies.runProbe != nil && dependencies.runExecution != nil && dependencies.closeManager != nil &&
		dependencies.closeState != nil && dependencies.selfTestRandomBytes != nil && dependencies.selfTestCloseFD != nil
}

func cleanupPartialOpen(dependencies driverDependencies, manager *nativeprocess.Manager, privateState *state) error {
	var managerErr error
	if manager != nil {
		managerErr = dependencies.closeManager(manager)
	}
	var stateErr error
	if privateState != nil {
		stateErr = dependencies.closeState(privateState)
	}
	return boundedCloseError(managerErr, stateErr)
}

func failOpen(ctx context.Context, dependencies driverDependencies, manager *nativeprocess.Manager, privateState *state, primary error) (*Driver, error) {
	cleanupErr := cleanupPartialOpen(dependencies, manager, privateState)
	if cancellationErr := driverContextError(ctx); cancellationErr != nil {
		secondary := make([]error, 0, 2)
		if primary != nil && !errors.Is(primary, cancellationErr) {
			secondary = append(secondary, primary)
		}
		if cleanupErr != nil {
			secondary = append(secondary, cleanupErr)
		}
		return nil, joinBoundedDriverErrors(cancellationErr, secondary...)
	}
	return nil, joinBoundedDriverErrors(primary, cleanupErr)
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

func runStartupSelfTest(caller context.Context, driver *Driver, dependencies driverDependencies) error {
	ctx, cancel := context.WithTimeout(caller, selfTestTimeout)
	defer cancel()

	const (
		allowedReadContents  = "otto-seatbelt-allowed-read"
		allowedWriteContents = "otto-seatbelt-allowed-write"
		deniedContents       = "otto-seatbelt-denied-fixture"
	)
	fixtures, err := prepareSelfTestFixtures(driver, dependencies, allowedReadContents, deniedContents)
	if err != nil {
		return sandbox.ErrUnavailable
	}

	probeErr := func() error {
		allowedRead := fixtures.fixture(selfTestAllowedReadFixture)
		deniedRead := fixtures.fixture(selfTestDeniedReadFixture)
		deniedWrite := fixtures.fixture(selfTestDeniedWriteFixture)
		if allowedRead == nil || deniedRead == nil || deniedWrite == nil {
			return sandbox.ErrUnavailable
		}

		if result, stdout, err := executeSelfTestProbe(caller, ctx, driver, dependencies.runProbe, selfTestProfileStart, []string{"/bin/sh", "-c", "exit 0"}, nil); err != nil || !successfulProbe(result) || len(stdout) != 0 {
			return sandbox.ErrUnavailable
		}
		if result, stdout, err := executeSelfTestProbe(caller, ctx, driver, dependencies.runProbe, selfTestAllowedRead, []string{"/bin/cat", allowedRead.path}, nil); err != nil || !successfulProbe(result) || string(stdout) != allowedReadContents || allowedRead.validateContents(allowedReadContents) != nil {
			return sandbox.ErrUnavailable
		}
		writeArguments, err := fixtures.writeProbeArguments()
		if err != nil {
			return sandbox.ErrUnavailable
		}
		writeProbe := []string{
			"/usr/bin/perl", "-MFcntl=O_WRONLY,O_NOFOLLOW", "-e",
			`my $value = $ARGV[0]; for my $base (1, 8) { my ($path, $device, $inode, $type, $uid, $mode, $links) = @ARGV[$base..$base+6]; sysopen(my $fh, $path, O_WRONLY|O_NOFOLLOW) or exit 65; my @stat = stat($fh); @stat or exit 66; $stat[0] == $device && $stat[1] == $inode && ($stat[2] & 0170000) == $type && $stat[4] == $uid && ($stat[2] & 07777) == $mode && $stat[3] == $links or exit 67; truncate($fh, 0) or exit 68; my $written = 0; while ($written < length($value)) { my $count = syswrite($fh, $value, length($value) - $written, $written); defined($count) && $count > 0 or exit 69; $written += $count; } close($fh) or exit 70; }`,
			allowedWriteContents,
		}
		writeProbe = append(writeProbe, writeArguments...)
		if result, stdout, err := executeSelfTestProbe(caller, ctx, driver, dependencies.runProbe, selfTestAllowedWrite, writeProbe, fixtures.validateBeforeWriteDispatch); err != nil || !successfulProbe(result) || len(stdout) != 0 {
			return sandbox.ErrUnavailable
		}
		if fixtures.validateWrittenContents(allowedWriteContents) != nil {
			return sandbox.ErrUnavailable
		}
		if result, stdout, err := executeSelfTestProbe(caller, ctx, driver, dependencies.runProbe, selfTestDeniedRead, []string{"/bin/cat", deniedRead.path}, nil); err != nil || !deniedProbe(result) || len(stdout) != 0 || deniedRead.validateContents(deniedContents) != nil {
			return sandbox.ErrUnavailable
		}
		if result, stdout, err := executeSelfTestProbe(caller, ctx, driver, dependencies.runProbe, selfTestDeniedWrite, []string{
			"/bin/sh", "-c", `printf changed > "$1"`, "probe", deniedWrite.path,
		}, nil); err != nil || !deniedProbe(result) || len(stdout) != 0 || deniedWrite.validateContents(deniedContents) != nil {
			return sandbox.ErrUnavailable
		}
		return nil
	}()
	cleanupErr := fixtures.cleanup()
	if probeErr != nil || cleanupErr != nil {
		return sandbox.ErrUnavailable
	}
	return nil
}

func executeSelfTestProbe(caller, child context.Context, driver *Driver, run selfTestProbeRunner, kind selfTestProbeKind, argv []string, beforeDispatch func() error) (nativeprocess.Result, []byte, error) {
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
	if beforeDispatch != nil {
		if err := beforeDispatch(); err != nil {
			return nativeprocess.Result{}, nil, sandbox.ErrUnavailable
		}
		if err := driverContextError(caller); err != nil {
			return nativeprocess.Result{}, nil, err
		}
		if err := driverContextError(child); err != nil {
			return nativeprocess.Result{}, nil, err
		}
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

	d.emitExecutionEvent(driverExecutionBeforeAdmission)
	d.mu.Lock()
	if d.closed {
		d.mu.Unlock()
		return sandbox.ExitStatus{}, driverAdmissionError(ctx, sandbox.ErrClosed)
	}
	if d.poisoned {
		d.mu.Unlock()
		return sandbox.ExitStatus{}, driverAdmissionError(ctx, unavailable(sandbox.ReasonRuntimeFailure))
	}
	manager := d.processes
	profilePath := d.profilePath
	d.mu.Unlock()
	if err := driverContextError(ctx); err != nil {
		return sandbox.ExitStatus{}, err
	}

	filteredStderr := newInfrastructureStderrFilter(streams.Stderr, d.latchInfrastructureFailure)
	args := make([]string, 0, len(request.Argv)+3)
	args = append(args, "-f", profilePath, "--")
	args = append(args, request.Argv...)
	result, err := d.runExecution(ctx, manager, nativeprocess.Spec{
		Path:        sandboxExecPath,
		Args:        args,
		Directory:   request.Dir,
		Environment: append([]string(nil), request.Env...),
		Stdout:      streams.Stdout,
		Stderr:      filteredStderr,
	})
	finishErr := filteredStderr.finish()
	infrastructureFailure := d.classifyInfrastructureFailure(filteredStderr.infrastructureFailure(), err, finishErr)
	status := sandbox.ExitStatus{Code: result.Code, Signaled: result.Signaled, Signal: result.Signal}
	if cancellationErr := driverContextError(ctx); cancellationErr != nil {
		var secondary []error
		if infrastructureFailure {
			secondary = append(secondary, unavailable(sandbox.ReasonRuntimeFailure))
		}
		if finishErr != nil {
			secondary = append(secondary, sandbox.ErrChildWait)
		}
		return status, joinBoundedDriverErrors(cancellationErr, secondary...)
	}
	if infrastructureFailure {
		return sandbox.ExitStatus{}, unavailable(sandbox.ReasonRuntimeFailure)
	}
	return status, boundedManagerExecutionError(err)
}

func managerInfrastructureFailure(err error) bool {
	if err == nil {
		return false
	}
	if errors.Is(err, sandbox.ErrChildLaunch) || errors.Is(err, sandbox.ErrChildWait) || errors.Is(err, sandbox.ErrChildTerminate) {
		return true
	}
	return managerErrorHasUnknownLeaf(err)
}

func managerErrorHasUnknownLeaf(err error) bool {
	if err == nil || err == context.Canceled || err == context.DeadlineExceeded || err == sandbox.ErrClosed {
		return false
	}
	if joined, ok := err.(interface{ Unwrap() []error }); ok {
		for _, child := range joined.Unwrap() {
			if managerErrorHasUnknownLeaf(child) {
				return true
			}
		}
		return false
	}
	if wrapped, ok := err.(interface{ Unwrap() error }); ok {
		return managerErrorHasUnknownLeaf(wrapped.Unwrap())
	}
	return true
}

func boundedManagerExecutionError(err error) error {
	if err == nil {
		return nil
	}
	if errors.Is(err, context.DeadlineExceeded) {
		return context.DeadlineExceeded
	}
	if errors.Is(err, context.Canceled) {
		return context.Canceled
	}
	if errors.Is(err, sandbox.ErrClosed) {
		return sandbox.ErrClosed
	}
	return unavailable(sandbox.ReasonRuntimeFailure)
}

type boundedDriverError struct {
	display    error
	identities []error
}

func (err *boundedDriverError) Error() string {
	return err.display.Error()
}

func (err *boundedDriverError) Unwrap() []error {
	return append([]error(nil), err.identities...)
}

func joinBoundedDriverErrors(primary error, secondary ...error) error {
	values := []error{primary}
	for _, value := range secondary {
		if value != nil {
			values = append(values, value)
		}
	}
	if len(values) == 1 {
		return primary
	}
	return &boundedDriverError{display: primary, identities: values}
}

func driverAdmissionError(ctx context.Context, primary error) error {
	if cancellationErr := driverContextError(ctx); cancellationErr != nil {
		return joinBoundedDriverErrors(cancellationErr, primary)
	}
	return primary
}

func (d *Driver) classifyInfrastructureFailure(filterFailure bool, managerErr, finishErr error) bool {
	d.mu.Lock()
	defer d.mu.Unlock()
	infrastructureFailure := filterFailure || managerInfrastructureFailure(managerErr) || finishErr != nil
	if infrastructureFailure && !d.poisoned {
		d.poisoned = true
		d.emitExecutionEvent(driverExecutionInfrastructureClassified)
	}
	return infrastructureFailure
}

func (d *Driver) latchInfrastructureFailure() {
	d.mu.Lock()
	if !d.poisoned {
		d.poisoned = true
		d.emitExecutionEvent(driverExecutionInfrastructureClassified)
	}
	d.mu.Unlock()
}

func (d *Driver) emitExecutionEvent(stage driverExecutionEventStage) {
	if d != nil && d.executionEvent != nil {
		d.executionEvent(driverExecutionEvent{stage: stage})
	}
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
	closeManager := d.closeManager
	closeState := d.closeState
	d.mu.Unlock()

	var managerErr error
	if manager != nil && closeManager != nil {
		managerErr = closeManager(manager)
	}
	var stateErr error
	if privateState != nil && closeState != nil {
		stateErr = closeState(privateState)
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
	if managerErr != nil {
		for _, identity := range []error{sandbox.ErrChildWait, sandbox.ErrChildTerminate} {
			if errors.Is(managerErr, identity) {
				values = append(values, identity)
			}
		}
		if len(values) == 0 {
			values = append(values, sandbox.ErrChildWait)
		}
	}
	if stateErr != nil {
		values = append(values, errStateCleanup)
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
	mu               sync.Mutex
	destination      io.Writer
	pending          []byte
	ordinary         bool
	infrastructure   bool
	execvpSuppressed bool
	writeFailed      bool
	onInfrastructure func()
}

func newInfrastructureStderrFilter(destination io.Writer, callbacks ...func()) *infrastructureStderrFilter {
	filter := &infrastructureStderrFilter{destination: destination}
	if len(callbacks) > 0 {
		filter.onInfrastructure = callbacks[0]
	}
	return filter
}

func (filter *infrastructureStderrFilter) Write(data []byte) (int, error) {
	filter.mu.Lock()
	defer filter.mu.Unlock()
	original := len(data)
	if filter.infrastructure || filter.execvpSuppressed {
		return original, nil
	}
	if filter.ordinary {
		if err := filter.writeOrdinary(data); err != nil {
			return 0, err
		}
		return original, nil
	}

	for len(data) > 0 && !filter.ordinary && !filter.infrastructure && !filter.execvpSuppressed {
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
	if filter.infrastructure || filter.execvpSuppressed {
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
	if !filter.ordinary && !filter.infrastructure && !filter.execvpSuppressed && len(filter.pending) > 0 {
		filter.classifyPending()
	}
	if filter.infrastructure || filter.execvpSuppressed {
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
	if bytes.HasPrefix(filter.pending, sandboxExecPolicyExecPrefix) {
		filter.execvpSuppressed = true
		filter.pending = nil
		return
	}
	if bytes.HasPrefix(filter.pending, sandboxExecDiagnosticPrefix) {
		if filter.onInfrastructure != nil {
			filter.onInfrastructure()
		}
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

func (filter *infrastructureStderrFilter) execvpDiagnosticSuppressed() bool {
	filter.mu.Lock()
	defer filter.mu.Unlock()
	return filter.execvpSuppressed
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
