# macOS Sandbox Driver Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Otto's unrestricted shell execution with a stable Sandbox Executor/Driver boundary, a fail-closed macOS Seatbelt Driver, and an explicitly selected direct/off Driver.

**Architecture:** `cmd/otto` captures one host-environment snapshot and owns one process-level `sandbox.Executor`. `internal/tool/bash.go` becomes a bounded presentation adapter over that Executor, while `internal/sandbox` owns policy, environment filtering, lifecycle, and Driver contracts. The Seatbelt Driver binds one immutable workspace policy and private runtime tree; the app/frontends receive only a sanitized immutable capability summary.

**Tech Stack:** Go 1.26, `os/exec`, `golang.org/x/sys/unix`, macOS `/usr/bin/sandbox-exec`, embedded SBPL, existing Bubble Tea v2 TUI and line REPL.

**Spec:** `docs/superpowers/specs/2026-08-31-macos-sandbox-driver-design.md`

## Global Constraints

- Phase 1 implements only `auto`, `seatbelt`, and explicit `off`; Docker, Apple Container, Podman, Linux Sandbox support, remote execution, resource controls, and permission prompts are excluded.
- On Darwin, `auto` always selects Seatbelt. Missing binaries, profile-generation errors, capability mismatches, self-test failures, invalid shell state, and environment-safety failures disable `bash`; none may silently select direct execution.
- Only explicit `driver = "off"` or `--sandbox off` permits unconfined execution, and every frontend must display the unsafe state.
- The default Seatbelt policy writes the whole canonical workspace and process-private home/temp/cache roots, reads only reviewed system/toolchain/`PATH`/`read_paths` roots, allows IP networking by default, and denies arbitrary Unix sockets, Apple Events, Launch Services, and writes elsewhere.
- The Seatbelt state/profile root must be outside the workspace, owner-only, symlink-free, cleaned without following symlinks, and created without changing process-wide umask.
- `HOME`, temporary directories, and writable build/package caches are private in Seatbelt mode. Direct mode preserves ordinary host home/cache paths but still applies mandatory environment filtering and exact-value output redaction.
- Provider credentials are non-restorable. `allow_env` restores only exact automatically classified names; dynamic-loader and shell-startup variables remain non-restorable.
- Sensitive-value collection is bounded to 512 unique values and 1 MiB total. A bound violation disables `bash` without exposing names or values.
- Driver and Executor values are defensively copied, concurrency-safe, close-safe, and non-reentrant. Driver callbacks do not exist.
- Every command starts in a new process group. Normal exit, cancellation, timeout, Executor close, and Driver close terminate/reap that group; hostile `setsid` escape remains a documented Seatbelt limitation.
- No Sandbox code mutates process-wide cwd, environment, signal handlers, or umask.
- Raw Sandbox errors, paths from internal state, environment values, profiles, command text, and diagnostics never enter provider prompts, tool results, app status, or Pi JSONL.
- Existing file tools stay workspace-bound and remain registered when `bash` is unavailable.
- `RunOnce` remains prompt-only and does not interpret slash commands.
- Add no third-party dependency; use the existing `golang.org/x/sys` module and the Go standard library.
- All default tests remain offline and require no credentials, Docker daemon, Node/Pi, external network, or interactive terminal.
- Sandbox tests synchronize with channels, pipes, fake timers, and process-exit barriers. They do not use sleeps or timing-based polling; `time.After` is permitted only as a hard deadlock deadline.
- Use strict TDD for every production behavior: observe the focused test fail for the expected reason before implementing it.
- Preserve `CLAUDE.md`; do not modify `hello.py`.

## File and Responsibility Map

### New neutral Sandbox package

- `internal/sandbox/types.go` — stable Driver, Executor-facing command, policy, capabilities, status, stream, and exit-status types.
- `internal/sandbox/errors.go` — bounded typed/sentinel errors and unavailable-reason classification.
- `internal/sandbox/executor.go` — immutable policy/environment binding, capability checks, concurrency, and idempotent close.
- `internal/sandbox/executor_test.go` — typed-nil rejection, defensive-copy, policy, close, and concurrent-execution tests.
- `internal/sandbox/environment.go` — deterministic host environment parsing, mandatory removal, secret classification, URI-userinfo extraction, private-path rewrites, and bounded redaction set.
- `internal/sandbox/environment_test.go` — table-driven filtering, restore, bounds, cloning, and deterministic-order tests.
- `internal/sandbox/direct/driver.go` — explicit unconfined Driver backed by the shared native process manager.
- `internal/sandbox/direct/driver_test.go` — direct capability and conformance tests.
- `internal/sandbox/sandboxtest/conformance.go` — reusable Driver behavior suite for advertised capabilities.

### New native process lifecycle package

- `internal/sandbox/internal/nativeprocess/manager_unix.go` — process groups, pipe forwarding, cancellation/timeout/close escalation, wait, and reaping on Unix.
- `internal/sandbox/internal/nativeprocess/manager_other.go` — bounded unsupported result on platforms without the Unix implementation.
- `internal/sandbox/internal/nativeprocess/manager_test.go` — barrier-driven process lifecycle tests.

### New Seatbelt package

- `internal/sandbox/seatbelt/profile_v1.sb` — embedded closed-by-default versioned SBPL template.
- `internal/sandbox/seatbelt/profile.go` — safe SBPL literals, reviewed root discovery, `PATH` filtering, policy rendering, and profile invariants.
- `internal/sandbox/seatbelt/profile_test.go` — injection resistance, broad-grant rejection, root selection, and deterministic rendering.
- `internal/sandbox/seatbelt/state_unix.go` — owner-only private state/profile tree and no-follow cleanup.
- `internal/sandbox/seatbelt/state_other.go` — unsupported state constructor for non-Unix builds.
- `internal/sandbox/seatbelt/state_test.go` — permissions, workspace separation, symlink rejection, and cleanup tests.
- `internal/sandbox/seatbelt/driver_darwin.go` — fixed `/usr/bin/sandbox-exec` Driver, self-test, execution wrapper, diagnostics suppression, and close.
- `internal/sandbox/seatbelt/driver_other.go` — typed unavailable constructor outside Darwin.
- `internal/sandbox/seatbelt/driver_other_test.go` — non-Darwin typed unavailability and no-process tests.
- `internal/sandbox/seatbelt/driver_darwin_test.go` — Driver conformance, self-test, filesystem/network/socket/process cleanup, and conditional toolchain tests.

### Modified integration files

- `internal/config/config.go`, `internal/config/config_test.go` — strict `[sandbox]` TOML shape.
- `internal/config/resolve.go`, `internal/config/resolve_test.go` — Sandbox defaults, CLI precedence, enums, list/name/size validation.
- `internal/tool/bash.go`, `internal/tool/bash_test.go` — delegate command execution to `sandbox.CommandExecutor`; retain bounded formatting and defense-in-depth redaction.
- `cmd/otto/sandbox_runtime.go`, `cmd/otto/sandbox_runtime_test.go` — select/open/close direct or Seatbelt, build immutable environment, and map safe status.
- `cmd/otto/main.go`, `cmd/otto/main_test.go` — one-shot environment capture, `--sandbox`, fail-closed startup, warnings, and lifecycle ownership.
- `cmd/otto/runtime_builder.go`, `cmd/otto/runtime_builder_test.go` — optional Bash registration, shared Executor reuse, combined redaction set, and dynamic system prompt.
- `internal/app/controller.go`, `internal/app/controller_test.go` — sanitized Sandbox info through session replacements and `Info()`.
- `internal/repl/repl.go`, `internal/repl/repl_test.go` — startup and `/session` status without changing `RunOnce`.
- `internal/tui/layout.go`, `internal/tui/layout_test.go`, `internal/tui/model_test.go` — fixed footer badge and Sandbox details in the existing session overlay.
- `cmd/otto/tui_pty_test.go` — terminal-level status evidence.
- `README.md`, `docs/user-manual.md`, `AGENTS.md` — Stage 1 behavior, configuration, threat model, troubleshooting, package ownership, and future-Driver rules.

---

### Task 1: Stable Driver Contract and Executor

**Files:**
- Create: `internal/sandbox/types.go`
- Create: `internal/sandbox/errors.go`
- Create: `internal/sandbox/executor.go`
- Create: `internal/sandbox/executor_test.go`

**Interfaces:**
- Produces: `Driver`, `DriverID`, `CommandExecutor`, `Executor`, `Capabilities`, `Policy`, `Request`, `Streams`, `ExitStatus`, `Settings`, `PrivateDirectories`, and safe error identities consumed by every later task.
- Consumes: only Go standard-library types.

- [ ] **Step 1: Add failing contract and validation tests**

Create tests named:

```go
func TestNewExecutorRejectsNilAndTypedNilDriver(t *testing.T)
func TestNewExecutorRejectsEmptyDriverIDAndUnsupportedPolicy(t *testing.T)
func TestNewExecutorCanonicalizesAndBindsWorkspace(t *testing.T)
func TestExecutorDefensivelyCopiesArgvAndEnvironment(t *testing.T)
func TestExecutorRejectsNoncanonicalOrEscapedDirectory(t *testing.T)
func TestExecutorRejectsMalformedOrDuplicateEnvironment(t *testing.T)
func TestExecutorBuildsFreshDriverRequest(t *testing.T)
func TestExecutorPreservesCancellationIdentityWithoutCallingDriver(t *testing.T)
func TestExecutorCloseIsConcurrentAndIdempotent(t *testing.T)
func TestExecutorExecuteRacingCloseNeverRunsAfterClose(t *testing.T)
```

Use a mutex-protected fake Driver that records cloned requests and exposes `started`, `release`, and `closed` channels. Assert that mutating request `Argv`/`Env` or a recorded request cannot alter subsequent calls. Assert `errors.Is(err, ErrClosed)`, `errors.Is(err, ErrUnsupportedPolicy)`, and `errors.Is(err, context.Canceled)` rather than matching raw implementation text.

- [ ] **Step 2: Run the focused tests and observe the missing-package/type failure**

Run:

```bash
go test ./internal/sandbox -run 'Test(NewExecutor|Executor)' -count=1
```

Expected: FAIL because `internal/sandbox` contract types do not exist.

- [ ] **Step 3: Define the normative types and immutable settings**

Use the spec's exact normative execution shapes:

```go
package sandbox

type DriverID string

type DriverMode string

const (
    DriverAuto     DriverMode = "auto"
    DriverSeatbelt DriverMode = "seatbelt"
    DriverOff      DriverMode = "off"
)

type FilesystemMode uint8

const (
    FilesystemWorkspaceWrite FilesystemMode = iota + 1
    FilesystemUnconfined
)

type NetworkMode uint8

const (
    NetworkDeny NetworkMode = iota + 1
    NetworkAllow
)

type Policy struct {
    Filesystem FilesystemMode
    Network    NetworkMode
}

type Capabilities struct {
    ReadConfinement  bool
    WriteConfinement bool
    NetworkDeny      bool
    NetworkAllow     bool
    UnixSocketDeny   bool
}

type Driver interface {
    ID() DriverID
    Capabilities() Capabilities
    Execute(context.Context, Request, Streams) (ExitStatus, error)
    Close() error
}

type Request struct {
    Argv []string
    Dir  string
    Env  []string
}

type Streams struct {
    Stdout io.Writer
    Stderr io.Writer
}

type ExitStatus struct {
    Code     int
    Signaled bool
    Signal   string
}

type CommandExecutor interface {
    Execute(context.Context, Request, Streams) (ExitStatus, error)
}

type Settings struct {
    Driver    DriverMode
    Network   NetworkMode
    ReadPaths []string
    AllowEnv  []string
}

type PrivateDirectories struct {
    Root  string
    Home  string
    Temp  string
    Cache string
}
```

`ExitStatus.Code` is `-1` when `Signaled` is true. Policy is immutable Executor state and never appears in a model-controlled request. Slice-bearing types expose clone helpers rather than returning internal storage.

- [ ] **Step 4: Implement bounded safe errors and capability checks**

Define exported identities with fixed text:

```go
var (
    ErrClosed            = errors.New("sandbox executor is closed")
    ErrInvalidRequest    = errors.New("invalid sandbox request")
    ErrUnsupportedPolicy = errors.New("sandbox policy is unsupported")
    ErrUnavailable       = errors.New("sandbox driver is unavailable")
    ErrEnvironmentUnsafe = errors.New("sandbox environment is unsafe")
    ErrChildLaunch       = errors.New("sandbox child launch failed")
    ErrChildWait         = errors.New("sandbox child wait failed")
    ErrChildTerminate    = errors.New("sandbox child termination failed")
)

type UnavailableReason string

const (
    ReasonUnsupportedPlatform UnavailableReason = "unsupported-platform"
    ReasonSeatbeltMissing      UnavailableReason = "seatbelt-missing"
    ReasonSelfTestFailed       UnavailableReason = "self-test-failed"
    ReasonRuntimeFailure       UnavailableReason = "runtime-failure"
    ReasonInvalidShell         UnavailableReason = "invalid-shell"
    ReasonEnvironmentRejected  UnavailableReason = "environment-rejected"
    ReasonPolicyUnsupported    UnavailableReason = "policy-unsupported"
)
```

A typed `UnavailableError` carries only one enum reason and matches `ErrUnavailable`; it never wraps an OS error. `NewExecutor` rejects interface nil, reflection-detected typed nil, and Driver IDs outside `[a-z0-9-]{1,32}` so IDs are safe for bounded local status. For `FilesystemWorkspaceWrite`, require read/write confinement, Unix-socket denial, and the selected network capability. For `FilesystemUnconfined`, require `NetworkAllow` and reject `NetworkDeny`. Concurrency, non-retention, same-process-group cleanup, and idempotent close are mandatory contract behavior, not optional capabilities.

- [ ] **Step 5: Implement Executor concurrency and copying**

Use this API:

```go
func NewExecutor(driver Driver, policy Policy, workspace string) (*Executor, error)
func (e *Executor) Execute(ctx context.Context, request Request, streams Streams) (ExitStatus, error)
func (e *Executor) Close() error
func (e *Executor) ID() DriverID
func (e *Executor) Capabilities() Capabilities
```

Canonicalize and bind one workspace at construction. Validate nonempty `Argv`, NUL-free arguments, `Dir` equal to the canonical workspace or an existing canonical descendant, a complete environment with valid nonempty unique names and no NUL bytes, and non-nil stdout/stderr. Defensively copy `Argv` and `Env` before delegation; the Driver may not retain requests, context, or writers. Reject pre-cancelled contexts without calling the Driver. Mark closed under a mutex, release the mutex, then call Driver `Close`; the Driver owns active-call draining. Cache the first `Close` result and return it to all callers.

- [ ] **Step 6: Run focused tests, race tests, formatting, and commit**

Run:

```bash
gofmt -w internal/sandbox/types.go internal/sandbox/errors.go internal/sandbox/executor.go internal/sandbox/executor_test.go
go test ./internal/sandbox -run 'Test(NewExecutor|Executor)' -count=1
go test -race ./internal/sandbox -run 'Test(NewExecutor|Executor)' -count=1
git diff --check
git add internal/sandbox
git commit -m "feat: define sandbox driver contract"
```

Expected: all focused tests PASS.

---

### Task 2: Strict Sandbox Configuration and Safe Environment Resolution

**Files:**
- Modify: `internal/config/config.go`
- Modify: `internal/config/config_test.go`
- Modify: `internal/config/resolve.go`
- Modify: `internal/config/resolve_test.go`
- Create: `internal/sandbox/environment.go`
- Create: `internal/sandbox/environment_test.go`

**Interfaces:**
- Consumes: Task 1 `sandbox.Settings`, `DriverMode`, `NetworkMode`, `PrivateDirectories`, and `ErrEnvironmentUnsafe`.
- Produces: `config.ResolveSandbox`, `sandbox.ParseEnvironment`, `sandbox.ResolveEnvironment`, `EnvironmentSnapshot.Entries`, and `EnvironmentSnapshot.RedactionValues` for CLI startup.

- [ ] **Step 1: Add failing strict-config tests**

Add exact cases for absent defaults, explicit empty Driver/Network rejection, valid TOML, unknown `[sandbox]` fields, invalid driver values including `docker`, invalid network values, relative `read_paths`, wildcard/invalid/duplicate `allow_env`, a 32-KiB-plus-one textual path, cloned/sorted inputs, and CLI Driver precedence.

The resolved defaults must equal:

```go
sandbox.Settings{
    Driver:    sandbox.DriverAuto,
    Network:   sandbox.NetworkAllow,
    ReadPaths: []string{},
    AllowEnv:  []string{},
}
```

Use:

```go
func ResolveSandbox(raw SandboxConfig, cliDriver *string) (sandbox.Settings, error)
```

- [ ] **Step 2: Run config tests and observe missing fields/functions**

Run:

```bash
go test ./internal/config -run 'Test(LoadSandbox|ResolveSandbox)' -count=1
```

Expected: FAIL because `SandboxConfig` and `ResolveSandbox` are undefined.

- [ ] **Step 3: Add strict TOML shape and resolver**

Add:

```go
type File struct {
    DefaultProfile string             `toml:"default_profile"`
    UI             UI                 `toml:"ui"`
    Agent          Agent              `toml:"agent"`
    Sandbox        SandboxConfig      `toml:"sandbox"`
    Profiles       map[string]Profile `toml:"profiles"`
}

type SandboxConfig struct {
    Driver    *string  `toml:"driver"`
    Network   *string  `toml:"network"`
    ReadPaths []string `toml:"read_paths"`
    AllowEnv  []string `toml:"allow_env"`
}
```

Pointers distinguish absent Driver/Network fields from explicit empty strings, which are invalid. `ResolveSandbox` accepts only absolute paths or `~/...` and exact unique environment names matching `[A-Za-z_][A-Za-z0-9_]*`; one textual path cannot exceed the final 32-KiB aggregate ceiling. Clone/sort paths and names and reject duplicate `allow_env` names. Canonical existence, nested-root collapse, at most 128 effective dynamic roots, and at most 32 KiB of aggregate canonical dynamic path text are enforced when a confining Driver is opened. A non-nil CLI Driver pointer overrides only the TOML Driver; network and lists remain TOML-owned.

- [ ] **Step 4: Add failing environment tests**

Create table-driven tests that prove:

1. parsing is deterministic and last duplicate entry wins;
2. malformed names, NUL bytes, and invalid UTF-8 fail closed;
3. provider names and `OTTO_API_KEY` are always removed and cannot be restored;
4. `DYLD_*`, `LD_*`, `BASH_ENV`, `ENV`, `ZDOTDIR`, `PROMPT_COMMAND`, `CDPATH`, `SHELLOPTS`, and `BASHOPTS` are non-restorable;
5. suffixes `_TOKEN`, `_SECRET`, `_PASSWORD`, `_PASSWD`, `_API_KEY`, `_ACCESS_KEY`, `_PRIVATE_KEY`, `_CREDENTIAL`, and `_CREDENTIALS` are classified case-insensitively;
6. fixed AWS/Azure/GCP/GitHub/GitLab/npm/registry/Docker/CI credentials, authorization/cookie names, plus `SSH_AUTH_SOCK`, `DOCKER_HOST`, and `CONTAINER_HOST` are classified;
7. exact `allow_env` restores a classified non-provider entry;
8. full raw/decoded URI userinfo plus nonempty username/password components from proxy values enter redactions while the proxy variable remains, including IPv6 authorities whose balanced brackets are preserved;
9. Seatbelt mode rewrites all private paths and adds the private root/home/tmp/cache values to exact redactions so `$HOME` output cannot reveal the runtime root; direct mode preserves host paths;
10. 513 unique sensitive environment values or more than 1 MiB of sensitive environment bytes fails with `ErrEnvironmentUnsafe`; the four fixed private-path redactions are added only after this bound is satisfied;
11. all outputs are cloned and sorted by environment name.

Use a helper that converts output entries back to a map without printing secret values in failures. Error assertions use only `ErrEnvironmentUnsafe`; malformed entry names/values and bound-triggering values must not appear in error text.

- [ ] **Step 5: Run environment tests and observe missing resolver failures**

Run:

```bash
go test ./internal/sandbox -run 'Test(ParseEnvironment|ResolveEnvironment)' -count=1
```

Expected: FAIL because environment APIs are undefined.

- [ ] **Step 6: Implement deterministic environment parsing and resolution**

Use these exact APIs:

```go
type EnvironmentOptions struct {
    HostEntries        []string
    ProviderNames      []string
    AllowNames         []string
    PrivateDirectories *PrivateDirectories
}

type EnvironmentSnapshot struct {
    entries     []string
    redactions  []string
}

func ParseEnvironment(entries []string) (map[string]string, error)
func ResolveEnvironment(options EnvironmentOptions) (EnvironmentSnapshot, error)
func (s EnvironmentSnapshot) Entries() []string
func (s EnvironmentSnapshot) RedactionValues() []string
```

For private mode apply this exact mapping:

```go
privateValues := map[string]string{
    "HOME":              directories.Home,
    "TMPDIR":            directories.Temp,
    "TMP":               directories.Temp,
    "TEMP":              directories.Temp,
    "XDG_CACHE_HOME":    filepath.Join(directories.Cache, "xdg"),
    "GOCACHE":           filepath.Join(directories.Cache, "go-build"),
    "GOMODCACHE":       filepath.Join(directories.Cache, "go-mod"),
    "NPM_CONFIG_CACHE": filepath.Join(directories.Cache, "npm"),
    "PIP_CACHE_DIR":    filepath.Join(directories.Cache, "pip"),
    "UV_CACHE_DIR":     filepath.Join(directories.Cache, "uv"),
}
```

Create derived cache directories with `0700`, verify owner read/write/execute bits, reject symlinks, and never call `syscall.Umask`. Collect credential/URI redaction values before removal/restoration and enforce their 512-value/1-MiB bound. Then add the four fixed private root/home/tmp/cache values, deduplicate, sort longest-first then lexical, and clone on access.

- [ ] **Step 7: Run focused and package tests, then commit**

Run:

```bash
gofmt -w internal/config/config.go internal/config/config_test.go internal/config/resolve.go internal/config/resolve_test.go internal/sandbox/environment.go internal/sandbox/environment_test.go
go test ./internal/config -run 'Test(LoadSandbox|ResolveSandbox)' -count=1
go test ./internal/sandbox -run 'Test(ParseEnvironment|ResolveEnvironment)' -count=1
go test -race ./internal/config ./internal/sandbox -count=1
git diff --check
git add internal/config internal/sandbox/environment.go internal/sandbox/environment_test.go
git commit -m "feat: resolve sandbox policy and environment"
```

Expected: all commands PASS.

---

### Task 3: Native Process Manager, Direct Driver, and Driver Conformance

**Files:**
- Create: `internal/sandbox/internal/nativeprocess/manager_unix.go`
- Create: `internal/sandbox/internal/nativeprocess/manager_other.go`
- Create: `internal/sandbox/internal/nativeprocess/manager_test.go`
- Create: `internal/sandbox/sandboxtest/conformance.go`
- Create: `internal/sandbox/direct/driver.go`
- Create: `internal/sandbox/direct/driver_test.go`

**Interfaces:**
- Consumes: Task 1 Driver/Request/Streams/ExitStatus/error contract.
- Produces: `direct.New`, reusable native process lifecycle, and `sandboxtest.RunDriverContract` for Seatbelt.

- [ ] **Step 1: Add failing barrier-driven native-process tests**

Use shell commands plus channel-signaling writers; do not sleep. Add:

```go
func TestManagerReportsZeroAndNonzeroExit(t *testing.T)
func TestManagerUsesNullStdinAndForwardsBothStreams(t *testing.T)
func TestManagerCancellationTerminatesAndReapsProcessGroup(t *testing.T)
func TestManagerDeadlineCancellationTerminatesAndReapsProcessGroup(t *testing.T)
func TestManagerTerminationFailurePreservesCancellationIdentity(t *testing.T)
func TestManagerNormalShellExitRemovesBackgroundDescendants(t *testing.T)
func TestManagerCloseTerminatesActiveGroupsAndRejectsNewRuns(t *testing.T)
func TestManagerConcurrentRunAndClose(t *testing.T)
```

For descendant checks, have the child print its PID through a synchronized writer, return from `Run`, then call signal `0` exactly once and require `ESRCH`. Use an unexported syscall dependency hook that performs the real kill and then reports a synthetic failure for the one termination-failure case; assert both the fixed infrastructure and cancellation identities without leaving a child. For deadline behavior, wait for the descendant-ready barrier and then cancel a context with `context.DeadlineExceeded` as its cause. `time.After` appears only in select statements that fail after a hard deadlock deadline.

- [ ] **Step 2: Run tests and observe the missing manager failure**

Run:

```bash
go test ./internal/sandbox/internal/nativeprocess -count=1
```

Expected: FAIL because `Manager` does not exist.

- [ ] **Step 3: Implement native process lifecycle**

Use an independent internal shape to avoid an import cycle:

```go
type Spec struct {
    Path        string
    Args        []string
    Directory   string
    Environment []string
    Stdout      io.Writer
    Stderr      io.Writer
}

type Result struct {
    Code     int
    Signaled bool
    Signal   string
}

type Manager struct {
    mu        sync.Mutex
    active    map[*process]struct{}
    closing   bool
    closeDone chan struct{}
    closeErr  error
}

func New() *Manager
func (m *Manager) Run(context.Context, Spec) (Result, error)
func (m *Manager) Close() error
```

On Unix, set `Setpgid: true` and connect stdin to `/dev/null`. Give the child real pipe file descriptors and forward stdout/stderr in owned `io.Copy` goroutines so parent exit is observable even while descendants hold inherited descriptors. On cancellation/close, immediately send `SIGKILL` to the process group, wait for the direct child, perform a final group kill, close pipe ends, join copy goroutines, and remove the active entry. After normal direct-child exit, perform the same final group kill before returning. Treat start/copy/wait/termination failures as fixed internal error identities without command, argv, environment, or OS error text. When termination infrastructure fails after context cancellation, join only the fixed termination identity with `ctx.Err()` so both identities remain discoverable without exposing an OS error.

The non-Unix file compiles and returns a bounded unsupported error; it must not invoke a command without group cleanup.

- [ ] **Step 4: Add the reusable conformance suite and failing Direct tests**

Define:

```go
package sandboxtest

type Fixture struct {
    Workspace   string
    OutsideFile string
    AllowedRead string
    UnixSocket  string
    Environment []string
    Policy      sandbox.Policy
}

type Case struct {
    NewDriver    func(testing.TB, Fixture) sandbox.Driver
    Request      func(testing.TB, Fixture, []string) sandbox.Request
    ShellCommand func(testing.TB, string) []string
    TCPClient    func(testing.TB, string) []string
    UnixClient   func(testing.TB, string) []string
}

func RunDriverContract(t *testing.T, testCase Case)
```

The suite constructs an Executor from `Fixture.Policy` and the fixture workspace, then checks both Executor validation and Driver behavior: ordinary exit, nonzero exit, separate streams, pre-cancellation, deadline cancellation, normal-exit background cleanup, close during execution, idempotent close, execute-after-close, request cloning, paths with spaces/Unicode/quotes, bounded errors, and concurrent calls. Its adapter also runs capability-specific canonical workspace, outside-root, symlink, filtered-environment, local TCP, and Unix-socket checks when a Driver advertises those capabilities.

Direct tests require `ID() == "direct"`, `NetworkAllow: true`, and every read/write/network-deny/Unix-socket-deny capability false. Constructing a Direct Driver is not sufficient authority: only an Executor resolved with `Policy{Filesystem: FilesystemUnconfined, Network: NetworkAllow}` accepts it.

- [ ] **Step 5: Run Direct tests and observe missing Driver failure**

Run:

```bash
go test ./internal/sandbox/direct -run 'TestDirect' -count=1
```

Expected: FAIL because `direct.New` is undefined.

- [ ] **Step 6: Implement the explicit Direct Driver**

Create package `internal/sandbox/direct` with:

```go
const ID sandbox.DriverID = "direct"

func New() sandbox.Driver
```

The Driver returns `ID`, advertises only `NetworkAllow`, delegates `request.Argv[0]` and `request.Argv[1:]` to its native Manager, maps native results to `ExitStatus`, and closes the Manager idempotently. It never calls `os.Environ`; only `request.Env` reaches the process. Executor capability checking, not the Driver request, binds the explicit unconfined policy.

- [ ] **Step 7: Run focused, conformance, race, and package tests; commit**

Run:

```bash
gofmt -w internal/sandbox/internal/nativeprocess internal/sandbox/sandboxtest internal/sandbox/direct
go test ./internal/sandbox/internal/nativeprocess -count=1
go test ./internal/sandbox/direct -run 'TestDirect' -count=1
go test -race ./internal/sandbox/internal/nativeprocess ./internal/sandbox/direct -run 'Test(Manager|Direct)' -count=1
git diff --check
git add internal/sandbox
git commit -m "feat: add direct sandbox driver lifecycle"
```

Expected: all commands PASS and no process remains from tests.

---

### Task 4: Private Seatbelt State and Closed-by-Default Profile Generation

**Files:**
- Create: `internal/sandbox/seatbelt/profile_v1.sb`
- Create: `internal/sandbox/seatbelt/profile.go`
- Create: `internal/sandbox/seatbelt/profile_test.go`
- Create: `internal/sandbox/seatbelt/state_unix.go`
- Create: `internal/sandbox/seatbelt/state_other.go`
- Create: `internal/sandbox/seatbelt/state_test.go`

**Interfaces:**
- Consumes: Task 1 `Policy`, `NetworkMode`, and `PrivateDirectories`.
- Produces: an immutable profile path/state handle and generated SBPL consumed by the Darwin Driver.

- [ ] **Step 1: Add failing state tests**

Add tests for exact `0700` root/home/tmp/cache/profiles modes, exact `0600` profile mode, state outside the workspace, canonical paths, owner UID, rejection of symlink candidates, fail-closed owner-bit stripping via injected file operations, idempotent no-follow cleanup, cleanup after partial construction failure, and a generated policy that grants child access to home/tmp/cache but not the state parent or profiles directory.

Inject one canonical user-cache base in tests. Prove construction succeeds when it is outside the workspace and fails closed when that cache base is inside the workspace; production does not fall back to a global temporary directory. Do not alter umask in tests or production.

- [ ] **Step 2: Add failing profile-generation tests**

Test these invariants:

- output begins with `(version 1)` and `(deny default)`;
- there is no unfiltered `(allow file-read*)`, `(allow file-write*)`, `(allow network-outbound)`, or `(allow network-inbound)`;
- workspace/home/tmp/cache are read/write subpaths while the state parent/profiles remain inaccessible;
- any automatic or explicit external read root that would contain the private state root is rejected fail-closed instead of exposing `profiles/`;
- every external read root is read-only;
- `NetworkAllow` emits `(remote ip)` and `(local ip)` filters, while `NetworkDeny` emits neither;
- arbitrary Unix socket, Apple Event, and Launch Services grants are absent;
- `/`, `/Users`, resolved user home, `/Applications`, `/Library`, `/Network`, `/Volumes`, `/dev`, `/private`, `/private/etc`, `/private/tmp`, `/private/var`, `/usr`, `/opt`, `/opt/homebrew`, and `/usr/local` are skipped when present as broad `PATH` entries; reviewed narrow descendants remain eligible;
- a configured shell is emitted as a literal file grant, not an automatic parent grant;
- only the already resolved user home expands `~/`; `$VAR`, `${VAR}`, command substitution, globbing, and other shell expansion remain literal and fail existence checks;
- `~/...` and explicit paths resolve canonically; regular files become literal grants, directories become subpath grants, and other file types are rejected;
- the configured shell resolves to one executable regular-file literal;
- quotes, backslashes, newlines, NUL, invalid UTF-8, symlinks, missing paths, and relative paths cannot inject SBPL;
- ancestor metadata needed to traverse an approved root does not grant ancestor file-content reads;
- after canonical duplicate/nested collapse, more than 128 effective dynamic roots or more than 32 KiB of aggregate canonical dynamic path text fails closed instead of truncating grants;
- two semantically equal inputs render byte-identical profiles.

- [ ] **Step 3: Run Seatbelt unit tests and observe missing constructors**

Run:

```bash
go test ./internal/sandbox/seatbelt -run 'Test(State|Profile)' -count=1
```

Expected: FAIL because state/profile code is absent.

- [ ] **Step 4: Implement owner-only state without process-wide mutation**

Use an unexported state handle:

```go
type state struct {
    directories sandbox.PrivateDirectories
    profiles    string
    profilePath string
    rootParent  string
    closeOnce   sync.Once
    closeErr    error
}

func createState(workspace, cacheBase string) (*state, error)
func (s *state) close() error
```

Canonicalize the resolved macOS user-cache base first, reject it when a state leaf would be inside the workspace, create a random owner-only `otto-sandbox-` leaf with `os.MkdirTemp`, verify `Lstat` regular directory/same UID/exact owner mode, then create child directories and profile with no-follow checks. Cleanup walks with `Lstat`/`ReadDir`, unlinks symlinks instead of following them, removes children before parents, and never reports internal paths outside local diagnostics.

- [ ] **Step 5: Implement reviewed root discovery and embedded SBPL**

Use `//go:embed profile_v1.sb` and replace four unique markers for read literals/subpaths, write subpaths, network rules, and shell literal. The template explicitly allows only reviewed process fork/exec/same-sandbox signal, bounded sysctl names, loader/runtime file roots, standard output/error/null/random devices, and the exact Mach services proven necessary by integration tests.

Canonicalize all dynamic paths, reject any root that contains the private state root, sort by path depth then lexical bytes, retain the parent when one directory contains another root or literal, and deduplicate identical canonical paths before enforcing the 128-root/32-KiB aggregate ceilings. Automatic roots are existing canonical entries from these classes:

```text
/bin, /sbin
/usr/bin, /usr/sbin, /usr/lib, /usr/libexec, /usr/share, /usr/include
/System
/Library/Apple, /Library/Developer
xcode-select's canonical Developer root (when present)
/opt/homebrew/{bin,sbin,lib,share,include,Cellar,opt}
/usr/local/{bin,sbin,lib,share,include,Cellar,opt,Homebrew}
canonical safe PATH entries
explicit read_paths
```

Automatic/fixed discovery must not add `/Applications`, `/Library`, `/Network`, `/Volumes`, `/dev`, `/private`, `/private/etc`, `/private/tmp`, `/private/var`, `/usr`, `/opt`, `/opt/homebrew`, or `/usr/local` broadly; it must not add `/opt/homebrew/etc`, `/opt/homebrew/var`, `/usr/local/etc`, `/usr/local/var`, filesystem root, `/Users`, or user home. Add only reviewed exact `/private/etc` runtime/certificate files required by local smoke tests. An explicit `read_paths` entry may intentionally name a broad root after canonical limit checks, except that any root containing private Sandbox state is rejected.

- [ ] **Step 6: Run unit/race tests and commit**

Run:

```bash
gofmt -w internal/sandbox/seatbelt/*.go
go test ./internal/sandbox/seatbelt -run 'Test(State|Profile)' -count=1
go test -race ./internal/sandbox/seatbelt -run 'Test(State|Profile)' -count=1
git diff --check
git add internal/sandbox/seatbelt
git commit -m "feat: generate Seatbelt workspace policy"
```

Expected: all platform-neutral tests PASS; Darwin-only execution is not introduced in this task.

---

### Task 5: Darwin Seatbelt Driver, Self-Test, and Security Matrix

**Files:**
- Create: `internal/sandbox/seatbelt/driver_darwin.go`
- Create: `internal/sandbox/seatbelt/driver_other.go`
- Create: `internal/sandbox/seatbelt/driver_other_test.go`
- Create: `internal/sandbox/seatbelt/driver_darwin_test.go`
- Modify: `internal/sandbox/seatbelt/profile_v1.sb`
- Modify: `internal/sandbox/seatbelt/profile.go`

**Interfaces:**
- Consumes: Task 3 native Manager/conformance suite and Task 4 profile/state.
- Produces: `seatbelt.Open`, `Driver.PrivateDirectories`, and a production Seatbelt `sandbox.Driver`.

- [ ] **Step 1: Add failing constructor/self-test tests**

On Darwin, test:

```go
func TestOpenUsesFixedSandboxExecAndPrivateProfile(t *testing.T)
func TestOpenRejectsMissingOrNonExecutableSandboxExec(t *testing.T)
func TestOpenRejectsProfileParseFailure(t *testing.T)
func TestOpenRejectsAllowedProbeFailure(t *testing.T)
func TestOpenRejectsDeniedReadOrWriteProbeSuccess(t *testing.T)
func TestOpenCancellationCleansState(t *testing.T)
func TestDriverCloseCleansStateAndIsIdempotent(t *testing.T)
func TestDriverSuppressesSandboxExecInfrastructureDiagnostics(t *testing.T)
func TestDriverInfrastructureFailurePoisonsLaterCalls(t *testing.T)
```

Production `Open` always uses `/usr/bin/sandbox-exec` and verifies with `Lstat` that it is a non-symlink, root-owned, executable regular file at that canonical path. It resolves state beneath the canonical macOS user cache directory and refuses a cache root contained by the workspace. Tests in package `seatbelt` use an unexported dependency struct to inject identity metadata, a probe runner, and the user-cache base. No public executable override exists.

- [ ] **Step 2: Add failing Driver conformance and security tests**

Run `sandboxtest.RunDriverContract` and add fixture tests for:

1. workspace read/write;
2. denied sibling read/write;
3. private home/tmp/cache writes;
4. denied host home and host caches;
5. absolute and relative symlink escapes;
6. local IPv4/IPv6 TCP in allow mode;
7. local TCP rejection in deny mode;
8. fake Docker and SSH-agent Unix sockets placed inside the writable workspace are still rejected in allow mode, proving the network operation—not only path access—is denied;
9. `/usr/bin/open` and `/usr/bin/osascript` rejection, and a nested permissive `sandbox-exec` cannot widen the inherited profile;
10. a sandboxed shell can signal its own child but cannot signal a test-owned host process outside the policy;
11. normal-exit, cancel, timeout, and close process-group cleanup;
12. `/bin/sh`, `/usr/bin/git --version`, and `git status --porcelain` in a test-owned repository without reading host Git config;
13. conditional `xcrun --find clang` plus trivial compile;
14. conditional canonical Homebrew `go version` plus a dependency-free temporary-module `go test` using private caches with `GOTOOLCHAIN=local`, `GOPROXY=off`, `GOSUMDB=off`, and `GOTELEMETRY=off`.

All networking uses loopback listeners created by the test process. Unix socket tests use test-owned socket paths. Toolchain tests skip only when the toolchain is absent.

- [ ] **Step 3: Run the Darwin tests and observe missing Driver failures**

Run:

```bash
go test ./internal/sandbox/seatbelt -run 'Test(Open|Driver|Seatbelt)' -count=1
```

Expected: FAIL because `Open` and the Driver are undefined.

- [ ] **Step 4: Implement the Driver and self-test**

Use:

```go
type Options struct {
    Workspace     string
    Shell         string
    Home          string
    HostEntries   []string
    ReadPaths     []string
    Network       sandbox.NetworkMode
}

func Open(ctx context.Context, options Options) (*Driver, error)
func (d *Driver) ID() sandbox.DriverID
func (d *Driver) PrivateDirectories() sandbox.PrivateDirectories
func (d *Driver) Capabilities() sandbox.Capabilities
func (d *Driver) Execute(context.Context, sandbox.Request, sandbox.Streams) (sandbox.ExitStatus, error)
func (d *Driver) Close() error
```

Return `ID() == "seatbelt"` and advertise read/write confinement, Unix-socket denial, and only the network capability selected when that Driver instance was constructed (`NetworkAllow` xor `NetworkDeny`). Bind the canonical workspace, configured-shell read grant, network mode, profile path, and state at construction; reject a request whose `Dir` is outside the bound workspace. Execute the validated `Argv` exactly, so the Driver contract remains backend-neutral even though Phase 1 Bash supplies the configured shell. Invoke through the native Manager with no extra shell:

```go
argv := append([]string{"-f", d.profilePath, "--"}, request.Argv...)
result, err := d.processes.Run(ctx, nativeprocess.Spec{
    Path:        "/usr/bin/sandbox-exec",
    Args:        argv,
    Directory:   request.Dir,
    Environment: append([]string(nil), request.Env...),
    Stdout:      streams.Stdout,
    Stderr:      filteredStderr,
})
```

Do not reconstruct command text or invoke an extra shell. Buffer at most the first 4 KiB stderr line until it is known not to begin with a Sandbox infrastructure signature. Suppress matching diagnostics and return a fixed safe error; stream ordinary command stderr unchanged.

Self-test uses a child context capped at five seconds, collectors capped at 8 KiB, and a minimal fixed environment (`PATH=/usr/bin:/bin`, private `HOME`/`TMPDIR`, fixed locale) that never inherits `HostEntries`. It parses the embedded profile, proves workspace/private writes, proves a denied sibling cannot be read or written, and checks the caller/child contexts immediately before each subprocess; it never opens a network connection. A failed self-test closes the Manager and removes state before returning a reason-only `UnavailableError`. If a runtime profile/launcher infrastructure failure makes confinement ambiguous, latch a poisoned state under the Driver mutex; every later call returns the same bounded unavailable class without starting a child.

The non-Darwin constructor returns `ReasonUnsupportedPlatform` and never constructs direct execution.

- [ ] **Step 5: Tighten only grants proven necessary by failed compatibility tests**

For each compatibility failure, first add a test that identifies the exact denied operation/root. Add the narrowest literal/subpath/Mach/sysctl rule and a comment naming the command that requires it. Reject any proposed broad home, `/Applications`, `/usr/local`, `/opt/homebrew`, unfiltered network, Apple Event, Launch Services, or Unix-socket grant.

- [ ] **Step 6: Run repeated security/conformance/race tests and commit**

Run:

```bash
gofmt -w internal/sandbox/seatbelt/*.go
go test ./internal/sandbox/seatbelt -count=1
go test ./internal/sandbox/seatbelt -run 'Test(Open|Driver|Seatbelt)' -count=10
go test -race ./internal/sandbox/seatbelt -count=1
git diff --check
git add internal/sandbox/seatbelt
git commit -m "feat: add Seatbelt sandbox driver"
```

Expected on Darwin: all tests PASS. On non-Darwin: profile unit tests PASS and Driver construction reports the typed unsupported reason.

---

### Task 6: Add the Executor-Backed Bash Adapter

**Files:**
- Modify: `internal/tool/bash.go`
- Modify: `internal/tool/bash_test.go`
- Modify: `internal/tool/result.go`
- Modify: `internal/tool/registry_test.go`

**Interfaces:**
- Consumes: Task 1 `sandbox.CommandExecutor`, `Request`, `Streams`, and `ExitStatus`.
- Produces: `NewSandboxedBashTool`, an Executor-backed adapter with no direct environment, process, signal, or process-group ownership; Task 8 performs the atomic production call-site migration and removes the temporary legacy constructor.

- [ ] **Step 1: Add failing fake-Executor tests beside the legacy tests**

Define a fake Executor that captures a cloned `sandbox.Request`, writes configured stream chunks, blocks on channels, observes context cancellation, and returns a configured status/error. Cover:

- exact `[]string{shell, "-lc", command}`, workspace directory, and complete environment forwarding;
- stdout/stderr independent caps;
- exact secrets split across writes and redacted before truncation;
- exit code and signal formatting;
- caller-cancellation versus injected timeout-cancellation formatting;
- pre-canceled context without child execution;
- fixed safe infrastructure error text with no raw fake error;
- nil and reflection-detected typed-nil Executor, empty/NUL shell, invalid timeout/output bounds, and invalid workspace rejected by construction/registry wiring;
- concurrent Execute calls do not share collectors or redactor state.

Give `bashTool` an unexported `deadlineContext(context.Context, time.Duration, error) (context.Context, context.CancelFunc)` factory initialized to `context.WithTimeoutCause`; tests replace it with a channel-controlled `context.WithCancelCause` so timeout assertions use an event barrier rather than wall-clock timing.

Move descendant-lifecycle assertions for the new adapter to Task 3/5 tests. Keep the existing process-backed tests unchanged in this commit so every current call site and the whole repository remain green until Task 8's atomic migration; add no new sleeps.

- [ ] **Step 2: Run focused Bash tests and observe constructor/signature failures**

Run:

```bash
go test ./internal/tool -run 'TestBash' -count=1
```

Expected: FAIL because the tests require Executor delegation.

- [ ] **Step 3: Refactor Bash to a bounded adapter**

Use this constructor:

```go
func NewSandboxedBashTool(
    workspace *Workspace,
    executor sandbox.CommandExecutor,
    shell string,
    environment []string,
    timeout time.Duration,
    maxOutputBytes int,
    redactionValues []string,
) (Tool, error)
```

Clone the complete filtered environment at construction. `Execute` validates JSON/command, creates independent capped collectors and exact-redacting writers, derives a timeout context, and calls:

```go
status, err := t.executor.Execute(commandCtx, sandbox.Request{
    Argv: []string{t.shell, "-lc", args.Command},
    Dir:  t.workspace.root,
    Env:  append([]string(nil), t.environment...),
}, sandbox.Streams{Stdout: redactedStdout, Stderr: redactedStderr})
```

Immediately call the timeout cancel function after Executor return so a successful result wins over a not-yet-fired deadline. Driver/Executor infrastructure errors take precedence even when a context ended: discard captured diagnostics and return only `sandbox execution unavailable` with `IsError: true`, without formatting `err`. The only non-error exception is an exact `context.Canceled`/`context.DeadlineExceeded` returned by Executor pre-cancellation with no Sandbox infrastructure identity. For successful cleanup (`err == nil`) or that exact pre-start exception, flush writers, distinguish parent cancellation from the unique timeout cause, and render the existing non-error cancellation/timeout result with `status.Signal` when `status.Signaled`. Preserve the existing `stdout:`, `stderr:`, truncation, and `exit_code:` layout.

Implement the adapter as a distinct internal type. Keep the existing `NewBashTool`, `BashSecurity`, and process-backed implementation explicitly unchanged and marked for Task 8 removal; no production call site uses the new adapter yet. Keep timeout selection in the adapter through the injected deadline-context factory; the Driver reacts only to context cancellation.

- [ ] **Step 4: Add one Direct integration test**

Build `direct.New()` plus `sandbox.NewExecutor(driver, unconfinedPolicy, canonicalWorkspace)` with a deterministic environment snapshot supplied to `NewSandboxedBashTool`. Assert cwd, environment, exit code, and stream formatting. Close the Executor in `t.Cleanup`.

- [ ] **Step 5: Run focused/race/package tests and commit**

Run:

```bash
gofmt -w internal/tool/bash.go internal/tool/bash_test.go internal/tool/result.go internal/tool/registry_test.go
go test ./internal/tool -run 'TestBash' -count=1
go test -race ./internal/tool -run 'TestBash' -count=1
go test ./internal/tool -count=1
git diff --check
git add internal/tool
git commit -m "feat: add sandbox-backed bash adapter"
```

Expected: all commands PASS. The temporary legacy process code still exists only to keep pre-migration call sites green; Task 8 must remove it, and Task 9's source gate fails if any direct process ownership remains in `internal/tool/bash.go`.

---

### Task 7: Sanitized App State, Dynamic Prompt, REPL, and TUI Presentation

**Files:**
- Modify: `internal/app/controller.go`
- Modify: `internal/app/controller_test.go`
- Modify: `internal/repl/repl.go`
- Modify: `internal/repl/repl_test.go`
- Modify: `internal/tui/layout.go`
- Modify: `internal/tui/layout_test.go`
- Modify: `internal/tui/model_test.go`
- Modify: `cmd/otto/main.go`
- Create: `cmd/otto/system_prompt_test.go`

**Interfaces:**
- Consumes: no raw Sandbox object; receives only fixed enums/booleans from CLI wiring.
- Produces: `app.SandboxInfo`, shared safe summaries/badges, and `systemPromptFor` consumed by runtimeBuilder.

- [ ] **Step 1: Add failing app-state tests**

Define expected values with closed string enums:

```go
type SandboxMode string
type SandboxNetwork string
type SandboxReason string

const (
    SandboxSeatbelt    SandboxMode = "seatbelt"
    SandboxOff         SandboxMode = "off"
    SandboxUnavailable SandboxMode = "unavailable"

    SandboxNetworkAllowed    SandboxNetwork = "allowed"
    SandboxNetworkDenied     SandboxNetwork = "denied"
    SandboxNetworkUnconfined SandboxNetwork = "unconfined"

    SandboxReasonNone                SandboxReason = ""
    SandboxReasonUnsupportedPlatform SandboxReason = "unsupported-platform"
    SandboxReasonSeatbeltMissing      SandboxReason = "seatbelt-missing"
    SandboxReasonSelfTestFailed       SandboxReason = "self-test-failed"
    SandboxReasonRuntimeFailure        SandboxReason = "runtime-failure"
    SandboxReasonInvalidShell         SandboxReason = "invalid-shell"
    SandboxReasonEnvironmentRejected  SandboxReason = "environment-rejected"
    SandboxReasonPolicyUnsupported    SandboxReason = "policy-unsupported"
)

type SandboxInfo struct {
    Mode          SandboxMode
    Network       SandboxNetwork
    BashAvailable bool
    Reason        SandboxReason
}
```

Test `RuntimeInfo -> Info`, successful `/new` and `/resume`, failed replacement rollback, value copying, invalid-enum fallback rendering, and concurrency. Sandbox status is process-level and remains identical across every session replacement.

- [ ] **Step 2: Add failing summary/prompt/frontend tests**

Require exact summaries:

```text
seatbelt · workspace-write · network allowed
seatbelt · workspace-write · network denied
sandbox off · WARNING: bash is unsandboxed
bash disabled · sandbox unavailable
```

Require badges `sb`, `unsafe`, and `no-bash`. Add tests that:

- Seatbelt prompt names only workspace-write and effective network mode;
- off prompt explicitly says Bash is unsandboxed;
- unavailable prompt omits Bash from the usable-tool list;
- no prompt includes unavailable raw reason text;
- REPL interactive startup and `/session` show the summary, and `/session` shows the fixed reason code when unavailable;
- `RunOnce` still emits no banner and treats `/session` as a provider prompt;
- TUI footer includes the badge whenever it fits without violating the minimum layout;
- TUI help and the existing session overlay include `Sandbox:`;
- control characters in fields are escaped.

- [ ] **Step 3: Run focused tests and observe missing status failures**

Run:

```bash
go test ./internal/app ./internal/repl ./internal/tui ./cmd/otto -run 'Test.*(Sandbox|SystemPrompt)' -count=1
```

Expected: FAIL because Sandbox info and rendering do not exist.

- [ ] **Step 4: Implement shared safe state and replacement preservation**

Add `Sandbox SandboxInfo` to both `app.RuntimeInfo` and `app.Info`. Implement:

```go
func (s SandboxInfo) Summary() string
func (s SandboxInfo) Badge() string
```

using exhaustive switches that return only fixed literals. `Controller.Info` copies the value under the existing lifecycle lock. Replacement builders preserve the process-level value and never derive it from session JSONL.

- [ ] **Step 5: Implement frontend rendering and dynamic prompt**

The REPL prints `Sandbox: ` plus `info.Sandbox.Summary()` after the logo and session ID. `/session` appends the same field plus `Sandbox reason: ` followed by `info.Sandbox.Reason` only when unavailable. `RunOnce` remains unchanged.

The TUI footer inserts the fixed badge before optional usage/session fields and retains it while dropping lower-priority fields. `helpOverlayContent` adds the shared summary; `sessionOverlayContent` also adds the fixed unavailable reason code. Both preserve bounded layouts. Do not add `/status`.

Replace the constant prompt with:

```go
func systemPromptFor(definitions []model.ToolDefinition, info app.SandboxInfo) string
```

Derive the usable-tool list from the actual registry definitions in their stable order, then append one fixed Sandbox policy sentence selected from `SandboxInfo`. The function discloses no internal state path, profile, environment name/value, self-test diagnostic, or raw error.

- [ ] **Step 6: Run focused/full frontend tests and commit**

Run:

```bash
gofmt -w internal/app/controller.go internal/app/controller_test.go internal/repl/repl.go internal/repl/repl_test.go internal/tui/layout.go internal/tui/layout_test.go internal/tui/model_test.go cmd/otto/main.go cmd/otto/system_prompt_test.go
go test ./internal/app ./internal/repl ./internal/tui ./cmd/otto -run 'Test.*(Sandbox|SystemPrompt)' -count=1
go test ./internal/app ./internal/repl ./internal/tui -count=1
go test -race ./internal/app ./internal/repl ./internal/tui -run 'Test.*Sandbox' -count=1
git diff --check
git add internal/app internal/repl internal/tui cmd/otto/main.go cmd/otto/system_prompt_test.go
git commit -m "feat: expose sandbox status in frontends"
```

Expected: all commands PASS.

---

### Task 8: CLI Selection, Environment Snapshot, Runtime Wiring, and Fail-Closed Lifecycle

**Files:**
- Create: `cmd/otto/sandbox_runtime.go`
- Create: `cmd/otto/sandbox_runtime_test.go`
- Modify: `cmd/otto/main.go`
- Modify: `cmd/otto/main_test.go`
- Modify: `cmd/otto/runtime_builder.go`
- Modify: `cmd/otto/runtime_builder_test.go`
- Modify: `cmd/otto/tui_pty_test.go`
- Modify: `internal/config/config.go`
- Modify: `internal/config/config_test.go`
- Modify: `internal/tool/bash.go`
- Modify: `internal/tool/bash_test.go`
- Modify: `internal/agent/agent_test.go`

**Interfaces:**
- Consumes: Tasks 1–7.
- Produces: one process-level Sandbox runtime shared by startup, `/new`, and `/resume`; optional Bash registration; sanitized status and warnings.

- [ ] **Step 1: Add failing Sandbox runtime selection tests**

Use an injected Seatbelt opener and fake Driver to prove:

```go
func TestOpenSandboxAutoSelectsSeatbeltOnDarwin(t *testing.T)
func TestOpenSandboxExplicitSeatbeltUsesSeatbelt(t *testing.T)
func TestOpenSandboxFailureDisablesBashWithoutDirectFallback(t *testing.T)
func TestOpenSandboxOffUsesOnlyDirect(t *testing.T)
func TestOpenSandboxOffIgnoresNetworkDenyAndReadPathsAndReportsUnsafe(t *testing.T)
func TestOpenSandboxEnvironmentFailureClosesDriverAndDisablesBash(t *testing.T)
func TestOpenSandboxProviderCredentialsAreNonRestorable(t *testing.T)
func TestOpenSandboxCloseWaitsForActiveExecutionAndCleansState(t *testing.T)
```

The fake opener records calls; the no-fallback test requires zero Direct construction calls.

- [ ] **Step 2: Implement production runtime selection**

Use these cmd-local types:

```go
type sandboxOpenOptions struct {
    Settings      sandbox.Settings
    Workspace     string
    Shell         string
    Home          string
    HostEntries   []string
    ProviderNames []string // selected/resolved plus every configured profile credential name
}

type sandboxRuntime struct {
    Executor        sandbox.CommandExecutor
    Environment     []string
    Info            app.SandboxInfo
    RedactionValues []string
    close           func() error
}

func openSandboxRuntime(context.Context, sandboxOpenOptions) sandboxRuntime
func (r *sandboxRuntime) Close() error
```

For Seatbelt: open the concrete Driver, obtain private directories, resolve the complete environment, and construct `sandbox.NewExecutor(driver, workspaceWritePolicy, canonicalWorkspace)`; close partial resources on every failure. For off: resolve a complete direct environment without private directories, call `direct.New()`, and construct `sandbox.NewExecutor(driver, unconfinedNetworkAllowPolicy, canonicalWorkspace)`. Every unavailable result has nil Executor and nil command environment, a fixed `app.SandboxInfo`, cloned redactions when safe, and an idempotent close function.

- [ ] **Step 3: Add failing CLI/config/help tests**

Add tests for:

- `--sandbox auto|seatbelt|off` parsing and invalid values;
- CLI-over-TOML Driver precedence;
- help text describing `auto -> Seatbelt` and explicit unsafe off;
- old configs defaulting to auto;
- unsupported `docker` rejection;
- one environment-enumerator call per process, including when captured `HOME` is absent, with an injected OS-user-home fallback and no later `os.Getenv`/`os.UserHomeDir` read;
- required-path config loading that does not re-read the process environment while preserving the public default-path compatibility helper;
- invalid shell disabling Bash instead of stopping file tools;
- successful Seatbelt adds no headless status noise;
- unavailable Seatbelt fixed warning on stderr;
- off fixed warning in headless, REPL, and TUI modes;
- Executor close after controller close and after startup errors;
- no Bash definition when unavailable and all six file-tool definitions retained;
- one Executor pointer reused across startup, `/new`, and `/resume`;
- a PTY test with a ready-barrier fake Seatbelt Executor proving Ctrl+C cancels active Bash and restores the terminal without starting a host child.

Refactor the test seam from `func(string) string` to an injected enumerator:

```go
func testEnviron(values map[string]string) func() []string {
    return func() []string {
        names := make([]string, 0, len(values))
        for name := range values { names = append(names, name) }
        sort.Strings(names)
        entries := make([]string, 0, len(names))
        for _, name := range names { entries = append(entries, name+"="+values[name]) }
        return entries
    }
}
```

Define `type environmentEnumerator func() []string`; production passes `os.Environ`, and `runWithDependencies` calls it exactly once. Add an injected OS-user-home resolver (production uses `os/user.Current`, not another environment lookup) for the captured-`HOME`-absent case. Add a required-path config loader that never calls `DefaultPath`; keep the existing environment-reading `DefaultPath` compatibility helper out of the production startup path. A cmd-local bounded lookup map built from valid entries resolves HOME/config/provider/UI values. The strict `sandbox.ResolveEnvironment` independently examines the same immutable slice; a malformed entry disables Bash without preventing Otto from using unrelated valid provider/config entries. Update every cmd test/PTY call site to provide `testEnviron` explicitly. Add a cmd-test `runForTest`/`withTestSandbox` helper that injects a deterministic fake Seatbelt runtime and never starts a host process. Mechanically route unrelated legacy cmd tests through that helper. Tests intentionally exercising direct Bash select explicit off and assert its warning; focused auto/Seatbelt CLI tests inject outcome-specific openers. Real Seatbelt enforcement remains concentrated in Darwin package tests.

- [ ] **Step 4: Run focused CLI/runtime tests and observe wiring failures**

Run:

```bash
go test ./cmd/otto -run 'Test(OpenSandbox|Run.*Sandbox|RuntimeBuilder.*Sandbox|RunHelp)' -count=1
```

Expected: FAIL because CLI and runtimeBuilder do not own the Sandbox runtime.

- [ ] **Step 5: Wire startup and lifecycle in `main.go`**

Add `sandbox string` and `sandboxSet bool` to `cliOptions`. Resolve the canonical workspace, capture/parse environment once, resolve home/config/UI/provider/Sandbox settings, then open the Sandbox runtime before sessions/runners.

Skip shell validation when Bash is unavailable. Treat shell validation failure as `ReasonInvalidShell`, close any partial Seatbelt state, and continue with file tools. Print only fixed warnings:

```go
fmt.Fprintf(stderr, "warning: bash is unavailable because the configured sandbox could not be established (reason: %s); file tools remain available\n", sandboxInfo.Reason)
fmt.Fprintln(stderr, "warning: sandbox is off; bash runs unsandboxed as your macOS user")
```

The unavailable formatter accepts only an `app.SandboxReason` constant, never raw constructor text. After frontend exit, cancel the process context, close the Controller (which waits for active Agent/Bash work), and then close the Sandbox runtime even if Controller close failed. Preserve the Controller error as primary; otherwise report a bounded Sandbox close class. Early-startup defers close partial runtimes exactly once. A Sandbox close failure is a bounded local CLI error and never a tool/Agent event.

- [ ] **Step 6: Refactor runtimeBuilder for optional Bash and shared redaction**

Replace Bash security-name calculation with fields:

```go
commandExecutor    sandbox.CommandExecutor
sandboxEnvironment []string
sandboxInfo        app.SandboxInfo
sandboxSecrets     []string
```

Build file tools unconditionally. Append Bash only when `commandExecutor != nil`, initially using Task 6's `NewSandboxedBashTool` with a cloned `sandboxEnvironment`. After `tool.NewRegistry`, call `systemPromptFor(registry.Definitions(), sandboxInfo)` so prompt and registered tools cannot drift. Combine cloned Sandbox redactions with provider API key and endpoint userinfo/query/fragment values before constructing both the Bash Tool and `agent.Redactor`. Keep one Executor and one immutable environment snapshot across all runner/session replacements.

Every `app.RuntimeInfo` emitted by startup, `buildNewReplacement`, and `openReplacement` includes the immutable process Sandbox info.

After every production and test call site uses the Executor-backed adapter, delete the temporary process-backed `NewBashTool`, `BashSecurity`, environment filtering, `os/exec`, signal, and process-group code plus its obsolete tests. Rename `NewSandboxedBashTool` to the final `NewBashTool`. Update `internal/agent/agent_test.go` to construct a test-owned Direct Executor with deterministic filtered environment and close it with `t.Cleanup`; production `internal/agent` remains unaware of Sandbox. Run a source grep before committing so this migration cannot leave a callable direct path.

- [ ] **Step 7: Add end-to-end credential and fail-closed tests**

Update the existing all-profile credential test to pass a complete deterministic environment snapshot containing active/inactive/fallback credentials and one unrelated variable. Assert protected names/values are absent from Bash, stdout/stderr, provider follow-up, Agent events, and Pi JSONL while the unrelated variable remains.

Add a fake-provider test where unavailable Sandbox means the request tool definitions omit Bash. If the provider nevertheless calls `bash`, the result is the existing bounded unknown-tool error; no direct process runs.

- [ ] **Step 8: Run cmd/runtime/package/race/PTY tests and commit**

Run:

```bash
gofmt -w cmd/otto/*.go internal/config/config.go internal/config/config_test.go internal/tool/bash.go internal/tool/bash_test.go internal/agent/agent_test.go
go test ./cmd/otto -run 'Test(OpenSandbox|Run.*Sandbox|RuntimeBuilder.*Sandbox|RunKeepsEveryProfileCredential|RunHelp)' -count=1
go test ./cmd/otto -count=1
go test ./internal/tool ./internal/agent -count=1
go test -race ./cmd/otto ./internal/tool ./internal/agent -count=1
go test ./cmd/otto -run 'TestTUIPseudoTerminal(Lifecycle|CancelsSandboxedBash)' -count=1
! rg -n 'os/exec|os\.Environ|exec\.Command|Setpgid|syscall\.Kill' internal/tool/bash.go
git diff --check
git add cmd/otto internal/config/config.go internal/config/config_test.go internal/tool internal/agent/agent_test.go
git commit -m "feat: wire sandbox lifecycle into CLI"
```

Expected: all commands PASS.

---

### Task 9: Documentation, Driver Author Guide, Acceptance Tests, and Final Security Gates

**Files:**
- Modify: `README.md`
- Modify: `docs/user-manual.md`
- Create: `docs/sandbox-driver-authoring.md`
- Modify: `AGENTS.md`
- Modify: `cmd/otto/main_test.go`
- Modify: `cmd/otto/tui_pty_test.go`
- Modify: `internal/sandbox/seatbelt/driver_darwin_test.go`
- Modify: `internal/sandbox/sandboxtest/conformance.go`

**Interfaces:**
- Consumes: completed implementation.
- Produces: user-facing Stage 1 documentation, backend-author requirements, and whole-feature acceptance evidence.

- [ ] **Step 1: Add final failing acceptance assertions**

Add a single table/checklist test layer that verifies:

- default Darwin auto status is Seatbelt;
- denied external file read/write including symlink;
- workspace/private writes;
- default local IP networking and deny mode;
- Unix socket denial;
- no fallback after forced Seatbelt failure;
- explicit off status and execution;
- environment limits disable Bash;
- normal/cancel/timeout/close background cleanup;
- REPL/TUI/headless status/warnings;
- no Sandbox paths, profile text, raw errors, credentials, or authorization/cookie values in model requests or sessions.

Use fixtures and barriers already created in earlier tasks; do not duplicate process helpers.

- [ ] **Step 2: Run acceptance tests and fix only concrete failures**

Run:

```bash
go test ./internal/sandbox/... ./internal/tool ./internal/app ./internal/repl ./internal/tui ./cmd/otto -run 'Test.*(Sandbox|Seatbelt|Direct|Bash)' -count=1
```

Expected before final corrections: any failure names one unmet acceptance behavior. Make the smallest production/test change that satisfies that named behavior, then rerun until PASS.

- [ ] **Step 3: Replace unsandboxed documentation with exact Stage 1 behavior**

Document in README and user manual:

```toml
[sandbox]
driver = "auto"
network = "allow"
read_paths = []
allow_env = []
```

Include:

- `--sandbox auto|seatbelt|off` precedence and that only the global config is auto-discovered;
- a warning to review any explicitly selected project-owned config because it can request off/read/environment grants on the next process start;
- Darwin `auto -> seatbelt`, never Docker detection;
- fail-closed Bash disablement while file tools remain;
- persistent unsafe warning for off;
- whole-workspace write authority;
- selected external read roots and private home/temp/cache, with an explicit warning that broad `read_paths` expose those contents to command code/default networking and are rejected if they would contain Otto's private Sandbox state;
- default network exfiltration implications, deny mode, and the absence of domain allowlisting in Phase 1;
- exact environment filtering/restoration behavior, the 512-value/1-MiB fail-closed bound, and the fact that transformed/encoded secrets cannot be redacted reliably;
- `allow_env` as a high-risk grant of the restored value to untrusted command code;
- Git/user dotfiles and host caches not automatically available;
- pairing narrow `read_paths` with tool-specific absolute config environment variables when needed;
- Seatbelt's deprecated `/usr/bin/sandbox-exec` dependency and same-user/same-kernel, hard-link, `setsid`, resource-exhaustion, and workspace-destruction limitations;
- Docker/Apple Container as planned Drivers, not supported Stage 1 drivers;
- remediation for unavailable Seatbelt, including the fail-closed case where the selected workspace contains Otto's user-cache Sandbox root, and the explicit risk of off.

Update `AGENTS.md` package ownership with `internal/sandbox` and change the Bash rule from unrestricted execution to Driver delegation with off as the sole direct path.

- [ ] **Step 4: Create and link the Driver author guide**

Create `docs/sandbox-driver-authoring.md`, link it from the user manual, and document that a future backend must:

- implement the exact Task 1 Driver interface;
- defensively copy requests, never retain request slices/contexts/writers after return, and report truthful capabilities;
- enforce all-or-nothing policy before command start;
- isolate stdout/stderr and return ordinary nonzero exits as status;
- terminate descendants on normal exit/cancel/timeout/close to the backend's documented guarantee;
- use bounded safe errors and never expose control-plane credentials/endpoints;
- pass `sandboxtest.RunDriverContract` plus capability-specific filesystem/network tests;
- avoid host Docker/Podman sockets, SSH agents, host home, untrusted project Dockerfiles, and mutable image tags.

- [ ] **Step 5: Run formatting, scope, leak, and no-sleep checks**

Run:

```bash
test -z "$(gofmt -l .)"
git diff --check
base=$(git merge-base HEAD main)
test -z "$(git diff "$base"...HEAD -- go.mod go.sum)"
! rg -n 'os\.Environ|exec\.Command|Setpgid|syscall\.Kill' internal/tool/bash.go
! rg -n 'os\.Environ|\.Umask' internal/sandbox --glob '!**/*_test.go'
! rg -n 'os\.(Getenv|LookupEnv|UserHomeDir)' cmd/otto --glob '!**/*_test.go'
test "$(rg -n 'os\.Environ' cmd/otto --glob '!**/*_test.go' | wc -l | tr -d ' ')" -eq 1
! rg -n 'time\.Sleep|Eventually|Consistently' internal/sandbox internal/tool/bash_test.go cmd/otto/sandbox_runtime_test.go
! rg -n "Authorization:|Cookie:|Bearer [A-Za-z0-9]|api[_-]?key\\s*=\\s*['\"][^'\"]+" internal/sandbox cmd/otto README.md docs/user-manual.md docs/sandbox-driver-authoring.md
! rg -n 'driver\s*=\s*"docker"|Docker (Driver )?is supported|supports Docker' README.md docs/user-manual.md docs/sandbox-driver-authoring.md
! rg -n 'github.com/baiyuqing/otto/internal/sandbox' internal/agent internal/provider internal/session internal/memory internal/model --glob '!**/*_test.go'
```

Expected: every command exits zero and prints no secret/scope violations.

- [ ] **Step 6: Run complete offline verification**

Run fresh:

```bash
go test -count=1 ./...
go test -race -count=1 ./...
go test ./internal/sandbox/seatbelt -run 'Test(Open|Driver|Seatbelt)' -count=10
go test ./cmd/otto -run 'TestTUIPseudoTerminal(Lifecycle|CancelsSandboxedBash)' -count=1
CGO_ENABLED=0 go test -count=1 ./...
go vet ./...
go build -trimpath -o ./otto ./cmd/otto
```

Expected: all commands PASS with no network or credentials.

- [ ] **Step 7: Run pinned static analysis and cross-platform compile checks**

Run:

```bash
go run honnef.co/go/tools/cmd/staticcheck@v0.8.1 ./...
GOOS=linux GOARCH=amd64 go build ./internal/sandbox/... ./cmd/otto
```

Expected: cross-platform build PASS. Staticcheck may report only the documented pre-existing `internal/repl/repl.go: const ottoMark is unused (U1000)`; any Sandbox finding must be fixed and the command rerun.

Remove the verification binary and verify repository hygiene:

```bash
rm -f ./otto
test -z "$(git status --short | grep -E '(^|/)(otto|.*\.test|coverage.*|.*\.out)$' || true)"
git diff --check
```

- [ ] **Step 8: Commit documentation and acceptance closure**

```bash
git add README.md docs/user-manual.md AGENTS.md cmd/otto internal/sandbox internal/tool internal/app internal/repl internal/tui
git commit -m "docs: document macOS sandbox boundaries"
git status --short --branch
```

Expected: the feature worktree is clean and the final commit contains only intentional Sandbox/documentation changes.
