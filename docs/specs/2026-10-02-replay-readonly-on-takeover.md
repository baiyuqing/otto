# Replay read-only tool calls after a takeover

Status: draft 2026-10-02, awaiting approval. No production code or tests are
written until this design is approved. It is a slice of the approved
[resilience, retry, and idempotency](2026-09-28-resilience-retry-idempotency.md)
design (Phase 4's `ReadOnly` declaration plus a new consumer of it) and of
[session failover](2026-09-28-session-failover.md); where they disagree, this
document names the exact difference below.

## A read-only call interrupted by a takeover costs the model a turn

Facts from the code at `123c0c6`:

- When a session is opened from a taken-over lease epoch,
  `Store::repair_dangling_tool_calls` (`crates/otto/src/session/store.rs`)
  gives every tool call without a result a synthetic error result
  (`OPERATION_RESULT_UNAVAILABLE_TEXT`) and settles its operation as
  `Interrupted + Unknown + ProcessLost`.
- `failover::recovery::notify` then tells the resumed agent which calls were
  left unanswered, and the first of them is marked `may_have_run`.
- The model must notice the notice, decide the call is safe, and re-issue it.
  For `read`, `ls`, `grep`, `find` this is always safe, yet it costs a model
  round trip and depends on the model doing it.
- `RetrySafety` exists in `otto-core` (`model.rs`) but no tool declares one:
  the registry contract in Phase 4 of the resilience design is not
  implemented.

## Goal

After a takeover, a dangling call whose tool is declared `ReadOnly` is run
again by Otto, under the same operation id, instead of receiving a synthetic
error. Every other call keeps today's behavior.

## Non-goals

- No replay of `Idempotent`, `IdempotentWithKey` or `NonIdempotent` calls,
  `write`, `edit`, `bash`, MCP tools, `remember`, `remind`, sub-agent or
  workflow control. Those remain Phase 4/5/6 gates.
- No per-call widening of safety from arguments; the declaration is the
  tool's base safety only.
- No replay of an interrupted model response or sub-agent transcript.
- No new session record type and no change to the Pi v3 format.

## Design

### Declaration

The native tool registry carries a base `RetrySafety` for each built-in tool,
separate from the provider-visible schema (the Phase 4 registry contract).
This slice declares only: `read`, `ls`, `grep`, `find`, `memory_search` as
`ReadOnly`; every other tool, including every MCP tool, as `NonIdempotent`.
`skill` is `NonIdempotent` until audited. A tool without a declaration is
`NonIdempotent`. `otto-core` gets no new tool types; the declaration lives in
`crates/otto/src/tool` and reaches the agent through the existing executor
boundary.

### Who decides, and when

The store cannot decide: it opens before tools exist and must stay free of
tool knowledge. The decision is split:

1. **Store.** `Store::from_file` is given a `replayable: &dyn Fn(&str) -> bool`
   by the composition root (`cli::wiring`), true for tool names declared
   `ReadOnly`. In `repair_dangling_tool_calls`, if **every** pending call of
   the trailing assistant message is replayable, the store settles nothing
   and appends nothing, and records them in `Takeover::replayable` instead of
   `Takeover::repaired`. If any pending call is not replayable, all of them
   are repaired as today. All-or-nothing keeps results in call order and keeps
   the "calls with results are a prefix" invariant of the turn loop.
2. **Agent.** The resume path in `otto-core::agent` runs pending calls
   through the normal tool execution path before the first model request:
   same operation id, `attempt + 1`, same deadline, cancellation and
   append-before-next ordering as a live call. Their results are appended
   exactly as a first execution would be.

Because history is append-only, a replayed call has no synthetic result to
remove: the store simply left it pending.

### Bounds

- At most one replay per call. If the process dies again during a replay,
  the next open sees `attempts >= 2` in the operation ledger and falls back to
  the synthetic repair. Prevents a crash loop on a call that kills the process
  (e.g. a huge `grep`).
- Cancellation or a deadline during replay settles the call as any live call
  would; the turn ends cancelled.
- A replay that errors is a normal tool error result; it is not retried.

### Notification

`failover::recovery` still reports the takeover, but lists replayed calls
separately ("re-ran read(...) after the session moved; results below are
current") so the model knows a result may differ from what it would have seen
before the interruption. The notification is pushed before the replayed
results.

### Difference from the approved designs

- The resilience design's automatic-retry rules (Phase 4) cover retry inside
  a live process; this adds one new trigger, process loss, limited to
  `ReadOnly`. Its line "`Unknown` never causes automatic replay" is
  qualified to "except `ReadOnly` operations, once".
- The failover design's "process loss never automatically replays an
  interrupted step" for workflows is unchanged; this slice does not touch
  workflows or sub-agents.

## Ownership

- `crates/otto/src/tool`: base safety declarations and their architecture
  guard (a tool without a declaration fails a test naming this document).
- `crates/otto/src/session`: `replayable` predicate, `Takeover` split.
- `crates/otto-core/src/agent`: replay of pending calls on resume. Stays
  wasm-safe; it only uses the existing executor trait.
- `crates/otto/src/failover/recovery.rs`: notification wording.
- `crates/otto/src/cli/wiring.rs`: composition of the predicate.

## Acceptance

Offline, deterministic tests that fail if the behavior breaks:

- Takeover with a dangling `read`: no synthetic result appended, the call
  runs once with `attempt 2` and the same operation id, the result is
  appended and the model sees it without issuing a call.
- Dangling `[read, bash]` in one message: both get synthetic results, nothing
  runs (all-or-nothing).
- Dangling `bash` or an MCP tool: unchanged.
- Second takeover during a replay: falls back to synthetic repair.
- Cancellation during replay ends the turn cancelled and settles the operation.
- Architecture guard: every registered built-in tool has a declared safety.
- Notification lists replayed and repaired calls separately.

Docs: the user manual's "Continuing a session on another host" section is
updated in the same change; README unchanged.

## Open questions

1. Is all-or-nothing per message acceptable, or should a leading run of
   `ReadOnly` calls be replayed and the rest repaired? The prefix variant
   needs the store to append synthetic results after replay output, which
   complicates ordering; I recommend all-or-nothing.
2. Should `skill` (reads skill files) and `memory_search` be in the first
   set? Recommended: `memory_search` yes, `skill` after an audit.
3. Should the same predicate later feed the SIGTERM migration path
   (`notify_moved`)? Out of scope here; the design does not preclude it.
