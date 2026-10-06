# Crash recovery and interrupted-turn resume

Status: proposed 2026-10-06, awaiting approval. No production code or tests
until approved.

This extends [replay read-only tool calls after a takeover](2026-10-02-replay-readonly-on-takeover.md),
[session failover](2026-09-28-session-failover.md) and Phase 4 of
[resilience, retry, and idempotency](2026-09-28-resilience-retry-idempotency.md).
Where they disagree, this document names the difference below.

The comparison point is Pi Durable (`earendil-works/pi`, `packages/durable`).
It resumes interrupted work on every open. A run that was interrupted goes back
to pending from its checkpoint. A model request is resent from its recorded
cutoff. A tool call is rerun only when both the stored and the current
declaration say it is safe to replay. Otto already offers stronger write
durability and cross-host fencing than Pi Durable. What Otto lacks is resume
after a plain process crash.

## Problem

Facts from the code at `9eafd43`:

- **Takeover gets full recovery; a plain crash does not.** On a lease
  takeover, `Store::from_file` leaves replayable dangling calls pending, and
  `failover::recovery::notify` does three things: it pushes a recovery notice,
  marks interrupted children, and starts a wake turn. Without a takeover the
  replay predicate is forced to `|_| false`
  (`storage/backends/posix/session/store.rs`, `let replayable: fn(&str) -> bool = if taken_over …`)
  and `Takeover` is `None`. A process killed by OOM, a panic, a laptop shutdown
  or `kill -9` therefore leaves the following:
  - every dangling call gets a synthetic error result, even `read`;
  - no notice is pushed and no wake turn starts;
  - children are never marked interrupted, so `agent resume` rejects them.
    `tasks.db` only infers their state from PID liveness.
- **A turn that dies during the model stream disappears silently.** Nothing is
  written during the stream. The transcript then ends on the user message (or
  on tool results), exactly as it does after a provider error or a user
  cancel. With nothing on disk to tell these cases apart, Otto cannot resume
  the turn safely after a crash, and today it does not.
- **Only the current replay declaration is checked.** The attempt fact does not
  record the safety level the call ran under, so if a later binary reclassifies
  a tool, that classification would apply to a call made under the old one.

## Goals

1. After process loss, the next open runs the same recovery for every
   file-backed session as a takeover does, with or without a lease:
   - replay or repair the dangling calls;
   - push a notice;
   - mark interrupted children.
2. A turn that process loss interrupted resumes automatically, at most once,
   the next time a frontend that runs wake turns opens the session.
3. A call is replayed only when the safety recorded with its attempt and the
   current declaration both allow it.
4. History stays append-only and Pi v3 compatible. Old sessions open unchanged,
   and older Otto binaries can still read sessions written by this one.

## Non-goals

- **Persisting partial model output during a stream.** The resumed turn sends a
  new request built from the committed transcript. Partial output exists only
  to show progress in the UI, and Pi v3 has no record type for it.
- **Provider request identity or deduplication.** A resumed request is a new,
  billed request.
- **Replaying anything that is not `ReadOnly`.** This covers `write`, `edit`,
  `bash`, MCP tools, sub-agent and workflow control. MCP annotation trust is a
  separate gate.
- **Durable inputs** (left for a later spec):
  - steering;
  - serve turn queue;
  - `task_queue`;
  - child `agent_send`;
  - submission `requestId`;
  - notification IDs (resilience Phase 3).
- **Background or resumable compaction, session fork, workflows.**
- **Reopening sessions on `otto serve` startup.** A session resumes when a
  client opens it, so this design starts no unattended provider spending.
- **Changing the SIGTERM migration path.** It already cancels, records the
  outcome and notifies.

## Design

### 1. Turn record `otto.turn`

`otto-core::agent` appends a Pi v3 `custom` entry with `customType`
`otto.turn` through the existing `Session::append_custom`. The model never
sees it.

```json
{"schemaVersion":1,"event":"started","turnId":"turn_…","trigger":"user","attempt":1}
{"schemaVersion":1,"event":"started","turnId":"turn_…","trigger":"recovery","attempt":2,"resumes":"turn_…"}
{"schemaVersion":1,"event":"ended","turnId":"turn_…","outcome":"completed"}
```

**Fields**

- `trigger` is one of:
  - `user`: a prompt or follow-up;
  - `wake`: a notification wake turn;
  - `recovery`: this design's resume turn.
- `outcome` is one of `completed`, `failed`, `cancelled`.
- `attempt` counts turns in one recovery chain. It is 1 unless the turn is a
  recovery turn, which carries `resumes` and the interrupted turn's attempt
  + 1.

**When the agent writes them**

- **Start record**: at the start of `run`, before `replay_pending_calls`. It is
  the first durable write of the turn, so a crash anywhere later leaves a
  start record with no matching end record.
- **End record**: on every return path of `run`:
  - `AgentFinished` → `completed`;
  - `fail()` with a cancellation or deadline stop reason → `cancelled`;
  - any other error → `failed`.

  If the end record cannot be written, the turn still returns its original
  error. A failed write already halts the store, so later writes fail too
  (existing behavior).
- **Turns that never start**: a run rejected before admission writes nothing.

**Reading them**

`otto-core::session::context` folds the records in the same way it folds
`otto.capabilities`. The newest start record with no matching end record is
the **interrupted turn**. Records that are malformed or use an unsupported
schema produce a warning and are ignored. Older Otto binaries skip
`otto.turn` entirely, because unknown custom types are already skipped.

**Cost**: two short lines per turn, well within the 256 MiB session cap.

### 2. Recovery replaces takeover-only state

`Takeover` becomes `Recovery`. It holds:

- `cause`: `TakenOver(holder)` or `ProcessLost`;
- `repaired`: the dangling calls that got synthetic results;
- `replayable`: the dangling calls left pending for replay;
- `interrupted_turn`: an optional `{turn_id, attempt}`.

`Store::from_file` sets `ProcessLost` when there was no takeover and either:

- at least one dangling tool call was found, or
- the session has an interrupted turn.

`flock` lets only one process hold the file, so whoever opens it can be sure
the previous writer is gone.

The forced `|_| false` predicate is removed. Replay follows the same rule on
every open (section 3). Old sessions without `otto.turn` records still recover
dangling calls, but they never get an interrupted turn.

`failover::recovery::notify` takes `Recovery` and runs for both causes:

- The header differs by cause. A takeover keeps today's text. Process loss
  says that the previous Otto process stopped before the turn finished.
- When `interrupted_turn` is set, the notice says the interrupted turn is
  being continued from the transcript. It also tells the model not to assume
  that unanswered work happened unless a result says so.
- Children with no final status are marked interrupted for both causes, so
  `agent resume` works after a plain crash.

Two existing wiring constraints stay as they are:

- Recovery runs only where an inbox exists. `--no-session` sessions, and
  sessions whose caller disables sub-agents, get no notice.
- Pushing the notice is what starts the resume. The existing wake paths
  (serve wake loop, TUI idle `run_wake`, ACP wake slot) already run one turn
  for a non-empty inbox, so no frontend needs new turn logic.

### 3. Resume bound

- **Which turn resumes.** The wake turn that delivers a process-loss notice
  writes `trigger: "recovery"` with `resumes` and `attempt` set. A takeover
  notice is treated the same way when an interrupted turn exists.
- **Bounded to one automatic resume per chain.** When the interrupted turn's
  `attempt` is already 2 or more, which means the automatic resume also died:
  - no notice is pushed and no wake runs;
  - dangling calls get the synthetic repair;
  - the frontend shows the warning `turn <id> was interrupted again after
    automatic resume; not resuming`;
  - the user's next prompt then proceeds normally.

  This prevents a crash loop on a request or tool that kills the process.
- **Prompt before wake.** If the user types a prompt before the wake turn
  claims the session, the prompt turn delivers the notice first (existing
  `deliver_notifications` order) and becomes the resumption. It records
  `trigger: "user"`, and it closes the chain.

### 4. Replay safety stored with the attempt

- **New attempt field.** `OperationFact::Attempt` gets an optional
  `retrySafety`, in the same wire encoding as `RetrySafety`.
  - It is written at attempt time from the executor's current declaration.
  - It is omitted when absent.
  - The schema version stays 1: the field is additive and is not denied as
    unknown, so older readers ignore it.
- **One shared predicate** in `otto-core::session::operation`:
  `replay_allowed(record: Option<&OperationRecord>, current: RetrySafety) -> bool`.
  It returns true only when all of these hold:
  - `current == ReadOnly`;
  - the record is absent, or it is not corrupt, has no terminal fact and
    `attempts <= 1`;
  - the recorded `retrySafety` is absent (a legacy attempt, which keeps
    today's rule) or is `ReadOnly`.

  So the current declaration can veto a stored `ReadOnly`. A tool reclassified
  to `ReadOnly` after the attempt was written is not replayed.
- **Callers and unchanged rules.** The store's `all_replayable` and the agent's
  `replay_pending_calls` both call this predicate instead of keeping two copies
  of the rule. The store's tool-name predicate becomes
  `fn(&str) -> Option<RetrySafety>`. Unchanged:
  - all-or-nothing per assistant message;
  - no replay when hooks are configured;
  - same operation id with `attempt + 1`.

### Difference from the approved designs

- **Replay spec.** "After a takeover" becomes "after any process loss". The
  attempt bound and all-or-nothing rule are unchanged.
- **Failover spec.** Recovery notices and child marking are no longer limited
  to lease-managed sessions. Workflow steps still never replay automatically.
- **Resilience spec.** "`Unknown` never causes automatic replay" was already
  qualified for `ReadOnly`. This design adds one automatic provider request
  per interrupted turn, sent only when a frontend opens the session.

## Compatibility

- **New sessions in old Otto.** `otto.turn` entries and the `retrySafety`
  field are ignored, and behavior is unchanged.
- **Old sessions in new Otto.** With no turn records, there is no interrupted
  turn. Dangling-call recovery, the notice and child marking apply. This means
  an old session that crashed with dangling calls now gets one wake turn when
  opened, which is intended.
- **Pi interop.** `scripts/pi-session-interop.test.mjs` must still pass,
  because Pi ignores custom entries.

## Ownership

| Area | Change |
| --- | --- |
| `crates/otto-core/src/session/` | `otto.turn` codec and fold; `retrySafety` field; `replay_allowed` |
| `crates/otto-core/src/agent/` | write start/end records; recovery trigger; shared predicate in `replay_pending_calls` |
| `crates/otto/src/storage/backends/posix/session/store.rs` | `Recovery`; drop takeover-only predicate; resume bound |
| `crates/otto/src/failover/recovery.rs` | cause-specific wording; child marking for both causes |
| `crates/otto/src/runtime/wiring.rs` | pass `Recovery` |
| `crates/otto/src/tool/safety.rs` | expose declared safety to the store predicate |
| `crates/otto/src/server/turn.rs` | `TRIGGER_RECOVERY` for the status stream (open question 2) |

## Acceptance

Offline, deterministic tests that fail if the behavior breaks:

- **Turn records**
  - A completed, a failed and a cancelled turn each write one start record and
    one matching end record.
  - A turn killed after its user message, with no assistant message, folds as
    an interrupted turn.
- **Plain crash (no lease)**
  - Dangling `read`: the call is replayed with `attempt 2` under the same
    operation id, a process-loss notice is delivered, and a recovery turn runs
    without user input.
  - Dangling `[read, bash]`: both get synthetic results. A notice and a
    recovery turn still run.
  - Interrupted mid-stream (user message last, open start record): a recovery
    turn resends the request, and the scripted provider sees the same
    transcript plus the notice.
  - A turn that ended `failed` or `cancelled` does not resume on open.
- **Resume bound and ordering**
  - A recovery turn that is itself interrupted (`attempt 2`) is not resumed
    again. A warning is shown and dangling calls are repaired.
  - A prompt submitted before the wake becomes the resumption and records
    `trigger: "user"`.
- **Replay safety**
  - An attempt recorded as `NonIdempotent` is not replayed after its tool
    becomes `ReadOnly`.
  - An attempt recorded as `ReadOnly` is not replayed after its tool becomes
    `NonIdempotent`.
  - A legacy attempt without `retrySafety` follows today's rule.
- **Children and compatibility**
  - Children with no final status are marked interrupted after a plain crash,
    and `agent resume` accepts them.
  - A pre-change session file opens with no interrupted turn, and its
    dangling calls still recover.
  - The session written by these tests round-trips through the Pi interop
    check.
- **Server**
  - The serve wake loop starts the recovery turn and reports its trigger.

Docs, in the same change:

- the user manual's crash recovery and "Continuing a session on another host"
  sections;
- the session record contract in `development.md`;
- a `CHANGELOG.md` entry.

## Later slices (not part of this approval)

Gaps from the Pi Durable comparison, in proposed order:

1. Durable inputs: persist steering, serve queue, `task_queue` and child
   `agent_send` in the session inbox. Add submission `requestId`
   deduplication on HTTP/ACP and notification IDs (resilience Phase 3).
2. Compaction as a resumable task with a pinned summary request.
3. Structured todo/plan state as session `custom` records, which makes it
   atomic with the journal.
4. Session fork at an entry.
5. Session-journal and mailbox conformance helpers.
6. Pre-output retry for `openai-compatible`, and the resilience spec text
   corrected to match the code.

## Open questions

1. Reclassify `agent_status`, `remind_status` and `list_models` as `ReadOnly`
   in this slice? They only read state. Recommended: yes, each with a test
   showing it has no side effects. `skill` stays `NonIdempotent` until it is
   audited.
2. Should the server report recovery turns with a new trigger `recovery`, or
   reuse `task`? Recommended: a new value, because clients can show "resumed
   after crash". Adding the value is compatible for clients that pass unknown
   triggers through.
3. Is one automatic resume per chain the right bound, or should it be
   configurable? Recommended: fixed at one with no setting, because resume
   happens only when a user opens the session.
