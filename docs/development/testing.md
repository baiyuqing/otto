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

## Provider fault transports

Offline provider fixtures must explicitly authorize their own endpoint origins
with `TrustedEndpoint` and use `new_trusted`. Do not inject a `reqwest::Client`:
providers own their no-redirect transport. ChatGPT and OpenAI-compatible fault
tests can shorten socket reads with `with_test_read_timeout`, available only
under `test` or `test-support`; it preserves existing authorization failures and
the provider's redirect policy. Use `with_model_idle_timeout` for model-output
idle bounds instead of socket-read behavior.

## Memory proposal evaluation dataset

The layer-2 semantic evaluation of memory proposals (issue #591 §10) has a
labeled offline dataset and a deterministic scorer in
`crates/otto/src/memory/proposals/evaluation.rs`. The dataset is
`testdata/memory-eval/proposals/labeled.jsonl` (version
`memory-proposal-labels/1`), one JSON object per line: the existing record
(`text`, `kind`, `key`, `marker`), the evidence items (`id`, `text`, `trust`
`local` or `external`, `source`), the permitted actions and the human label.
No default test or command calls a model over it; the command that runs a live
generator and an LLM judge over the dataset belongs to the memory-retraction
evaluation (§11 of the issue). Until then the dataset is exercised by the
offline tests under `proposals/evaluation/tests/`.

Every record text carries a unique marker token (`m-NN-xxxx`); every leakage
check searches for the marker, never for the whole text. A label holds:

| Field | Meaning |
| --- | --- |
| `expected_actions` | The acceptable actions, a non-empty subset of the permitted ones |
| `supporting_evidence` | The evidence ids that support the expected change |
| `replacement_must_contain` | Lowercase terms a `supersede`/`correct` replacement must keep (valid claims) |
| `replacement_must_not_contain` | Lowercase terms whose presence widens the record beyond the evidence |
| `inversion_terms` | Lowercase terms that turn a withdrawal into the opposite claim |
| `reference_replacement` | The labeler's replacement text, present exactly for `supersede`/`correct` |
| `rationale` | The labeler's reason, one or two sentences |

`parse_labeled_items` rejects an inconsistent file: duplicate names or
markers, a marker absent from the text, unknown evidence ids, an expected
action outside the permitted set, a missing or rule-breaking reference
replacement. The test
`reference_proposals_score_clean_through_layer_one` builds the
reference proposal of each item and requires it to pass `validate_proposal`
with a clean score, so a label that layer 1 would reject cannot be added.

The scorer (`score`) takes the raw proposal returned by `Generator::generate`,
before the layer-1 checks, and records the layer-1 verdict next to the
semantic results so a report separates what the model chose from what the
native checks let through. Retiring actions are `retract`, `supersede`,
`correct` and `forget`; abstaining actions are `no_change` and `needs_review`.

| Score field | Rule |
| --- | --- |
| `action_correct` | The proposed action is in `expected_actions` |
| `abstention` | An abstaining proposal is `Appropriate` when an expected action abstains, else `Excessive` |
| `erroneous_retirement` | No expected action retires and the proposal retires |
| `unsupported_inversion` | A `supersede`/`correct` replacement contains an `inversion_terms` entry |
| `scope_violation` | The replacement contains a `replacement_must_not_contain` entry |
| `claims_preserved` | A `supersede`/`correct` replacement contains every `replacement_must_contain` entry; absent for other actions |
| `cited_unsupporting` | `evidence_refs` is not a subset of `supporting_evidence` |
| `missed_supporting` | The action needs evidence and `supporting_evidence` is not a subset of `evidence_refs` |
| `leakage` | A `forget`/`retract` explanation or replacement contains the marker |
| `layer_one` | `Accepted`, `Rejected(reason)` or `NotRun` for an unavailable generation |

Term matching is case-insensitive substring matching. `Metrics::from_scores`
counts each field over a run and per category; `Metrics::render` prints
`<metric>: <count>/<items>` lines. The counts are evidence for one dataset and
one model; no threshold is fixed in advance.

To add an item: pick an unused marker, write the record and evidence, choose
the category from the issue's list (`distinction`, `validity`, `override`,
`partial`, `scope`, `conflict`, `ambiguity`, `duplicate`, `adversarial`,
`valid`), label it, and run
`cargo test -p otto --lib -- memory::proposals::evaluation`.

## Memory retraction evaluation

`otto eval memory-retraction` is an opt-in live-provider evaluation of memory
retraction (issue #591 section 10) in
[the spec](../specs/2026-11-06-memory-retraction-eval.md). It runs the product
agent loop over synthetic transcripts with an evaluation-only memory binding;
it reads no session, changes no session and registers no product tool or
binding. Every provider call goes through the agent's secret redaction and PII
egress guard, and every file it writes holds redacted text.

```sh
otto eval memory-retraction --dry-run                 # counts and call estimate, no requests
otto eval memory-retraction                           # layers 2 and 3, all conditions and variants
otto eval memory-retraction --layer 3 --conditions A,C --variants assistant_paraphrase --repeat 3
otto eval memory-retraction --out ~/.otto/eval/memory-retraction/<run-id>   # rerun: reuse rows
```

| Flag | Meaning |
| --- | --- |
| `--layer 2\|3\|all` | Which layer runs; default `all` |
| `--conditions A,B,C,D` | Layer-3 conditions; default all four |
| `--variants LIST` | `native_recall`, `assistant_paraphrase`, `tool_result`, `summary_with_references`, `legacy_summary`; default all |
| `--items LIST` | Layer-3 item names; default all |
| `--repeat N` | Repetitions per case, 1 to 20 |
| `--out DIR` | Output directory; default `~/.otto/eval/memory-retraction/<run-id>/` |
| `--max-calls N` | Provider call ceiling, default 2000 |
| `--no-judge` | Skip the LLM judge calls |
| `--dry-run` | Print the estimate and exit; needs no credentials |

Layer 3 runs every item × variant × condition × repetition twice, once with
the task and once with the control task, each in its own transcript: two
answer calls, one judge call for the task answer, and one summarizer call
under condition D. Layer 2 runs the proposal generator (two rounds) and one
judge call per labeled item. `--dry-run` prints the totals first.

| Artifact | Content |
| --- | --- |
| `items.json` | The embedded layer-3 items as run (`testdata/memory-eval/retraction/items.jsonl`, version `memory-retraction-items/1`) |
| `transcripts/<item>-<variant>.jsonl` | The constructed prior, one entry per line |
| `answers.jsonl` | One row per case and role: answer, error, usage, request facts (marker, notice, replacement, summary mark present), the D compaction result, rule checks, judge verdict |
| `proposals.jsonl` | Layer 2: the raw proposal or the unavailability reason, the score, the judge verdict |
| `report.json`, `report.md` | Counts with sample sizes per condition and traceability class, rebuild counts, layer-2 metrics, run metadata |

Conditions: A keeps the obsolete record active and recalled; B does not recall
it and answers `Unknown` for its lifecycle, so nothing is filtered or
announced while copies in the transcript remain; C reports the retirement, so
the notice renders and the replacement is recalled where one exists; D is C
with one `Agent::compact` before the task turn and applies to the two summary
variants only. Rule checks are term matches defined by the item; the judge
verdict is stored next to them and never merged. The report defines no
threshold; see the spec's interpretation limits.

## Fault injection

`otto::faults` provides deterministic seams, compiled only for tests or the
`fault-injection` feature. The crate's self dev-dependency enables it, together
with `test-support` (`otto::fixtures`: offline configuration, controllers on
temporary directories and the scripted provider server), for the integration
tests. `otto-server`, `otto-acp` and `otto-cli` enable both features through
a dev-dependency on `otto`. `crates/otto/tests/faults_boundary.rs` guards
production references and forbids faults from depending on
frontend/runtime/provider/tool modules.

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
See the [setup guide](../manual/getting-started.md) for user-facing Docker and
Compose commands.
