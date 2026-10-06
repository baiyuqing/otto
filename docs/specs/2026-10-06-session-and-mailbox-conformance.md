# Session and mailbox conformance checks

Status: proposed 2026-10-06, awaiting approval. No production code or tests
until approved.

This design follows [crash recovery and interrupted-turn resume](2026-10-06-crash-recovery-and-turn-resume.md)
and [durable inputs](2026-10-06-durable-inputs.md). It closes the
"session-journal and mailbox conformance" item listed under "Later slices" in
the crash-recovery design. It also complements
[external storage backends](2026-10-06-external-storage-backends.md), whose
public `storage::conformance` checks cover the SQL domains only.

## Problem

Facts from the code at `3f5cf4f`.

Two ports now carry Otto's crash-safety guarantees:

- `otto_core::session::Session` covers the following:
  - the message ordering rule;
  - `append_input_if_absent` and `has_input`, which give exactly-once
    delivery;
  - `interrupted_turn`, folded from `otto.turn`, which drives turn resume;
  - the operation ledger, which drives replay decisions;
  - `observed_capabilities`;
  - compaction checkpoints.
- `otto_core::mailbox::Mailbox` covers the following:
  - FIFO snapshot and `ack` by sequence;
  - deduplication by `id` on enqueue;
  - `load`, which continues sequence numbering;
  - persistence errors reported by `enqueue`.

Several things leave these guarantees unprotected:

- **Many implementations.** `Session` has 6 production implementations:
  `MemorySession`, POSIX `Store`, `SharedSession`, `MemoryHandle`,
  `SharedTranscript`, and the CLI's `FixedPathSession`. It also has test
  wrappers. `Mailbox` has `Inbox`, plus the durable sidecar adapter built on
  it.
- **Hand-written forwarding.** Every wrapper forwards each method by hand. The
  last two PRs added `interrupted_turn`, `append_input_if_absent` and
  `has_input` to every wrapper. A wrapper that forgot one would compile with a
  wrong default or a stub and would silently disable recovery or
  deduplication. Only a behavioural check would catch that, and today none
  exists.
- **A future backend has nothing to test against.** The external storage
  backends design expects a non-POSIX session store later. Its author could not
  tell whether it keeps these guarantees.
- **The existing checks cover other stores.** `storage::conformance` covers
  approvals, memory, schedule, workflows, tasks, usage, reflection, skill
  checks and memory advice. Their tests live in
  `crates/otto/tests/external_storage_backend.rs`. Nothing covers sessions or
  mailboxes.

## Goals

1. Provide public, WASM-safe check functions in `otto-core` for both ports, so
   the browser runtime and any external backend can run the same contract.
2. Run them in the repository against every production implementation and
   wrapper. A wrapper that drops a guarantee then fails a test that names the
   broken check.
3. Where an implementation is durable, also run the checks across a reopen.

## Non-goals

- Fault injection: torn writes, `fsync` loss, a full disk. These stay with the
  POSIX backend's own tests.
- Concurrency stress and performance limits.
- Lease and fencing behaviour, which belongs to the failover tests.
- New behaviour in either port. If a check exposes a gap, the fix ships in the
  same PR only when it is a forwarding bug. Anything larger is reported.

## Design

### Shape

```rust
// otto-core, behind the `conformance` feature (default on, like
// otto's `storage-conformance`):
pub mod conformance {
    pub struct Failure { pub check: &'static str, pub detail: String }

    /// Runs every session check against fresh sessions from `fresh`.
    /// `reopen`, when given, closes and reopens a session from the same
    /// backing storage; the durable checks then run across it.
    pub async fn session<S: Session>(
        fresh: &(dyn Fn() -> S + Sync),
        reopen: Option<&(dyn Fn(S) -> S + Sync)>,
    ) -> Result<(), Failure>;

    /// The same for mailboxes. `reopen` rebuilds a mailbox from the same
    /// durable state, as the native adapter does after a restart.
    pub fn mailbox(
        fresh: &dyn Fn() -> Arc<dyn Mailbox>,
        reopen: Option<&dyn Fn(Arc<dyn Mailbox>) -> Arc<dyn Mailbox>>,
    ) -> Result<(), Failure>;
}
```

A check name is stable, for example `session.input_id.already_present`, so a
failing backend sees exactly which guarantee broke.

### Session checks

Each check starts from a fresh session.

| Check | Contract |
| --- | --- |
| `session.order` | Messages return in append order. Ids are unique: a duplicate id is rejected. |
| `session.tool_sequence` | A tool result must answer a pending call of the previous assistant message. Nothing else may come between a call and its result. A duplicate call id is rejected. A rejected append stores nothing. |
| `session.input_id` | `append_input_if_absent` appends once, then reports `AlreadyPresent`. `has_input` agrees. An assistant message with an input id is rejected. |
| `session.turns` | A start record with no end record makes `interrupted_turn` return that turn and its attempt. The matching end record clears it. An end record for another turn changes nothing. |
| `session.capabilities` | The latest `otto.capabilities` record wins. |
| `session.operations` | An attempt fact, then a terminal fact, fold into one record; `retrySafety` is kept. A conflicting terminal fact is rejected. |
| `session.compaction` | After a checkpoint, `latest_compaction` names it, and `model_messages` starts with the summary followed by the retained messages. |
| `session.reopen` (with `reopen`) | Everything above survives a reopen. This includes input ids, the interrupted turn, the ledger and the checkpoint. |

### Mailbox checks

| Check | Contract |
| --- | --- |
| `mailbox.fifo` | `queued()` lists entries in enqueue order. `ack(seq)` removes only that entry. An entry enqueued after a snapshot comes after the snapshot's entries. |
| `mailbox.dedup` | Enqueueing a notification whose id is already queued returns the existing sequence number and adds nothing. |
| `mailbox.remove` | `remove(task_id, kind)` removes the first match only. |
| `mailbox.load` | `load` restores the given entries in order. The next enqueue gets a sequence number above the highest one loaded. |
| `mailbox.reopen` (with `reopen`) | Queued entries, ids and order survive. Acknowledged entries do not come back. |

### Where they run

| Implementation | Fresh | Reopen |
| --- | --- | --- |
| `MemorySession` | otto-core tests | — |
| POSIX `Store` | otto tests, temp dir | `Store::open` on the same file |
| `SharedSession` over a `Store` and over memory | otto tests | over `Store` |
| `MemoryHandle`, `SharedTranscript`, `FixedPathSession` | otto tests | where file-backed |
| `Inbox` | otto-core tests | — |
| sidecar adapter (`runtime::sidecar_mailbox`) | otto tests, temp dir | `load` from the same file |

The test wrappers inside `otto-core/src/agent/run_tests.rs` are fixtures that
inject failures on purpose, so they are not run.

### Documentation

- `docs/development.md`: the storage section lists the new checks next to
  `storage::conformance`. A new `Session` or `Mailbox` implementation must
  add a conformance test.
- No user-manual change: there is no user-visible behaviour.

## Ownership

| Area | Change |
| --- | --- |
| `crates/otto-core/src/conformance.rs` (new), `Cargo.toml` feature | check functions |
| `crates/otto-core` tests | memory session, `Inbox` |
| `crates/otto/tests/session_conformance.rs` (new) | `Store`, every wrapper, sidecar adapter |
| `docs/development.md`, `CHANGELOG.md` | contract and entry |

## Acceptance

- Every check passes for every implementation in the table.
- Mutation evidence: for each port, a deliberately broken wrapper (for example
  one whose `has_input` always returns false) fails with the expected check
  name. These broken wrappers live only in the conformance module's own
  tests.
- `make check-fast` passes; the checks also compile for `wasm32` (the
  `rust-wasm-check` target).

## Open questions

1. **Placement.** Put the checks in `otto-core` behind a default-on
   `conformance` feature, or in `otto`'s existing `storage-conformance`
   module? Recommended: `otto-core`, because both ports and the browser
   runtime live there.
2. **Wrapper findings.** If a wrapper turns out to break a check, may the fix
   ride this PR? Recommended: yes for forwarding fixes; anything else is
   reported first.
