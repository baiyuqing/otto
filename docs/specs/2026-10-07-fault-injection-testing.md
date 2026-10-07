# Fault injection testing

Status: approved 2026-10-07, including Phase 4. Test-only infrastructure.
Phases 2 and 4 make behavior-preserving production refactors to expose
existing seams. Amended the same day after checking each claim against the
code; [Amendments](#amendments) lists what changed.

## Problem

Otto's recovery contracts are specified in detail:
[resilience](2026-09-28-resilience-retry-idempotency.md),
[session failover](2026-09-28-session-failover.md),
[crash recovery](2026-10-06-crash-recovery-and-turn-resume.md),
[durable inputs](2026-10-06-durable-inputs.md),
[durable turn queue](2026-10-07-durable-turn-queue.md) and
[state storage contracts](2026-10-06-state-storage-contracts.md). The tests
that exercise their failure paths have grown one boundary at a time and share
nothing:

- About seven loopback HTTP servers, each hand-written on a raw
  `TcpListener`: `auth/testserver.rs`, the `openaicompat.rs` test module,
  a copy of it in `skill/check/tests.rs`, `memory/advice/tests.rs`,
  `tool/web/client.rs`, `tool/models.rs` and `tests/common/mod.rs`. Each
  scripts a different subset of failures.
- Test hooks in production code, each with its own shape:
  `StoreState.fail_writes` in the POSIX session store, `fail_renewal_for_test`
  and friends in the lease, `break_for_test` / `break_schema_for_test` /
  `poison_for_test` in Turso stores, the `DirectorySync` function pointer in
  `tool/write.rs`. `Inbox::set_persist` and `CommitGuard::with_sync` are
  production seams that tests also use.
- Per-test fake trait implementations (`FailingSession`,
  `RejectingTransport`, `FailAndCancelExecutor`, the migration `Ports`
  fault flags).

Concrete gaps:

1. Missing `[DONE]`, malformed SSE events and stalls after headers are
   tested only at the parser level, not through the real `reqwest`
   transport and the retry loop.
2. No test injects `EIO`/`ENOSPC`, a short write or a failed `fsync`
   through `FileDriver`/`FileRoot`/`JournalFile`; the session store relies on
   one boolean hook.
3. Eight Turso repositories (`approval`, `memory`, `reflection`, `usage`,
   `memory_advice`, `tasks`, `workflow` and `schedule`) call
   `Connection::open` directly, bypassing the injected `SqlDriver`, so their
   commit-failure paths cannot be reached.
4. The temp-write → `sync_all` → rename → directory-sync pattern is copied
   in about seven places (config, workspace marker, skillwrite,
   `remind::replace_private`, `tool::write`, session fork, seatbelt
   profile). Only `tool::write` has an injection point.
5. Nothing kills a process between two durable steps (after a rename,
   before the directory sync). `tests/sigterm.rs` covers graceful SIGTERM
   only.
6. A lost acknowledgement (the effect happened, the caller saw an error) is
   modelled only in the migration `Ports` fake.

## Goals

1. One test-only `faults` module with a single typed fault vocabulary.
2. A shared scripted HTTP fault server that replaces the copied servers.
3. Fault-wrapping decorators over the seams that already exist:
   `FileDriver`, `SqlDriver`, `mcp::Transport`, `sandbox::Driver` and
   `otto_core::mailbox::Mailbox`.
4. Route the remaining direct I/O through those seams: thread the SQL driver
   into every repository (Phase 2) and consolidate atomic publishing behind
   one helper (Phase 4).
5. A crash harness that terminates a process at a named operation and lets
   the test reopen the state with the real driver.
6. A contract matrix per boundary: each row is "inject fault F at operation
   O, assert disposition, effect certainty, attempt count and persisted
   state". Each row is one test.

## Non-goals

- A global failpoint registry, `fail-rs` or any runtime fault configuration
  in the shipped binary.
- New dependencies. `Cargo.lock` adds no package (see Acceptance).
- Changing any retry, certainty or recovery contract. Tests assert the
  contracts in the owning specs. A test that finds a violation is reported
  and fixed in its own commit that cites the owning spec, not silently
  adjusted.
- Power-loss simulation (page-cache loss). The crash harness models process
  death only.
- Network tooling (`tc netem`, Toxiproxy), randomized or seeded fault
  schedules, and Seatbelt fault tests on Linux.
- Faults in `otto-core`. Its fakes stay in `run_tests.rs` and
  `conformance.rs`; the wasm32 boundary is unchanged.

## Decisions

**D1. No global failpoints.** The workspace runs `cargo test`, not nextest,
so tests share one process and a global registry leaks state between
threads. String-named points are not type-checked, and production code would
carry the hooks. The repository already has typed seams; decorators over
them inject faults per instance.

**D2. Decorators, not hooks.** Fault code lives in `crates/otto/src/faults/`,
compiled only under `cfg(any(test, feature = "fault-injection"))`. The
feature is off by default. Integration tests enable it through a self
dev-dependency:

```toml
[dev-dependencies]
otto = { path = ".", features = ["fault-injection"] }
```

If that does not build cleanly with the existing test targets, fall back to
`required-features = ["fault-injection"]` on the new `[[test]]` targets and
pass the feature in `scripts/check-changed.mjs` and the Makefile test
targets. Record which was used in the plan file.

Production code never names `crate::faults`. It only accepts trait objects it
already accepts, or the ones Phases 2 and 4 add.

**D3. Raw TCP, not axum, for the HTTP server.** The scenarios need
byte-level control (declared `Content-Length` larger than the body, a cut
inside a chunk, a stall after headers, a reset). `auth/testserver.rs` already
documents why the crate writes HTTP bytes directly; this design extends it
instead of adding a second style.

**D4. Every fault test proves the fault fired.** A plan that never fired
makes a test pass vacuously. `FaultPlan::assert_all_fired()` is called at
the end of each test (a `Drop` guard panics in debug if it was forgotten).

**D5. Deterministic time.** Use `#[tokio::test(start_paused = true)]`
wherever the code under test uses tokio time and no response is read from a
live socket. Paused time auto-advances while the runtime waits on socket I/O,
which fires reqwest's 60 s read timeout, so rows that read a response from
the loopback server run in real time, within the existing 1 + 2 + 4 s
provider backoff. Do not add real sleeps longer than the existing tests use. Making the lease renewal and watchdog loops
take the `Clock` trait is a later slice, not part of this design.

## Design

### Module layout

```
crates/otto/src/faults/
  mod.rs        // gating, re-exports, docs
  plan.rs       // FaultPlan, Rule, Trigger, Fault, Fired
  http.rs       // scripted HTTP fault server (from auth/testserver.rs)
  files.rs      // FaultyFileDriver / FaultyRoot / FaultyJournal
  sql.rs        // FaultySqlDriver / FaultyConnection / FaultyStatement
  mcp.rs        // FaultyTransport
  sandbox.rs    // FaultyDriver + process fixtures
  mailbox.rs    // FaultyMailbox
  crash.rs      // re-exec crash harness
```

`lib.rs`:

```rust
#[cfg(any(test, feature = "fault-injection"))]
pub mod faults;
```

### Fault plan

One generic plan, parameterised by a per-boundary operation enum:

```rust
pub struct FaultPlan<Op> { /* Mutex<Vec<Rule<Op>>>, Mutex<Vec<Fired<Op>>>, call counters */ }

pub struct Rule<Op> {
    pub op: Op,                        // which operation
    pub filter: Filter,                // Any | NameSuffix(&'static str) | SqlPrefix(&'static str)
    pub trigger: Trigger,
    pub fault: Fault,
}

pub enum Trigger {
    Always,
    Nth(u32),                          // 1-based call count of this rule (after filter)
    FromNth(u32),
}

pub enum Fault {
    /// Fail without performing the operation (effect: none).
    Before(FaultError),
    /// Perform the operation, then report failure (lost acknowledgement).
    After(FaultError),
    /// Writes only: persist the first `keep` bytes, then fail (torn write).
    Short { keep: usize, error: FaultError },
    /// Never return; the caller's deadline or cancellation must end it.
    Hang,
    /// Terminate the process immediately with `_exit(CRASH_EXIT)`.
    /// Only meaningful inside the crash harness child.
    Exit,
    /// Perform the operation, then `_exit` before the caller sees the result.
    ExitAfter,
    /// Writes only: persist the first `keep` bytes, then `_exit`.
    ShortExit { keep: usize },
}

pub enum FaultError {
    Os(i32),                           // e.g. libc::EIO, libc::ENOSPC, libc::EACCES
    Kind(std::io::ErrorKind),
    Sql(SqlFault),                     // Copy mirror of storage::Error variants
}
```

`storage::Error` is not `Clone`, so a rule carries a `SqlFault` and the SQL
decorator builds the storage error each time the rule fires. Every matching
rule counts a call; the first rule that fires on it wins.

The plan records every fired rule (`Fired { rule, op, call, fault }`) so tests
can assert order and count. `Before` versus `After` is the key distinction: it
is how tests separate `NotStarted` from `Unknown` effect certainty.

Per-boundary operation enums:

| Boundary | `Op` variants |
| --- | --- |
| files | `OpenRoot`, `OpenDirectory`, `CreateDirectory`, `OpenJournal`, `RemoveJournal`, `ListNames`, `RootSync`, `Read`, `Write`, `Seek`, `FileSync`, `SetLen`, `Lock` |
| sql | `Open`, `ExecuteBatch`, `Execute`, `Prepare`, `Step`, `Begin`, `Commit`, `Rollback`, `Checkpoint`, `Close` |
| mcp | `Request`, `Notify`, `Close` |
| sandbox | `Execute` |
| mailbox | the `Mailbox` trait's methods, one variant each |
| publish (Phase 4) | `CreateTemp`, `Write`, `FileSync`, `Rename`, `DirSync` |

### HTTP fault server (`faults::http`)

Move `auth/testserver.rs` to `faults/http.rs` and keep its current API
(`spawn`, `spawn_hanging`, `Request`, the response builders) so existing
callers change only their import. Add a scripted mode:

```rust
pub enum Step {
    Head { status: u16, headers: Vec<(String, String)> },
    DeclareLength(u64),
    Body(Vec<u8>),
    Chunk(Vec<u8>),                    // chunked transfer encoding
    Sse(String),                       // writes "data: <json>\n\n" as one chunk
    Flush,
    Stall(Option<Duration>),           // None = forever
    Close,                             // graceful FIN
    Reset,                             // SO_LINGER(0) then close: RST
}

pub struct Script(Vec<Step>);

/// One script per accepted connection, consumed in order.
/// Connections beyond the list get `Script::fail_test()` (500 with a
/// message that names the test bug).
pub async fn spawn_scripted(scripts: Vec<Script>) -> Server;
```

Scenario constructors cover every gap listed above:

- `Script::sse(events)` and `Script::sse_done(events)` (appends `[DONE]`)
- `Script::sse_cut_after(events, n)`: send `n` events, then `Reset`
- `Script::sse_without_done(events)`: all events, then `Close`
- `Script::malformed_event()`
- `Script::status(code, body)`
- `Script::stall_before_head()` and `Script::stall_after_head()`
- `Script::truncated(body, declared)`

Two listener helpers model connect failures: `refused_port()` binds and
drops a listener to obtain a port that refuses connections
(`reqwest::Error::is_connect`), and `late_listener(port, after)` starts
listening only after a delay.

`Server` records `requests()` in arrival order, so tests assert attempt
counts and that every retry sent the same body.

Migrate, in separate mechanical commits with unchanged assertions:
the `openaicompat.rs` test server, the `skill/check/tests.rs` copy,
`memory/advice/tests.rs::serve`, `tool/models.rs::serve`,
`tool/web/client.rs::testserver` and `tests/common/mod.rs::serve` (the last
through the feature from D2).

The gated servers in `tests/acp_stdio.rs`, `tests/acp_attach.rs` and
`tests/tui_pty.rs` hold a response until the test releases it, which a
`Script` does not model; they stay.

### File decorators (`faults::files`)

`FaultyFileDriver::new(inner: Arc<dyn FileDriver>, plan: Arc<FaultPlan<FileOp>>)`
wraps every `FileRoot` and `JournalFile` it returns, so faults reach nested
directories and journals. `Short` writes call the inner `write_all` with the
prefix, then return the error. `After` on `Write`, `FileSync` and `RootSync`
performs the real call first.

Tests construct the POSIX session store and its roots with
`FaultyFileDriver::new(Arc::new(PosixDriver), plan)`. If the store is not yet
constructible with an injected driver, add a constructor that takes
`Arc<dyn FileDriver>`; `Storage::default()` keeps passing `PosixDriver`.

Once the session matrix passes, delete `StoreState.fail_writes` and move its
tests onto the decorator.

### SQL decorators (`faults::sql`)

`FaultySqlDriver` wraps `Arc<dyn SqlDriver>` and every
`SqlConnection`/`SqlStatement` it opens. It injects the typed
`storage::Error` values repositories already handle, including the
"commit outcome unknown" error that `TursoConnection::commit` produces.
`After` on `Commit` performs the real commit and then returns that error:
the lost-acknowledgement case the storage contract requires callers to
report as unknown, never as a rollback.

**Phase 2 prerequisite (production refactor).** Thread the injected
`Arc<dyn SqlDriver>` into the repositories that call `Connection::open`
directly (`storage/backends/turso/{approval,memory,reflection,usage,memory_advice,tasks,workflow,schedule}.rs`),
using `Connection::open_with_driver`. `SqlRepositories::new` already
receives the driver. Behavior is unchanged because production passes
`TursoDriver`. Add a check to `tests/storage_boundary.rs` that non-test code
under `storage/backends/turso/` does not call `Connection::open(`.

### Atomic publishing (Phase 4, separate approval)

Introduce one helper in `crate::fsops`:

```rust
pub trait PublishOps { /* create_temp, write_all, sync_file, rename, sync_dir */ }

pub enum PublishStep { CreateTemp, Write, FileSync, Rename, DirSync }

pub struct PublishError {
    pub step: PublishStep,
    pub committed: bool,   // true once the rename returned Ok
    pub source: io::Error,
}

pub fn publish_file(dir: &Dir, name: &str, bytes: &[u8], mode: Replace | Exclusive, ops: &dyn PublishOps)
    -> Result<(), PublishError>;
```

`committed` generalises the before/after distinction that
`remind::replace_private` already makes. Migrate one call site per commit:
`tool::write::write_file_atomic` (subsumes `DirectorySync`),
`remind::replace_private`, `workspace::publish`, `config::write_bytes`
(its lock, compare-and-swap and backup stay outside the helper),
`reflection::skillwrite::write_atomically` and session `fork::write_new`
(`Exclusive`). `seatbelt::state::write_profile` is not migrated: it truncates
and rewrites a pinned profile file in place, with no temporary file or
rename, and keeps `seatbelt::state::Operations`. The lease's `pwrite`
heartbeat and directory renames keep their own hooks.

Each migration must be behavior-preserving: the call site's existing tests
pass unchanged before the new fault tests are added. One intended change:
`skillwrite::write_atomically` does not sync the directory after its rename
today; through the helper it does.

### MCP, sandbox and mailbox

- `FaultyTransport` wraps `mcp::Transport`: `Before` errors, `Hang` on
  `Request` (drives `request_within_timeout`), `Hang` on `Close`.
- `FaultyDriver` wraps `sandbox::Driver` for spawn failures (`Before` on
  `Execute` must settle as `NotStarted`).
- Process-cleanup faults use real processes, because the contract is about
  signals and reaping. Add shell fixtures in `faults::sandbox`: ignores
  `SIGTERM`; forks a background grandchild that outlives the leader; closes
  stdout and keeps running; writes past the output cap. Run them against
  `DirectDriver` on Linux and macOS, and against Seatbelt only on macOS,
  through the existing `driver_contract!` macro where possible.
- `FaultyMailbox` wraps `otto_core::mailbox::Mailbox`; `TurnQueue` already
  takes `Arc<dyn Mailbox>`. Only `enqueue` returns an error, so the plan's
  operation enum has one variant, `Enqueue`. Keep `Inbox::set_persist`; it is
  an otto-core production seam.

### Crash harness (`faults::crash`)

In-process faults cannot model process death between two syscalls. The
harness re-executes the current test binary as a child:

```rust
#[test]
fn session_append_torn_by_crash() {
    faults::crash::run("session_append_torn_by_crash", |role| match role {
        Role::Child(dir) => {
            // build the store with FaultyFileDriver and
            // Rule { op: Write, trigger: Nth(3), fault: ShortExit { keep: 7 } }
        }
        Role::Parent(dir, status) => {
            assert_eq!(status.code(), Some(faults::crash::CRASH_EXIT));
            // reopen with PosixDriver and assert the recovery contract
        }
    });
}
```

`run` spawns `current_exe()` with `--exact <name> --nocapture
--test-threads=1` and `OTTO_FAULT_CRASH_DIR=<tempdir>`. The child sees the
variable and runs `Role::Child`; the parent waits (bounded, kills on
timeout) and runs `Role::Parent`. `Fault::Exit` calls `libc::_exit`, so no
destructor, buffered writer or lease release runs. This models `SIGKILL`,
not power loss.

Crash scenarios, each one test:

| Scenario | Crash point | Recovery assertion (from the owning spec) |
| --- | --- | --- |
| Session append | `ShortExit` during the Nth record write | reopen truncates the incomplete final line with a warning; dangling tool calls render as "may have run" |
| Session create | after journal create, before root sync | reopen either sees no session or a valid empty one; never a corrupt one |
| Lease takeover | between the two renames that move the old log aside; the lease uses `std::fs` with no seam, so the test builds that on-disk state directly unless an existing test already covers it | the next acquire completes the takeover and reuses the `fenced-*` file |
| Session fork | after rename, before directory sync | fork is either absent or complete |
| Config write (Phase 4) | after temp sync, before rename | previous config intact; no partial file at the target |
| Skillwrite revert | between rename-aside and restore; directory exchanges are outside `publish_file`, so the same state-construction rule as the lease applies | the skill directory is recoverable per the reflection design |
| Turn queue save | `Exit` at the `Rename` and at the `DirSync` of the sidecar write (`remind::replace_private` through `publish_file`) | the reloaded queue holds either the previous entries or the previous entries plus the prompt; never a torn file |
| Workflow commit | `ExitAfter` on the step's `Commit` | the run reopens as `paused` with the step `interrupted` |

Add one end-to-end test beside `tests/sigterm.rs`: run the built binary
against `faults::http` with a script that streams a tool call and then
stalls; when the tool has started, send `SIGKILL`; restart with resume; assert
the turn is reported interrupted, the tool call's certainty is unknown, and a
non-idempotent tool is not re-executed.

## Contract matrices

New integration test files, one per boundary:
`tests/fault_provider.rs`, `tests/fault_session.rs`, `tests/fault_storage.rs`,
`tests/fault_process.rs`, `tests/fault_mcp.rs`, `tests/fault_crash.rs`.
Unit-level rows may live beside the owning module instead. Each test name
states the row.

The expected values come from the owning spec, `docs/development.md` and
the owning module's documented contract. Where none determines a row's
expected value, the implementer asserts the observed behavior, records it in
the plan file and lists it in the PR description. The rows below are the
minimum set.

### Provider (`openai-compatible`)

| Fault | Expected |
| --- | --- |
| connection refused for 3 attempts, then success | success; 4 attempts; 3 `Retry` events with reason `connection failed`; the server records 1 request (refused connections never reach it) |
| connection refused on every attempt | error after 4 attempts; certainty `NotStarted` |
| 429, 500, 503 | error; exactly 1 request; certainty `Completed` |
| reset before the first SSE event | retried (resilience amendment of 2026-10-07); after 4 attempts, `Unknown` with `TransportLost` |
| reset after one content delta | error; 1 request; no retry |
| stream ends without `[DONE]` | error; 1 request |
| malformed SSE event | error; 1 request |
| stall after head past the deadline (the test stops the control with `Deadline`, as `complete_with_timeout` does) | `DeadlineExceeded` |
| cancellation during backoff | `Cancelled`; no further request |
| deadline budget smaller than backoff | stops without waiting out the backoff |

### Provider (`chatgpt`)

| Fault | Expected |
| --- | --- |
| transport failure before output | retry the same request, up to 7 attempts; no tool is re-executed |
| `response.failed` overload before output | retried |
| overload after output started | not retried |
| HTTP 429, 500, 502, 503 or 504 before output | retry the same request; no tool is re-executed |
| Other HTTP status, HTTP 401, stream codec error | terminal; 1 request |
| token source failure | terminal; 0 requests; certainty `NotStarted` |
| body cut short before output | retried |
| body cut short after output | error; 1 request; certainty `Unknown` |

For both providers, one agent-level row: a provider retry never re-executes
an already-executed tool (count tool executor calls).

### Session store (files)

| Fault | Expected |
| --- | --- |
| `Before(EIO)` on journal `Write` | append fails; store poisoned; later appends refused; reopen shows earlier records |
| `Short` on `Write`, then reopen | incomplete final line truncated with a warning |
| `After(EIO)` on `FileSync` | store poisoned; reopen may contain the record; `append_input_if_absent` does not duplicate it |
| `Before(ENOSPC)` on `RootSync` during create | create fails and removes the journal |
| `Before` on `Lock` | open fails; no file left behind |

### Storage (SQL)

| Fault | Expected |
| --- | --- |
| `Before(Busy)` on `Commit` | typed busy error surfaces; no poison |
| `After(commit unknown)` on `Commit` | caller reports unknown, never rollback; connection poisoned; reopen finds the row |
| `Before` on `Rollback` | connection poisoned |
| `Before` on `Open` | the owning service degrades exactly as its spec states |
| `Before` on `Step` mid-iteration | query error; no partial result treated as complete |

Run the SQL rows once per repository that the storage conformance suite
already covers. The pass lives in `tests/fault_storage.rs` and builds the
repositories through `SqlRepositories::new` with a `FaultySqlDriver`;
`storage::conformance` is compiled into the default build through the
default `storage-conformance` feature, so it cannot name `faults`.

### Process and MCP

| Fault | Expected |
| --- | --- |
| child ignores `SIGTERM` | `SIGKILL` after grace; group reaped; no zombie |
| background grandchild outlives leader | killed with the group |
| `Before` on sandbox `Execute` | `NotStarted` |
| MCP `Hang` on `Request` past timeout (stdio) | request times out; server stopped |
| MCP `Hang` on `Request` past timeout (HTTP) | only that request abandoned |
| MCP HTTP 401, refresh succeeds | one retry |
| MCP HTTP 401, refresh fails | `NeedsLogin` |

## Guardrails

- Default tests stay offline: loopback only, no real credentials.
- `tests/faults_boundary.rs` scans non-test source and fails if any module
  other than `faults` itself names `crate::faults` or `otto::faults`.
- `make rust-wasm-check` stays green; `otto-core` is not touched.
- Every new test must fail when its contract is broken. For each matrix
  file, the PR description lists one mutation the implementer applied (for
  example, treating every send error as retryable) and the tests it broke.
- Superseded hooks are removed in the phase that replaces them:
  `StoreState.fail_writes` (Phase 3), `DirectorySync` (Phase 4), the copied
  HTTP servers (Phase 1). Hooks without a seam stay: the lease test hooks,
  `CommitGuard::with_sync`, `seatbelt::state::Operations`, Turso
  `break_for_test` variants and `Inbox::set_persist`.

## Phases

Each phase is one PR, follows the repository workflow in `AGENTS.md`, and
leaves `make check-fast` green.

1. `faults::plan` and `faults::http`; migrate the copied HTTP servers;
   `tests/fault_provider.rs`.
2. Thread the SQL driver into the eight repositories; `faults::sql`;
   `tests/fault_storage.rs` and the conformance fault pass.
3. `faults::files`; `tests/fault_session.rs`; remove `fail_writes`.
4. *(Separate approval.)* `fsops::publish_file`, call-site migrations, publish
   fault rows.
5. `faults::crash`; `tests/fault_crash.rs`; the `SIGKILL` end-to-end test.
6. `faults::mcp`, `faults::sandbox`, `faults::mailbox`;
   `tests/fault_process.rs` and `tests/fault_mcp.rs`.

## Documentation

- `docs/development.md`: a "Fault injection" section that explains the
  plan vocabulary, the `Before`/`After`/`Short` distinction, the HTTP
  scripts, the crash harness and the "prove it fired" rule. Link it from
  the testing section.
- `docs/code-organization.md`: list the `faults` module as test-only.
- `CHANGELOG.md`: not required; this is test infrastructure with no
  user-visible change.

## Acceptance

- Every row in the matrices above has a named test that passes.
- `Cargo.lock` adds no package. Its only change is the `otto` self-edge the
  dev-dependency in D2 records.
- The default build (`cargo build -p otto` without the feature) does not
  compile `faults`.
- Phases 1, 2, 3, 5 and 6 change no production behavior; Phase 4 is
  behavior-preserving at each call site.
- The duplicated HTTP servers and `fail_writes` are gone.

## Resolved questions

1. Self dev-dependency feature or `required-features` on test targets: the
   self dev-dependency. It builds with every existing test target, and
   `required-features` would hide the new targets from a plain
   `cargo test --workspace`.
2. Phase 4 does not cover the lease's directory renames; the lease has its
   own hooks and spec.
3. Lease renewal and watchdog loops taking the `Clock` trait: a later slice.
4. Seeded randomized fault schedules: a later slice, after the deterministic
   matrices exist.

## Amendments

Checked against `main` at `cef0a16` on 2026-10-07:

- Eight Turso repositories bypass the driver, not six: `approval` and
  `memory` were missing.
- `storage::Error` is not `Clone`; `FaultError::Sql` carries `SqlFault`.
- `Trigger::Calls` and `Trigger::AfterBytes` are dropped: no row uses them.
  `Fault::ExitAfter` and `Fault::ShortExit` are added: a crash after a
  commit, and a crash in the middle of a write, cannot be expressed as two
  rules because the caller reacts to the first fault before the second.
- Provider rows follow the code and the 2026-10-07 resilience amendment:
  openai-compatible retries a reset before the first event; refused
  connections never reach the server; ChatGPT's token failure makes no
  request, and a cut body is retried before output.
- `seatbelt::state::write_profile` writes in place and is not migrated in
  Phase 4; `skillwrite::write_atomically` gains a directory sync.
- The storage conformance fault pass lives in `tests/fault_storage.rs`.
- The turn-queue crash row uses the Phase 4 publish seam of the sidecar
  write instead of `FaultyMailbox`.
- The lease and skillwrite crash rows build the intermediate on-disk state,
  because those renames have no seam.
- Unspecified row values are asserted as observed and listed in the PR,
  instead of waiting for a decision.

Phase 6, checked against `main` at `d3f17e7` on 2026-10-07:

- The grandchild row and both MCP 401 rows already had tests
  (`sandbox::conformance` and `mcp::http`); Phase 6 adds only the other four
  rows.
- `FaultyMailbox` is not built: no Process and MCP row uses it, and
  `Inbox::set_persist` failures are already tested in `app::task_control`,
  `app::turn_queue` and `subagent::tasks`.
- The "closes stdout" and "writes past the output cap" fixtures are not
  built, because no row uses them. The `SIGTERM`-ignoring child is a shell
  script inside the conformance check, not a `faults::sandbox` fixture.
- The HTTP hang row uses a `faults::http` script that reads the request and
  never sends a response head, so the real `HttpTransport` abandons it;
  `FaultyTransport` is used for the stdio row.
- `FaultyDriver` reports `Error::ChildLaunch` for every `Before` rule,
  because sandbox errors carry no I/O error.
- The rows live next to the existing harnesses; there is no
  `tests/fault_process.rs` or `tests/fault_mcp.rs`.

Phase 5, checked against `main` at `cb5ee53` on 2026-10-07:

- The session create row has two tests. A crash during the header write left
  a torn `<id>.jsonl`: 0 bytes, or `{"type"` with `ShortExit { keep: 7 }`.
  `Store::open` rejected it ("session file is empty", "session header must be
  one JSON object"), `session::list` counted it as skipped, and `--continue`
  resumed an older session. A new session file is now written as
  `<id>.jsonl.<hex>.tmp`. After the header and runtime record are synced,
  `FileRoot::publish_journal` (`FileOp::PublishJournal`) links it to
  `<id>.jsonl`, failing when that name exists, and unlinks the temporary
  name. A crash before the link leaves only the temporary file, which no
  session scan reads. `linkat` is used instead of `renameat2` with
  `RENAME_NOREPLACE` because the Linux NFS and CephFS clients return `EINVAL`
  for rename flags, and session failover supports both. Files torn by
  earlier versions still fail to open. The second test crashes at the
  session directory sync, as specified.
- `Filter::NameContains` is added: the writes made while a session file is
  created have the temporary name as their subject.
- The lease row builds the on-disk state (epoch 2 marker, `fenced-1.jsonl`,
  no log), as the amendment above says. The skillwrite row does not: the
  child reverts through a reflection store whose `pop_skill_version` calls
  `crash::exit()` after the tree has moved to its `.reflection-*` staging
  directory. Recovery is manual: the staging directory holds the whole tree,
  and moving it back restores the skill. No code removes the staging
  directory or corrects the store row.
- The session append row asserts `MAY_HAVE_RUN_TOOL_RESULT_TEXT` for a tool
  call without an operation record. The bash call in the `SIGKILL` test has a
  durable operation record, so its repaired result starts with "tool result
  unavailable after prior session interruption". The resumed request carries
  the recovery text "Continue the interrupted turn from the transcript".
- The `SIGKILL` test serves the tool call from the scripted server in
  `tests/common` instead of `faults::http`; the command
  `echo ran >> ran.txt; sleep 30` provides the stall and the evidence that
  the resume did not run it again.
- `remind::replace_private` is replaced by `replace_private_with`, which
  takes the publish ops, so the sidecar crash rows can inject them.
