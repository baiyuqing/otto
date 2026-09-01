# macOS Sandbox Driver Design

**Date:** 2026-08-31

**Status:** Approved direction; implementation not started

## Summary

Otto currently confines `read`, `grep`, `find`, `ls`, `write`, and `edit` to the selected workspace, but its `bash` tool only starts in that directory. A shell command still runs as the current macOS user and can read, modify, or delete anything that user can access.

This design adds a stable, backend-neutral command-execution boundary under `internal/sandbox`. Phase 1 implements macOS Seatbelt through `/usr/bin/sandbox-exec` and retains direct execution only behind an explicit `off` setting. Docker, Apple Container, Podman, Linux, remote sandboxes, domain-filtering proxies, and interactive permission escalation remain later work.

On macOS, the default is `auto`, which resolves to Seatbelt. Seatbelt failure never falls back to direct execution. Otto remains usable without `bash`: file tools stay registered, while local status and the dynamic system prompt accurately report that shell execution is unavailable.

The default Seatbelt policy:

- allows the entire canonical workspace to be read and written;
- permits only selected system and toolchain roots outside the workspace, read-only;
- gives the command a private `HOME`, temporary directory, and cache root;
- denies all other filesystem reads and writes;
- permits ordinary IP networking by default;
- denies Unix-domain sockets, Apple Events, Launch Services, and inherited control channels;
- removes provider credentials and automatically filters likely secret environment variables.

Seatbelt limits blast radius but is not a VM boundary. The design makes that distinction explicit.

## Goals

1. Prevent `bash` commands from reading or modifying arbitrary files outside the selected workspace and explicit read-only roots.
2. Make safe shell execution the zero-configuration default on supported macOS systems.
3. Preserve normal macOS developer workflows where practical, including Xcode/CommandLineTools, Homebrew tools, language toolchains, package downloads, and localhost tests.
4. Define a typed Driver contract that a later container implementation can satisfy without changing `bash`, Agent, TUI, or REPL APIs.
5. Fail closed whenever a requested boundary cannot be enforced.
6. Keep all default tests offline and automated.
7. Preserve existing `bash` output, redaction, timeout, cancellation, and nonzero-exit semantics.
8. Keep configuration small and make the effective security state visible locally.

## Non-goals

Phase 1 does not provide:

- Docker, Apple Container, Podman, Lima, Colima, or other container execution;
- Linux or Windows sandbox support commitments;
- remote execution;
- App Sandbox entitlements for the Otto process itself;
- domain allowlists, HTTP/SOCKS proxies, TLS interception, or request inspection;
- per-command permission prompts or automatic permission expansion;
- CPU, memory, disk, or process-count isolation;
- protection of the workspace from the Agent;
- sandboxing of Otto's file tools, provider client, session store, memory store, or frontends;
- MCP server isolation;
- arbitrary Seatbelt profiles, backend flags, or project-defined sandbox rules;
- a `sandbox doctor` command while only one sandbox backend exists.

## Current behavior

`internal/tool.Workspace` canonicalizes the workspace, resolves symlinks, and rejects file-tool paths outside it. Search tools also avoid discovered symlinks. These checks apply only to Otto's file tools.

`internal/tool/bash.go` currently executes:

```text
<shell> -lc <model-provided command>
```

with `cmd.Dir` set to the workspace. It inherits most of the host environment and has no operating-system filesystem or network restriction. Setting a working directory does not create a security boundary; a command can use `..`, absolute paths, subprocesses, shell startup behavior, local sockets, or any other permission available to the current user.

The new subsystem closes this shell-specific gap. It does not replace the existing file-tool workspace validator.

## Threat model

### Trusted

- the Otto binary and embedded Seatbelt policy template;
- the selected macOS kernel and `/usr/bin/sandbox-exec` supplied by the OS;
- `cmd/otto` lifecycle and configuration resolution;
- an explicitly selected global or `--config` configuration file;
- the canonical workspace path selected by the user;
- Driver construction inputs after validation.

### Untrusted

- model-generated shell text;
- repository content, including instructions and executable scripts;
- child and descendant processes started by the shell;
- command output before redaction;
- environment variable names and values until filtered;
- symlinks and path spelling supplied from the workspace.

### Security properties

When the effective driver is Seatbelt, Otto must ensure:

1. Shell descendants cannot read file contents outside approved read roots.
2. Shell descendants cannot write outside the canonical workspace and the private `home`, `tmp`, and `cache` subdirectories granted to that process.
3. Symlinks cannot turn an allowed path spelling into access to a disallowed target.
4. Network policy is enforced by Seatbelt, not merely by proxy environment variables.
5. Unix-domain sockets are unavailable unless a future typed policy explicitly introduces a narrowly defined capability.
6. Apple Events and Launch Services cannot launch an unsandboxed surrogate process.
7. Cancellation, timeout, normal shell exit, and Driver close terminate and reap every remaining member of the Driver-owned process group.
8. Process inspection and signaling are limited to processes carrying the same Sandbox policy where Seatbelt supports that distinction.
9. Provider credentials and filtered secret variables are not inherited by the command.
10. Failure to establish these properties cannot cause an unsandboxed retry.

### Explicit limitations

Seatbelt is mandatory access control within the same user account and kernel. It does not defend against:

- a macOS kernel or Seatbelt vulnerability;
- a malicious concurrent host process already running as the user;
- malicious manipulation by trusted in-process code;
- CPU, memory, disk-space, or fork exhaustion;
- deletion, encryption, or corruption of the workspace itself;
- data exfiltration from the workspace when network access is allowed;
- user-approved environment credentials being used for their intended network services;
- a pre-existing workspace hard link whose other inode name is outside the workspace;
- later execution by the user of intentionally modified workspace source or scripts;
- a descendant that deliberately creates a new session/process group and escapes the Driver-owned process group before cleanup.

A clean Git checkout cannot encode external hard links, but a locally prepared malicious workspace can contain them. Strong protection from that case, adversarial daemonization, and resource exhaustion requires a copied filesystem or VM/container boundary and is deferred. Escaped descendants still inherit Seatbelt and remain filesystem/network confined, but Phase 1 cannot guarantee their termination because macOS has no public cgroup, subreaper, or Job Object equivalent.

## Research basis

The practical macOS choices are constrained:

- Apple marks `sandbox-exec` and `sandbox_init` as deprecated and recommends App Sandbox for applications.
- App Sandbox is not a drop-in fit for a CLI that launches arbitrary developer toolchains against user-selected directories.
- Current coding agents nevertheless use custom Seatbelt profiles: OpenAI Codex, Gemini CLI, and Anthropic Sandbox Runtime all use this approach on macOS.
- Docker Desktop, Colima, and Podman run Linux containers in a VM on macOS. They provide stronger process/resource boundaries but cannot transparently use macOS-only tools such as Xcode.
- Apple Container runs each OCI container in a lightweight VM on supported Apple Silicon systems, but it is not part of Phase 1.

A local probe on the target Apple Silicon macOS system confirmed that custom `sandbox-exec` profiles still load and enforce a denied write. A trivial process incurred approximately five milliseconds of additional median startup latency. This is feasibility evidence, not a security proof.

The deprecation risk is the principal reason Seatbelt must remain behind a Driver contract and startup self-test.

## Architectural boundaries

### Package ownership

```text
cmd/otto
  - CLI/config wiring
  - Driver selection and process lifecycle
  - startup warnings and effective local status

internal/config
  - TOML types, validation, defaults, and CLI override resolution

internal/sandbox
  - backend-neutral policy, requests, status, Driver contract, Executor
  - environment filtering and private execution environment
  - conformance test helpers under internal/sandbox/sandboxtest

internal/sandbox/seatbelt
  - macOS profile template, startup self-test, process execution
  - Darwin implementation plus unsupported-platform construction

internal/sandbox/direct
  - explicit unsafe execution for driver=off only

internal/tool
  - bash JSON contract, output collection, exact redaction, result formatting
  - existing file-tool workspace enforcement remains unchanged

internal/app
  - exposes sanitized effective sandbox information to frontends
  - does not own Driver lifecycle or backend objects

internal/tui and internal/repl
  - render sanitized effective state only
```

Provider-specific packages, session persistence, Memory, and model types do not import `internal/sandbox`.

### Process-level lifetime

Otto currently keeps one canonical workspace for the lifetime of a process. `/new` and `/resume` replace sessions but cannot switch workspaces. Therefore `cmd/otto` constructs one Executor after configuration and workspace resolution, shares it with every runner created in that process, and closes it after the frontend and `app.Controller` have stopped.

This avoids changing the already delicate session-replacement protocol merely to prepare for containers. A future Docker Driver may lazily create and retain a container behind the same process-level interface.

Close ordering is:

1. cancel frontend/process context;
2. close the Controller and wait for any active Agent/tool operation;
3. close the Sandbox Executor/Driver;
4. clean private sandbox state;
5. return the process exit code.

Driver close remains independently safe if an earlier construction or cleanup path invokes it.

## Stable execution contract

The Phase 1 contract names and meanings are normative:

```go
type DriverID string

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
```

`Executor` is a concrete, backend-neutral wrapper around a Driver and immutable resolved `Policy`. It validates every request before delegating. Resource limits are intentionally absent until a backend can enforce them; they may be added later as typed fields without exposing backend-specific options.

### Request invariants

- `Argv` is nonempty and contains no NUL bytes.
- `Dir` is the canonical workspace or a canonical descendant.
- `Env` is a complete, already filtered environment, not an overlay on `os.Environ()`.
- Environment entries have valid nonempty names, contain no NUL bytes, and are unique.
- `Streams` writers are non-nil and owned by the caller for the duration of `Execute` only.
- Drivers cannot retain request slices, contexts, writers, or environment values after return.
- Policy is not carried on each request and cannot be expanded by model input.

`bash` supplies `[]string{configuredShell, "-lc", command}`. The Driver never reconstructs a shell command string and never invokes an additional shell.

### Capabilities

Capabilities are typed declarations, not free-form string maps. Read/write confinement, both network modes, and Unix-socket denial are independently represented. Concurrency safety, process-group cleanup, non-retention, and idempotent close are mandatory Driver-contract behavior rather than optional capabilities.

For `FilesystemWorkspaceWrite`, the Executor requires read confinement, write confinement, Unix-socket denial, and support for the selected network mode. `FilesystemUnconfined` requires no filesystem or Unix-socket guarantee. Missing capability returns a typed unsupported-policy error. No code interprets a missing capability as permission to continue.

The direct Driver advertises `NetworkAllow` because it can run with ordinary host networking, but advertises no read/write or Unix-socket confinement and no `NetworkDeny`. It can only be paired with `Policy{Filesystem: FilesystemUnconfined, Network: NetworkAllow}`, produced by explicit `driver = "off"`; configured `network = "deny"` is ignored with the unsafe status.

### Concurrency

The Driver contract is safe for concurrent `Execute` calls and concurrent `Close`. An implementation may serialize internally. `Close` is idempotent, begins rejecting new work, terminates active Driver-owned process groups if necessary, waits for their leaders, and then returns one stable close result to all callers.

Phase 1 Agent execution is sequential, but the stronger contract avoids future backend and tool coupling.

## Configuration

### TOML

```toml
[sandbox]
driver = "auto"       # auto | seatbelt | off
network = "allow"     # allow | deny
read_paths = []        # absolute paths or ~/...
allow_env = []         # exact environment variable names
```

Defaults when the table or fields are absent:

```text
driver  = auto
network = allow
read_paths = []
allow_env = []
```

This intentionally changes the default from unsandboxed shell execution to Seatbelt on macOS.

### CLI

```text
--sandbox auto|seatbelt|off
```

The CLI override affects only `driver`. Network, read roots, and environment exceptions remain in the global/explicit config to avoid a large security-sensitive flag surface.

`docker`, `apple-container`, and `podman` are rejected as unsupported values until their production Drivers exist. Documentation may list them only as future backends.

### Resolution

1. `--sandbox`, when present;
2. `[sandbox].driver`;
3. built-in `auto`.

Sandbox policy is process-wide. It is not profile-specific and does not change when a session restores a provider/model or when `/new` returns to startup runtime settings.

Otto auto-discovers only `~/.config/otto/config.toml`. It never auto-discovers a repository-local sandbox configuration. An explicitly supplied `--config` path is human authority, consistent with current config behavior.

### Validation

- Driver and network values are closed enums.
- `allow_env` entries must be unique valid environment names.
- `read_paths` entries must be absolute or start with `~/`.
- `~` expansion uses the already resolved user home; other shell or environment expansion is forbidden.
- Each path is bounded in length, must exist at startup, and is canonicalized with symlink resolution.
- Duplicate and nested read roots are deterministically collapsed.
- At most 128 effective dynamic read roots and 32 KiB of canonical dynamic path text are accepted.
- Unknown TOML fields continue to fail strict decoding.
- Validation errors contain no environment values or generated profile source.

## Effective status and unavailable behavior

The local status is a small sanitized value, for example:

```text
seatbelt · workspace-write · network allowed
seatbelt · workspace-write · network denied
bash disabled · sandbox unavailable
sandbox off · WARNING: bash is unsandboxed
```

There is no `/status` slash command today, so Phase 1 does not add one. Effective state appears in:

- startup/help text;
- the REPL banner;
- TUI help/session presentation and, when width permits, its footer;
- existing `/session` output or overlay;
- stderr warnings in headless mode when shell execution is unavailable or explicitly unsafe.

Normal successful Seatbelt startup does not add noise to headless stdout.

If `auto` or explicit `seatbelt` is unavailable or fails self-test, Otto continues without registering `bash`. It does not fail the entire application and does not construct a direct Driver. File tools remain available. Shell validation is skipped when no shell tool will be registered.

If `off` is explicit, Otto registers direct execution and emits a prominent local warning. The warning and dynamic system prompt must use the word `unsandboxed` and state that commands have the current macOS user's access.

## Filesystem policy

### Canonical workspace

The policy uses the physical canonical workspace already resolved at startup. All dynamic paths are normalized before profile construction. Seatbelt receives path values through profile parameters, not unescaped profile fragments.

Path ancestor metadata access may be granted only as needed to resolve approved roots. Ancestor grants do not imply file-content access.

### Writable roots

Only these roots are writable:

1. the full canonical workspace;
2. the process-private `home`, `tmp`, and `cache` subdirectories;
3. required character devices and inherited standard streams as narrowly allowed by the fixed profile.

There is no external writable-path configuration in Phase 1.

The workspace is intentionally treated as one mutable unit. `.git`, source, tests, build scripts, and generated files remain writable, matching current `write`, `edit`, and `bash` behavior. The documentation explicitly warns that the Sandbox does not protect workspace contents.

### Private execution environment

Otto securely creates a private process root beneath the user's macOS cache directory, using a random unguessable leaf and `0700` permissions. It contains:

```text
home/
tmp/
cache/
profiles/
```

Each Seatbelt child receives these fixed replacements:

| Variable | Private destination |
| --- | --- |
| `HOME` | `home/` |
| `TMPDIR`, `TMP`, `TEMP` | `tmp/` |
| `XDG_CACHE_HOME` | `cache/xdg/` |
| `GOCACHE` | `cache/go-build/` |
| `GOMODCACHE` | `cache/go-mod/` |
| `NPM_CONFIG_CACHE` | `cache/npm/` |
| `PIP_CACHE_DIR` | `cache/pip/` |
| `UV_CACHE_DIR` | `cache/uv/` |

Other ecosystem-specific cache locations follow the private `HOME` by default. A host-defined path variable still cannot grant filesystem access; the Seatbelt profile remains authoritative.

The process root is not inside the workspace, so repository cleanup commands cannot discover it through normal workspace traversal. Seatbelt grants child write access only to `home`, `tmp`, and `cache`. The parent and `profiles` directory are not readable or writable by the sandboxed child. Construction rejects symlinks and non-directories and verifies ownership/permissions without changing process-wide umask.

The root is removed after Driver close. Removal never follows symlinks. Cleanup failure is reported locally but must not expose secret environment data to the model or session.

Host build caches are not writable. Phase 1 does not automatically expose user cache directories because they may contain private dependency source. Users can opt into a specific cache as a read-only `read_path`; writes still go to Otto's private cache.

### Automatic read-only roots

The profile begins closed by default and permits only roots required for normal command execution. Existing roots from the following classes are considered:

- macOS runtime paths under `/System`, `/bin`, `/sbin`, and narrowly required `/usr` locations;
- Apple runtime data required for dynamic loading, DNS, certificates, user lookup, and process startup;
- `/Library/Apple`, CommandLineTools, `/Library/Developer`, and the active Xcode application when present;
- executable/library/share roots for Apple Silicon Homebrew under `/opt/homebrew`;
- executable/library/share roots for common `/usr/local` installations;
- the configured shell and canonical existing directories from `PATH`;
- user-configured `read_paths`.

The fixed profile must not broadly allow the entire user home, filesystem root, `/Users`, `/Applications`, `/opt/homebrew/etc`, `/opt/homebrew/var`, `/usr/local/etc`, or `/usr/local/var`. Automatic `PATH` processing skips entries equal to `/`, `/Users`, or the resolved user home. Toolchain rules should name the minimum stable subtrees needed for binaries, libraries, SDKs, and shared resources.

Canonical `PATH` entries below the user home are read-only, which supports common shim/tool directories without exposing unrelated home content. The configured shell itself is granted as a canonical executable file, not by automatically granting its whole parent. A tool that needs additional adjacent data fails with `EPERM` until the user adds the narrow root to `read_paths`.

### Symlinks and aliases

All configured roots are canonicalized before profile generation. Seatbelt enforcement applies to the resolved target; a workspace symlink to a disallowed path remains disallowed. Tests must cover read and write attempts through absolute and relative symlinks.

Pre-existing hard links are the explicit limitation described in the threat model. The UI and documentation must not claim VM-like filesystem isolation.

## Environment policy

The shell receives a complete deterministic environment snapshot assembled by Otto. It no longer calls `os.Environ()` inside `bashTool` after security resolution.

`cmd/otto` captures the process environment once through an injectable enumerator, then passes it to a pure resolver. The resolver returns immutable copies of the command environment and exact redaction values. `runtimeBuilder` combines those redaction values with provider secrets when constructing both `bashTool` and the Agent redactor. Frontends, sessions, and `app.Info` never receive the raw environment or redaction set.

### Non-overridable removals

The following cannot be restored by `allow_env`:

- `OTTO_API_KEY` and every selected/configured provider `api_key_env`;
- dynamic-loader injection variables such as `DYLD_*` and `LD_*`;
- shell startup injection variables such as `BASH_ENV`, `ENV`, and `ZDOTDIR`;
- inherited socket/control variables such as `SSH_AUTH_SOCK`;
- internal Otto Sandbox variables and profile paths.

### Automatic secret-name filtering

Names are compared case-insensitively for classification while preserving the original name for allowed output. Filtering covers:

- token, secret, password/passwd, credential, private-key, API-key, and access-key suffixes;
- common AWS, Azure, Google Cloud, GitHub/GitLab, npm, package-registry, Docker-auth, and CI credential names;
- configured provider credential names even when that provider is not currently selected.

The implementation uses a reviewed deterministic classifier, not a loose substring check that would remove unrelated names such as `AUTHORS`.

`allow_env` can restore an automatically classified non-provider variable by exact name. This is explicit user authorization, not a wildcard. Non-overridable removals remain removed.

### Environment redaction

Every sensitive value discovered during environment classification is added to the same exact redaction boundary used for provider credentials before output truncation and session persistence. This includes an explicitly restored sensitive variable.

Empty values are ignored and values are deduplicated. At most 512 sensitive values and 1 MiB of aggregate sensitive-value bytes may be retained. Exceeding either bound disables `bash` for the process rather than silently omitting a secret from the redactor.

Exact output redaction is defense in depth, not containment: a command that receives an explicitly restored credential can transform or encode it. `allow_env` therefore grants that credential to untrusted shell code and is documented as a high-risk user decision.

No error may include a filtered value, a proxy password, URI userinfo, authorization material, or a redaction placeholder derived from persisted content.

### Preserved and replaced values

Ordinary values such as `PATH`, locale, terminal type, SDK selection, compiler flags, and non-secret project settings remain available unless they match a non-overridable class. `HOME`, temporary/cache paths, and Otto-owned status variables are replaced after filtering.

Proxy variables may remain for compatibility only after URI userinfo is detected and included in exact redaction. They do not weaken Seatbelt's network mode.

## Network and IPC policy

### `network = "allow"` (default)

The fixed profile permits ordinary IPv4/IPv6 outbound traffic, DNS/certificate services, and IP loopback behavior needed by local test servers. It may allow local IP binding as required for developer test servers.

It does not grant AF_UNIX socket access. In particular, commands cannot connect to:

- Docker or Podman sockets;
- SSH/GPG agents;
- arbitrary launchd or application sockets.

The policy also denies Apple Events, `lsopen`, and the Mach services required to use `open` or `osascript` as an unsandboxed process launcher.

Because unrestricted IP networking can exfiltrate readable workspace content and can contact unsecured localhost or LAN services, local status and documentation say `network allowed`; they do not use language such as "fully safe." User Keychain access, TCC brokers, pasteboard services, and unrelated process inspection remain denied even when IP networking is allowed.

### `network = "deny"`

No IP network, local binding, or Unix socket is allowed. Provider HTTP streaming is unaffected because the provider client runs in Otto, outside the child sandbox.

The mode is useful for offline work but does not add an interactive retry prompt. The user changes global configuration and restarts to alter it.

### Future domain policy

Domain allowlisting requires a host proxy, DNS rules, TLS behavior, Java/Go trust handling, and explicit exfiltration semantics. It cannot be represented as an unimplemented string in Phase 1. A future design must extend the typed policy and capability contract.

## Seatbelt Driver

### Binary and profile

- The executable path is exactly `/usr/bin/sandbox-exec`.
- Otto does not search `PATH` or accept a configured replacement.
- The binary must be an executable regular OS path; unexpected type or identity causes unavailability.
- The profile is an embedded closed-by-default template.
- Dynamic paths are passed using `-D key=value` parameters or an equivalently injection-safe mechanism.
- Generated profile files are `0600` beneath the private `profiles` directory.
- Profile source, private roots, and dynamic environment values are never returned in a tool result.

The fixed profile includes only reviewed process, loader, IPC, device, sysctl, filesystem, and network operations needed by supported developer commands. Every broad compatibility grant requires a test and a comment explaining why it cannot expose user content or an escape channel.

### Startup self-test

Construction performs an offline enforcement probe in private temporary fixtures. It verifies at least:

1. the profile loads and starts a child;
2. an allowed fixture can be read and written;
3. a denied fixture cannot be read;
4. a denied fixture cannot be written;
5. the child exits and is reaped.

Network checks use only local fixtures in the test suite; production startup does not contact the internet. Production self-test remains bounded and fast.

A successful syntax-only invocation is insufficient: the probe must observe both allowed and denied behavior. Any ambiguous result marks the Driver unavailable.

### Execution

The Driver starts `sandbox-exec` and the configured shell in a new process group. Child processes inherit the Seatbelt profile. No extra file descriptors are supplied.

On context cancellation or timeout propagated by `bashTool`, the Driver:

1. sends termination to the entire Driver-owned process group;
2. escalates to `SIGKILL` according to the existing immediate-cancellation behavior or a future separately approved grace policy;
3. waits for the process group leader;
4. performs a final process-group kill to remove ordinary background jobs before returning.

The same final process-group cleanup runs after a normal shell exit, because `/bin/sh -lc 'sleep 30 &'` can otherwise return while the background process survives. Tests must verify actual same-group cleanup rather than assuming that waiting for the shell is sufficient. A malicious child can call `setsid` or otherwise leave the group; the explicit threat-model limitation applies, although that child continues to inherit Seatbelt confinement.

### Runtime failure

If `sandbox-exec` disappears, refuses the profile, or another infrastructure condition prevents enforcement:

- the current call returns a sanitized typed infrastructure error;
- it is never rerun directly;
- the Driver latches an unavailable/poisoned state when continued enforcement cannot be trusted;
- later calls fail deterministically until Otto restarts.

Ordinary policy denial is normally visible to the child as `EPERM` and a nonzero exit. Otto does not scrape Unified Log as a correctness dependency because violation attribution is asynchronous and racy.

## Direct Driver

The direct Driver preserves current process-group execution only when the resolved driver is explicitly `off`.

It must:

- advertise no filesystem/network confinement;
- retain provider-credential removal, automatic secret-name filtering, and output redaction;
- preserve the host `HOME`, temporary, cache, and ordinary environment values for compatibility instead of applying Seatbelt's private-path replacements;
- retain timeout, cancellation, and same-process-group cleanup;
- ignore `network` and `read_paths`, because it cannot enforce them;
- emit an unsafe local status and warning;
- never be selected by `auto` or as fallback from Seatbelt.

Keeping direct execution behind the same request/result contract avoids duplicate `bashTool` process code and gives users an explicit compatibility escape hatch.

## `bash` tool behavior

The tool keeps responsibility for:

- strict JSON decoding and nonempty command validation;
- configured timeout selection;
- separate capped stdout and stderr collectors;
- exact streaming redaction before truncation;
- existing human/model result formatting;
- distinguishing caller cancellation from timeout;
- turning Driver infrastructure errors into `Result{IsError: true}`.

The Driver owns process creation, platform isolation, process-group termination, waiting, and exit inspection.

Existing observable behavior remains:

- exit code zero or nonzero is a completed tool call, not an infrastructure error;
- stdout and stderr include independent truncation notices;
- pre-cancelled context does not start a child;
- cancelled and timed-out results include their existing status text and signal when available;
- a missing configured shell is an infrastructure error only when `bash` is otherwise enabled.

## Agent and frontend integration

### Dynamic tool set and prompt

The static `systemPrompt` is replaced by deterministic construction from actual registered tools and sanitized Sandbox status.

- Seatbelt enabled: describe file tools as workspace-bound and `bash` as Seatbelt-confined, with workspace writes and the effective network mode.
- Bash unavailable: omit `bash` from both tool definitions and prompt instructions.
- Sandbox off: retain an explicit warning that `bash` has the current macOS user's access.

The prompt never includes host read roots, private directory paths, environment variable names, profile source, or startup diagnostic details.

### Runtime information

A small provider-neutral value is carried through `app.RuntimeInfo`/`app.Info`, sufficient for frontends to display:

- effective Driver ID;
- effective network mode;
- whether `bash` is registered;
- one bounded sanitized unavailable reason category, not a raw wrapped error.

Session replacement copies the same process-level value. It is not restored from session metadata.

### Persistence

Sandbox configuration and diagnostics do not enter:

- Pi JSONL headers or messages;
- tool results except the command's own bounded stderr/exit result;
- compaction summaries;
- Memory records or observations;
- provider requests beyond the short generic system-prompt statement.

No session migration is required.

## Error model

Stable sentinel/typed classes distinguish:

- unavailable backend;
- unsupported policy/capability;
- invalid request;
- closed Driver;
- child launch/profile infrastructure failure;
- wait/termination infrastructure failure.

A nonzero child exit is `ExitStatus`, not an error. Context cancellation remains discoverable with `errors.Is` where an infrastructure path returns an error.

Driver errors are sanitized before crossing into `tool.Result`. They may include the Driver ID and a bounded reason category, but not:

- environment values;
- profile text;
- authorization/cookie/proxy credentials;
- URI userinfo;
- workspace-external attempted paths collected from system logs;
- private Sandbox root names.

Startup stderr can offer a local remediation such as "set sandbox.driver=off to run unsandboxed," but must not make that change automatically.

## Driver conformance suite

`internal/sandbox/sandboxtest` defines reusable behavioral tests for any production Driver. A backend cannot be documented as supported until all capabilities it advertises pass.

The suite covers:

1. successful stdout/stderr and zero exit;
2. nonzero exit preservation;
3. canonical workspace read/write;
4. denied read and write outside approved roots;
5. relative and absolute symlink escape attempts;
6. environment replacement and secret removal;
7. network allow and deny using local TCP fixtures only;
8. Unix-socket denial;
9. cancellation before start;
10. cancellation and timeout after a same-process-group descendant is running;
11. no ordinary same-process-group background job survives return;
12. concurrent execution according to the contract;
13. concurrent/idempotent Close and post-close rejection;
14. paths containing spaces, Unicode, quotes, and profile punctuation;
15. bounded, sanitized infrastructure errors.

Tests use event barriers, pipes, child-ready markers, and process handles. They do not use sleeps or polling to assert process ordering; `time.After` is only a hard deadlock deadline.

## Test strategy

### Platform-neutral tests

Default tests on every platform cover:

- config defaults, enum validation, precedence, and strict TOML behavior;
- path expansion/canonicalization and root bounds;
- environment classification, non-overridable removals, explicit allows, and redactor inputs;
- Executor capability checks and fail-closed behavior with fake Drivers;
- request cloning/non-retention expectations;
- direct Driver selection only after explicit `off`;
- dynamic tool registration and system prompt variants;
- sanitized `app.Info`, REPL, TUI, help, and headless warnings;
- cleanup and lifecycle ordering through injected fakes.

### Darwin integration tests

Darwin tests invoke the real `/usr/bin/sandbox-exec` and remain offline. They use only `t.TempDir()`, local pipes, local TCP listeners, and Unix sockets. They never inspect the real SSH directory, cloud configuration, API credentials, or unrelated user files.

They verify the embedded production profile rather than a permissive test-only substitute. Conditional compatibility smoke tests cover `/bin/sh`, `/usr/bin/git`, `xcrun` plus a trivial Clang compile when CommandLineTools/Xcode is installed, and `go version` plus a temporary-module `go test` when a canonical Homebrew Go executable is present.

On non-Darwin systems, Seatbelt files compile behind build tags and construction returns a typed unsupported/unavailable result. The neutral package and direct Driver remain testable. Otto makes no non-macOS support claim.

### CLI and PTY coverage

Coverage includes:

- default `auto` selecting Seatbelt on Darwin;
- Seatbelt self-test failure leaving file tools but omitting `bash`;
- explicit `off` warning;
- TUI footer/help/session state at wide and narrow terminal sizes;
- REPL banner and `/session` state;
- headless stderr without stdout contamination;
- Ctrl+C and PTY lifecycle with a sandboxed active command;
- no terminal restoration regressions.

### Repository gates

Implementation must pass the existing Go gates, including race tests, PTY lifecycle, formatting, vet, staticcheck policy, and CGO-disabled builds where applicable. No test may require credentials, Node, a container daemon, public network access, or an interactive terminal.

## Documentation changes

README and the user manual must replace the unconditional unsandboxed warning with:

- the default Seatbelt behavior;
- effective config examples;
- the distinction between file-tool confinement and shell confinement;
- the default `network = "allow"` exfiltration implication;
- explicit `off` behavior and warning;
- compatibility guidance for adding a narrow `read_path`;
- limitations of same-kernel Seatbelt isolation;
- a statement that Docker and Apple Container are planned, not supported.

CLI help lists `--sandbox` and describes `off` as unsafe. Examples must match actual flags and TOML fields.

The Driver author guide documents the interface, capability semantics, lifecycle, error model, security invariants, and conformance suite. It must not promise that merely implementing the Go interface makes a backend secure.

## Rollout and compatibility

Existing configuration files without `[sandbox]` resolve to `auto`, so macOS users receive Seatbelt by default after upgrading.

Expected compatibility changes:

- shell startup no longer reads the user's normal dotfiles because `HOME` is private;
- home-directory language managers may need a narrow `read_path`;
- writes to host caches/configuration fail or move to the private cache;
- Docker/SSH-agent commands fail because Unix sockets are blocked;
- commands needing arbitrary user files fail until a path is explicitly allowed;
- commands continue to access package registries and other IP services because network defaults to allow.

When compatibility cannot be resolved with a read path or ordinary environment allow, the user may explicitly choose `off`. Otto must make the loss of protection obvious every time.

No session schema or persisted data migration occurs.

## Future container Driver requirements

Phase 1 does not expose a container driver, but the stable contract is designed for one. A future Docker/OCI design must separately decide image trust, mount rules, UID/GID mapping, network mode, resource limits, container reuse, cancellation, and stale-container cleanup.

At minimum, a future container Driver must:

- mount only the canonical workspace and Driver-owned state;
- never mount Docker/Podman sockets, SSH agents, host home, or arbitrary project-requested paths;
- use a pinned trusted default image and never auto-build an untrusted repository Dockerfile;
- provide a read-only root filesystem where compatible;
- drop Linux capabilities, enable no-new-privileges, and apply process/resource limits;
- pass the same Driver conformance suite;
- add typed capabilities rather than backend-specific `extra_args`;
- remain explicit until compatibility and security are proven.

Docker Desktop and Colima may share one Docker CLI/API Driver because they expose the same trusted host control plane. Podman and Apple Container require separate conformance and must not be claimed compatible by name alone.

## Alternatives considered

### Keep `bash` unsandboxed

Rejected because a model can currently delete or read any user-accessible path despite file-tool workspace enforcement.

### Container-only default

Rejected for Phase 1 because it adds installation/runtime setup, changes the command OS to Linux, and breaks macOS/Xcode workflows. It remains the stronger future option for unknown code.

### Depend on Anthropic Sandbox Runtime

Rejected as Otto's mandatory backend because it adds a Node dependency and the project/config API is still evolving. Its filesystem, network, Unix-socket, and Apple Events findings remain useful reference material.

### Sign Otto with App Sandbox entitlements

Rejected because App Sandbox is designed around application entitlements and user-mediated resource access, not arbitrary CLI developer toolchains and shell subprocesses.

### Dedicated macOS user account

Rejected for the default because account creation, ACL management, workspace ownership, and network controls require significant setup and administrator involvement.

### Silently fall back to direct execution

Rejected categorically. A warning cannot repair a boundary that the user reasonably believes is active.

### Broad disk read with workspace-only writes

Rejected because command output is sent back to the model/provider. With default network access, broad reads expose unknown credentials and private data even if writes are contained.

## Acceptance criteria

Phase 1 is complete only when all of the following are true:

1. On a supported clean macOS system with no Sandbox config, `bash` runs under the embedded Seatbelt policy.
2. A shell command cannot read or write a test file outside approved roots, including through a symlink.
3. A shell command can read/write the workspace and use supported system/Homebrew/Xcode toolchains.
4. Ordinary IP network access works by default; `network = "deny"` blocks local test connections without affecting provider streaming.
5. Docker and SSH-agent Unix sockets are unavailable to the command.
6. Apple Events and Launch Services cannot launch an unsandboxed surrogate.
7. Provider credentials never reach the command. Automatically classified credentials reach it only after an exact `allow_env`; untransformed exact values are redacted, while documentation warns that transformed/encoded exfiltration remains possible after that explicit grant.
8. Normal completion, timeout, and cancellation leave no member of the Driver-owned process group running; deliberate process-group escape remains the documented native limitation.
9. Seatbelt unavailability omits `bash` without disabling file tools or falling back to direct execution.
10. Explicit `off` restores direct execution with prominent local and prompt warnings.
11. `/new` and `/resume` preserve one process-level effective Sandbox without persisting it in session data.
12. Default tests remain offline and all repository gates pass.
13. README, user manual, CLI help, REPL, TUI, and headless behavior describe the actual effective state.
14. The Driver contract and conformance guide are sufficient to design Docker later without changing `bash` or Agent APIs.

## References

- Apple local manuals: `sandbox-exec(1)`, `sandbox(7)`, and `sandbox_init(3)`
- Apple App Sandbox: <https://developer.apple.com/documentation/security/app-sandbox>
- OpenAI Codex sandboxing source: <https://github.com/openai/codex/tree/main/codex-rs/sandboxing>
- Anthropic Sandbox Runtime: <https://github.com/anthropic-experimental/sandbox-runtime>
- Gemini CLI sandboxing: <https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/sandbox.md>
- Docker Engine security: <https://docs.docker.com/engine/security/>
- Apple Container technical overview: <https://github.com/apple/container/blob/main/docs/technical-overview.md>
