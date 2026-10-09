# Testing

Previous: [Development workflow](workflow.md) · [Documentation index](../README.md) · Next: [Development contracts](contracts.md)

## Tests

Feature work and bug fixes ship with tests that check the behavior the change
adds or fixes:

1. Each test covers behavior the change defines: an input, a state, or an
   error path, not the structure of the implementation.
2. A test must fail when the behavior it covers breaks. When that is not
   evident from the test, name in the PR the assertion that fails without
   the change; do not revert the change locally to demonstrate it.
3. CI runs the tests; see [Local verification](workflow.md#local-verification).

Tests may be written before, during, or after the code. Docs-only and other
non-code changes need no tests.
For tests that inject transport, storage or process faults, see
[Fault injection](testing.md#fault-injection).
Keep unit tests next to the module they cover (`#[cfg(test)] mod tests`), and
integration tests in the crate's `tests/` directory. Prefer
`#[test]`/`#[tokio::test]` and `tempfile::TempDir`. TTY-specific coverage must
stay offline and automated, such as the PTY smoke test in
`crates/otto/tests/tui_pty.rs`. Live provider tests are opt-in only and
excluded from the default suite. Tests that spawn subprocesses while the
parent holds files, advisory locks, sockets, or other descriptors must ensure
those descriptors are close-on-exec unless the child intentionally needs them;
otherwise parallel tests can inherit locks and fail unrelated cases as “already
open” or “resource busy.” Add focused regression coverage for descriptor
inheritance when fixing this class of flake. Contract changes need focused
coverage for ownership, invalid states, cancellation, and history/wire compatibility;
verify both `Session` implementations (`MemorySession` and `Store`) where they
share a contract. Report failing gates and reruns accurately, check the
baseline for unchanged failures, and never weaken validation or safety checks
to obtain a pass. Preserve unrelated behavioral assertions when migrating test
fixtures.

## Fault injection

The `otto::faults` module holds deterministic fault seams for tests. It is
compiled only under `cfg(test)` or the `fault-injection` feature; the `otto`
dev-dependency on itself enables the feature for every integration test, and
`crates/otto/tests/faults_boundary.rs` fails when production code references
`faults` or when `faults` imports a frontend, runtime, provider or tool module.
The design and contract matrices are in
[fault injection testing](../specs/2026-10-07-fault-injection-testing.md).

Fault decorators over a seam take a `FaultPlan<Op>`, where `Op` is the
decorator's operation enum. A plan is a list of `Rule`s: an operation, a
`Filter` on the operation's subject (`Any`, `NameSuffix`, `NameContains` or
`SqlPrefix`), a `Trigger` (`Always`, `Nth(n)` or `FromNth(n)`, counting matching calls from 1)
and a `Fault`:

- `Before(error)` returns the error without performing the operation, so the
  effect did not happen.
- `After(error)` performs the operation and then returns the error. It models
  a lost acknowledgement: the effect happened, the caller sees a failure.
- `Short { keep, error }` writes the first `keep` bytes, then returns the
  error.
- `Exit`, `ExitAfter` and `ShortExit { keep }` end the process with exit code
  86 before the operation, after it, or after a partial write. No destructor
  runs.
- `Hang` blocks an asynchronous operation; synchronous decorators reject it.

A test calls `plan.assert_all_fired()` after the code under test; it panics
with the rules that never fired. Dropping a plan with rules that was never
checked panics in debug builds.

HTTP faults use `otto::faults::http`, a raw-TCP loopback server:

- `spawn(handler)` answers each request from a closure; `spawn_repeating`
  and `spawn_scripted` replay `Script` values, one per connection.
- A `Script` is a list of `Step` values: `Head`, `DeclareLength`, `Body`,
  `Chunk`, `Sse`, `Flush`, `Stall`, `Close` and `Reset` (a TCP reset). The
  constructors `sse_done`, `sse_without_done`, `sse_cut_after`,
  `malformed_event`, `status`, `stall_before_head`, `stall_after_head`,
  `stall_after_events` and `truncated` build the common scenarios.
- `Server::requests()` and `count()` return the requests received. A
  connection past the last scripted one gets a 500, so an unplanned retry is
  a visible failure.
- `refused_port()` returns a port with no listener, and
  `late_listener(port, after, scripts)` starts listening after a delay.

A test that reads a response from a live socket runs in real time: paused
tokio time advances into the client's 60 s read timeout while the socket is
idle. The slowest provider rows wait out the 1, 2 and 4 s retry backoff.

A fault test must prove the fault fired: assert the request count or the
recorded request, not only that the client returned an error. The provider
matrix is `crates/otto/tests/fault_provider.rs`; each test name states its row.

SQL faults use `otto::faults::sql::FaultySqlDriver`, which wraps an
`SqlDriver` and every connection, statement and row cursor it opens. Each
Turso repository has an `open_with_driver` constructor, and
`tests/storage_boundary.rs` fails if a repository calls `Connection::open`
directly. The rule subject is the database path for `Open`, `Begin`,
`Commit`, `Rollback`, `Checkpoint` and `Close`, and the SQL text for
`ExecuteBatch`, `Execute`, `Prepare`, `Query` and `Step`; select statements
with `Filter::SqlPrefix`. Errors are `FaultError::Sql`. The decorator applies
the real driver's poisoning rules: a commit error other than busy or
constraint is reported as `CommitUnknown`, and after `CommitUnknown` or a
failed rollback every later call on that connection returns `Closed`. The
repository matrix is `crates/otto/tests/fault_storage.rs`.

File faults use `otto::faults::files::FaultyFileDriver`, which wraps a
`FileDriver` (normally `PosixDriver`) and every root and journal it opens, so
a rule reaches journals under nested directories. A session store takes it
through `Store::create_lazy_with_driver` and the other `*_with_driver`
constructors. The rule subject is the path for `OpenRoot`, the root's name
for `RootSync` and `ListNames`, and the journal's name for journal
operations. A new session file is written as `<id>.jsonl.<hex>.tmp` and
published to `<id>.jsonl` by `PublishJournal`, whose subject is the final
name, so select a session journal with `Filter::NameContains(".jsonl")`.
`Short` and `ShortExit` apply only to `Write`, which counts `Write::write`
calls. Errors
are `FaultError::Os` or `FaultError::Kind`. The session store matrix is
`crates/otto/tests/fault_session.rs`.

Atomic file publishing goes through `fsops::publish_file`. It creates a
temporary file in the destination directory exclusively, without following
a symbolic link and with exact permissions, writes and syncs it, renames it
over the destination (`PublishMode::Replace`, or `Exclusive`, which fails
with `EEXIST` and leaves an existing destination unchanged) and syncs the
directory. A write, sync or rename failure removes the temporary file; a
`CreateTemp` failure leaves an existing entry at the temporary name in place.
`PublishError` names the failed `PublishStep` (`CreateTemp`, `Write`,
`FileSync`, `Rename`, `DirSync`); `committed` is `true` only for a `DirSync`
failure, after the rename returned `Ok` (the write and edit tools report it
as an unknown outcome). The write and edit tools, the reminder and inbox
sidecars (`tool::remind::replace_private_with`), the workspace identity,
configuration writes, reflection skill files and session forks use it.
Publish faults use `otto::faults::publish::FaultyPublishOps`, a
`PublishOps` over a `FaultPlan<PublishStep>`; the rule subject is the
destination name, and `Short` and `ShortExit` apply only to `Write`. Each
call site takes the ops through a test constructor or a `*_with` function,
and its unit tests map each step to that site's error and effect certainty.

Sandbox launch faults use `otto::faults::sandbox::FaultyDriver`, a
`sandbox::Driver` over a `FaultPlan<SandboxOp>`; the rule subject is
`argv[0]`, and `Before` returns `Error::ChildLaunch` without running the
inner driver. MCP faults use `otto::faults::mcp::FaultyTransport`, an
`mcp::Transport` over a `FaultPlan<McpOp>`; the rule subject is the JSON-RPC
method (empty for `Close`), and `Hang` ignores the cancellation token, so the
client's call timeout and grace end the call. Signal and reaping faults use
real child processes in `sandbox::conformance`, registered in
`driver_contract!`, so the direct driver and Seatbelt each run them. The
process and MCP rows map to these tests:

| Row | Test |
| --- | --- |
| child ignores `SIGTERM` | `sandbox::conformance::an_ignored_term_escalates_to_kill_and_removes_the_group` |
| background grandchild outlives the leader | `sandbox::conformance::normal_leader_exit_removes_a_background_process`, `deadline_cancellation_removes_the_process_group` |
| `Before` on `Execute` | `tool::bash::tests::a_driver_launch_failure_is_not_started_and_runs_nothing` |
| MCP request hangs past the timeout (stdio) | `test_hung_stdio_request_times_out_and_stops_the_server` in `crates/otto/tests/mcp_stdio.rs` |
| MCP request stalls past the timeout (HTTP) | `mcp::client::tests::a_stalled_http_request_times_out_and_the_next_request_succeeds` |
| MCP HTTP 401, refresh succeeds or fails | `mcp::http::tests::unauthorized_then_refresh_then_retry_succeeds`, `unauthorized_then_refresh_fails_needs_login` |

Crash faults use `otto::faults::crash`. `run(name, body)` re-executes the
current test binary with `--exact <name> --nocapture --test-threads=1` and
`OTTO_FAULT_CRASH_DIR` set to a new directory. The child runs
`body(Role::Child(dir))`, which must end at an `Exit`, `ExitAfter` or
`ShortExit` fault; a child whose body returns fails the test. The parent
waits up to 120 s, killing the child after that, then runs
`body(Role::Parent(dir, status))` and reopens what the child left with the
real drivers. `name_of(&|| {})` returns the calling test's libtest name, and
`crash::exit()` ends the process at a seam with no fault plan, such as a test
reflection store. A crash runs no destructor, so it models `SIGKILL`, not
power loss. The crash rows map to these tests:

| Row | Test |
| --- | --- |
| Session append | `a_crash_during_a_journal_append_leaves_a_torn_line_that_reopen_truncates_and_a_dangling_call_may_have_run` in `crates/otto/tests/fault_crash.rs` |
| Session create | `a_crash_while_writing_the_session_header_leaves_no_session_file`, `a_crash_before_the_session_directory_sync_leaves_no_session_or_a_valid_one` in `fault_crash.rs` |
| Lease takeover | `storage::backends::posix::lease::tests::a_crash_between_the_two_renames_of_a_takeover_is_completed_by_the_next_acquire` |
| Session fork | `storage::backends::posix::session::tests::a_crash_after_the_fork_rename_leaves_a_complete_fork_and_no_temporary_file` |
| Config write | `config::tests::a_crash_before_the_config_rename_keeps_the_previous_config_at_the_target` |
| Skillwrite revert | `reflection::skillwrite::tests::a_crash_after_a_revert_moves_the_skill_aside_keeps_the_whole_tree_in_a_staging_directory` |
| Turn queue save | `runtime::sidecar_mailbox::tests::a_crash_before_the_sidecar_rename_reloads_the_previous_queue`, `a_crash_before_the_sidecar_directory_sync_reloads_the_previous_queue_and_the_prompt` |
| Workflow commit | `a_crash_after_the_step_claim_commit_is_recovered_as_a_paused_run_with_an_interrupted_step` in `fault_crash.rs` |
| `SIGKILL` during a tool call | `sigkill_during_a_tool_call_leaves_a_session_that_reopens_with_the_call_marked_unknown_and_resumes` in `crates/otto/tests/sigkill.rs` |
## Docker acceptance

After changing `Dockerfile`, `.dockerignore` or `compose.yaml`, build the
image with `docker build -t otto:local .`, then run
`node scripts/docker-smoke.mjs`. This opt-in test requires Docker Compose
and a free localhost port 8787. It uses a disposable project, workspace and
state volume; it calls no live provider and deletes its own resources.
It checks the real setup flow across recreation, non-root execution,
API token enforcement and rotation, session persistence,
workspace writes and graceful SIGTERM shutdown. It does not run as part of
the default offline script tests. Check Docker Desktop on each Mac
architecture separately; Linux execution cannot verify macOS file sharing.

For managed environments with an inspection proxy, supply a trusted combined
CA bundle:

```bash
docker build --secret id=proxy_ca,src=/path/to/ca-bundle.pem -t otto:local .
```

The build uses this optional secret only during networked
toolchain and dependency installation; it is not copied into the image.

---

Previous: [Development workflow](workflow.md) · [Documentation index](../README.md) · Next: [Development contracts](contracts.md)
