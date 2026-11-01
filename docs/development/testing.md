# Testing

[Development guide](../development.md) · [Verification commands](workflow.md#local-verification)

## Tests

Feature/fix tests must exercise observable inputs, states and failure paths, not
implementation structure. They must fail when the behavior breaks. If this is
not evident, name the failing assertion in the PR; do not revert the change
locally to prove it. Run focused tests before opening the PR; documentation-only
changes need no new tests.

- Keep unit tests adjacent to their modules and integration tests in crate
  `tests/` directories. Prefer `#[test]`, `#[tokio::test]` and temporary roots.
- Default tests are offline: no real providers, credentials or external services.
  TTY coverage uses controlled PTYs (`tui_pty` and `tui_attach_pty`); live provider
  probes are explicit opt-ins.
- Subprocess tests must mark descriptors close-on-exec unless inheritance is
  intentional. Leaked files/locks/sockets cause unrelated parallel tests to fail.
  Add descriptor-inheritance regression coverage when fixing such flakes.
- Contract changes cover ownership, invalid states, cancellation and history/wire
  compatibility. Test both `MemorySession` and native `Store` for shared behavior;
  run session/mailbox/storage conformance for new implementations and wrappers.
- Report failures and reruns accurately; distinguish unchanged baseline failures.
  Never weaken safety/validation or delete unrelated assertions to pass a gate.
- File-memory tests use temporary roots; legacy import tests construct temporary
  schema-version-2 databases, not real user stores.

## Summary-tree view evaluation

`otto eval view-tree` is an opt-in live-provider evaluation of the summary-tree
context view in [the spec](../specs/2026-10-27-view-tree-eval.md). It reads an
existing JSONL session without taking its writer lease and does not change how
sessions run. Every provider call goes through the agent's secret redaction and
PII egress guard, and every file it writes holds redacted text.

```sh
otto eval view-tree all --session <id|path> --dry-run   # call and byte estimate, no requests
otto eval view-tree all --session <id|path>             # build, probes, run, report
```

`all` runs `build`, `probes` and `run`; each action can also run alone. A rerun
with the same output directory (default `~/.otto/eval/view-tree/<session-id>/`)
reuses built nodes, probes and answers.

| Action | Provider calls | Output |
| --- | --- | --- |
| `build` | One compressor call per message or sibling pair over 512 bytes, up to 5 attempts each | `log.jsonl`, `tree.jsonl`, `build_calls.jsonl`, `view.json`, `build.json`, `cost.json` |
| `probes` | One generation and one validation call per probe | `probes.jsonl`, dropped probes kept with reasons |
| `run` | Per probe: a view answer (up to `--max-steps` calls), a baseline answer and judge calls | `answers.jsonl`, `report.json`, `report.md` |

`build` and `all` take `--session`; `probes` and `run` take `--out` and read the
session recorded in `build.json`. `--dry-run` needs no credentials.

The compressor model defaults to the first `subagent_models` entry of a
converted legacy profile, else the selected model. The number of compressor
calls grows with the number of session messages; run `--dry-run` first. Use
`--view-low`/`--view-high` (KB) to force compression on short sessions.
`cost.json` is computed from message byte sizes with the multipliers given by
flags; it is not a measured bill. `report.md` prints pass or fail for the spec's
acceptance criteria.

## Fault injection

`otto::faults` provides deterministic seams, compiled only for tests or the
`fault-injection` feature. The crate's self dev-dependency enables integration
tests. `crates/otto/tests/faults_boundary.rs` guards production references and
forbids faults from depending on frontend/runtime/provider/tool modules.

A `FaultPlan<Op>` holds rules with operation, subject filter (`Any`, `NameSuffix`,
`NameContains`, `SqlPrefix`), trigger (`Always`, `Nth`, `FromNth`, counted from 1)
and effect:

| Effect | Meaning |
| --- | --- |
| `Before(error)` | No operation/effect happened |
| `After(error)` | Operation happened; acknowledgement was lost |
| `Short { keep, error }` | Partial write then failure |
| `Exit`, `ExitAfter`, `ShortExit` | Process exits with 86; no destructors |
| `Hang` | Async blocking until exercised recovery; synchronous decorators reject it |

Always call `plan.assert_all_fired()` after the code under test. Unchecked plans
with rules panic on drop in debug builds. Verify state/effect certainty after the
fault, not merely the returned error. Before/after faults are not interchangeable.

### Seams and entry points

| Seam | Implementation / focused coverage |
| --- | --- |
| HTTP/SSE | `faults::http`: raw TCP loopback server with controlled chunks, barriers and disconnects |
| SQL | `faults::sql::FaultySqlDriver`; repository `open_with_driver`; `tests/fault_storage.rs` |
| Session files | `faults::files::FaultyFileDriver`; Store `*_with_driver`; `tests/fault_session.rs` |
| Atomic publish | `faults::publish::FaultyPublishOps`; `fsops::publish_file` call-site unit tests |
| Sandbox launch | `faults::sandbox::FaultyDriver`; Bash tests and sandbox conformance |
| MCP | `faults::mcp::FaultyTransport`; client timeout/cancellation tests |
| Signals/process cleanup | Real subprocesses in `sandbox::conformance`, registered for each driver |

SQL subjects are database paths for connection/transaction operations and SQL
text for statements. Select statements with `SqlPrefix`. A commit failure other
than busy/constraint is `CommitUnknown`; uncertain commits or failed rollbacks
poison the connection so later operations return `Closed`. Never infer rollback
from a lost acknowledgement. `tests/storage_boundary.rs` prevents repositories
from bypassing injected drivers with direct connection opens.

File subjects are root/journal names as defined by the operation enum. Journal
publication uses the final `.jsonl` name, while partial writes count actual
`Write::write` calls. Wrap the entire driver so nested roots/journals are covered.

Atomic publishing creates an exclusive, no-follow temporary file in the target
directory, writes/syncs it, renames and syncs the directory. `PublishMode::Exclusive`
leaves an existing destination untouched. Faults cover `CreateTemp`, `Write`,
`FileSync`, `Rename`, `DirSync`; only a failed directory sync after successful
rename reports `committed = true` (unknown durability). Earlier failures clean
the temporary file, except an existing entry rejected at `CreateTemp`. Tools
must preserve this effect certainty instead of suggesting safe replay.

MCP `Hang` deliberately ignores cancellation to test the client's timeout/grace
boundary. Process cases cover ignored TERM, escalation to KILL, surviving
background descendants, deadlines and cleanup after leader exit. Run driver
conformance on the actual supported host: Linux cannot validate Seatbelt.

## Docker acceptance

The container path uses the root `Dockerfile` and `compose.yaml`; Docker Desktop
on Mac is the documented deployment. Check build, first-run setup, authenticated
API access, persistent config/sessions and restart behavior when changing that
path. Use disposable workspace/volume data and no live provider credentials for
default verification. Docker does not establish native macOS Seatbelt behavior.
See [installation](../manual/getting-started.md) for user-facing setup and the
root README for Compose commands.
