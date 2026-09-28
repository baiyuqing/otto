# Resilience, retry, and idempotency

Status: approved 2026-09-28. Production implementation proceeds in the
phased order below; Phase 5's concrete external capability protocol retains
its separate approval gate.

## Problem

Otto runs provider requests and tools across local processes, filesystems, MCP
servers, and remote APIs. Every boundary can fail before an operation starts,
while it is running, after its effect happened, or after its result was
committed but before the caller observed that commit. A timeout only says that
Otto stopped waiting; it does not say that the operation did not happen.

The repository already has several sound but domain-specific mechanisms:

- OpenAI-compatible completions have bounded connect/read waits and limited
  retry before any stream event is visible. ChatGPT completions do not retry.
- The agent appends an assistant tool call before it executes the tool, runs
  calls serially, and appends each result before starting the next call.
- Reopening a session represents the first unanswered call as "may have run"
  and later calls in the same assistant message as "not executed".
- Bash and MCP have call-specific timeouts and cancellation.
- The inbox is durable but at-least-once.
- Workflow recovery marks a running attempt `interrupted`, pauses the run, and
  requires an explicit retry because an external effect may already exist.
- Session failover fences writers and protects committed local workspace
  effects where the platform permits it, but deliberately does not claim to
  undo or deduplicate external effects.

These mechanisms do not yet form one contract. In particular:

1. timeout, cancellation, failure, and unknown outcome are not represented
   consistently;
2. connect/read timeouts do not give turns, tasks, or workflow steps an
   end-to-end deadline;
3. retry policy is embedded in individual transports instead of being derived
   from both failure certainty and operation safety;
4. notifications and inbound events can be delivered more than once;
5. external systems are not given a stable logical operation identity even
   when they support idempotency;
6. recovery can show an interrupted unit, but cannot generally reconcile it
   with the state of the external system.

## Goals

Build a coherent resilience contract in six implemented phases:

1. stable operation identity and structured outcome certainty;
2. inherited deadlines and bounded cancellation cleanup;
3. durable notification and inbound-event deduplication;
4. explicit retry contracts and conditional idempotency for built-in tools;
5. opt-in provider and MCP idempotency capabilities;
6. sub-agent and workflow reconciliation built on the preceding contracts.

The final rule is:

> Retry automatically only when Otto can prove that the operation did not
> start, or when the operation's contract makes repeating the same logical
> operation safe. Otherwise preserve the uncertainty and require an explicit
> decision.

The target is effectively-once behavior where Otto or a cooperating external
system owns durable deduplication. Otto does not claim general exactly-once
execution.

## Non-goals

- Making arbitrary Bash commands idempotent.
- Undoing partial filesystem, process, MCP, or remote API effects.
- Resuming an in-flight provider stream or tool process after process loss.
- Inferring success from stdout, response fragments, timing, or error prose.
- Treating a client-generated ID as proof that a server deduplicates it.
- Automatically retrying interrupted workflow steps or unknown non-idempotent
  calls.
- Introducing another provider or changing the supported platform claims.
- Persisting prompts, tool arguments, tool output, or response text in metrics
  or the general operation-fact stream. The inbound outbox is the one scoped
  exception: it must durably hold the bounded notification payload needed to
  recover delivery, with the same sensitive-data permissions as session
  inboxes.
- Replacing the append-only Pi v3 session, workflow SQLite source of truth, or
  current session lease protocol.

## Terminology and invariants

### Logical operation and attempt

A **logical operation** is the user- or agent-intended action whose duplicate
execution matters. An **attempt** is one transport or executor invocation made
for that logical operation.

A retry creates another attempt under the same operation ID. An explicit new
action creates a new operation ID, even when its arguments equal an earlier
operation. Attempt number is diagnostic metadata; it is not part of the
idempotency key.

Examples:

- one model completion step is one logical provider operation and may make
  several HTTP attempts;
- one assistant tool-call block is one logical tool operation;
- one MCP tool operation may make an authentication refresh attempt without
  becoming a new logical operation;
- a workflow retry is a new workflow attempt, but reconciliation may reuse the
  interrupted tool operation ID when it is querying or completing that same
  logical effect.

### Disposition and effect certainty

A single success/failure enum cannot describe resilience correctly. The shared,
provider-neutral contract therefore has two orthogonal dimensions in
`otto-core::model`:

```rust
pub enum OperationDisposition {
    Succeeded,
    Error,
    Cancelled,
    DeadlineExceeded,
    Interrupted,
}

pub enum EffectCertainty {
    NotStarted,
    KnownNoEffect,
    Completed,
    Unknown,
}
```

`OperationDisposition` describes what the caller observed. `EffectCertainty`
describes what Otto can prove about externally visible effects:

- `NotStarted`: execution was refused before crossing the effectful boundary.
  Examples are invalid arguments, a failed call guard, and cancellation
  observed before dispatch.
- `KnownNoEffect`: dispatch occurred, but the concrete protocol definitively
  established that no effect was applied. This is rare and must be guaranteed
  by the implementation contract, not inferred from generic transport text.
- `Completed`: the executor produced a definitive terminal result under its
  contract. It can pair with `Error`; for example, a remote business rejection
  can be a known completed result without being a successful business action.
- `Unknown`: execution may have happened fully, partly, or not at all. Timeout
  after dispatch, lost transport, forced cleanup, and process loss with an
  unanswered tool call normally produce this state.

A separate reason records why execution stopped when that fact matters across
an API boundary:

```rust
pub enum OperationStopReason {
    UserCancellation,
    Deadline,
    Shutdown,
    Migration,
    TransportLost,
    ProcessLost,
}
```

Cancellation and timeout are requests or observed dispositions, not proof of
no effect. Pre-dispatch cancellation is
`Cancelled + NotStarted + UserCancellation`; post-dispatch cancellation is
normally `Cancelled + Unknown + UserCancellation` unless the concrete executor
proves a stronger certainty.

### Retry safety

Retry safety is a property of an operation implementation, not model-supplied
arguments:

```rust
pub enum RetrySafety {
    ReadOnly,
    Idempotent,
    IdempotentWithKey,
    NonIdempotent,
}
```

- `ReadOnly`: repeating has no externally observable mutation under the
  documented trust boundary.
- `Idempotent`: repeating with the same validated inputs converges to the same
  state without a server-side operation key.
- `IdempotentWithKey`: repeating is safe only when every attempt uses the same
  operation ID and the receiver has declared and implemented durable
  deduplication.
- `NonIdempotent`: Otto cannot establish safe repetition.

The registry owns built-in tool safety. An MCP capability can narrow a remote
call from `NonIdempotent` to `IdempotentWithKey`; model output can never widen
it. Configuration may disable retry or reduce attempt limits, but cannot label
an implementation idempotent unless its concrete adapter supports the
corresponding contract.

### Automatic retry decision

An automatic retry is allowed only if all conditions hold:

1. the parent cancellation token is not cancelled;
2. the operation deadline has enough remaining budget for backoff and another
   attempt;
3. the attempt budget is not exhausted;
4. no externally visible partial stream has been emitted;
5. either:
   - certainty is `NotStarted` or `KnownNoEffect`, or
   - safety is `ReadOnly` or `Idempotent`, or
   - safety is `IdempotentWithKey` and the receiver's capability was
     successfully negotiated;
6. the disposition/failure class is declared transient by that implementation.

`Unknown + NonIdempotent` is never retried automatically. `Error` is not by
itself retryable: a definitive business rejection remains terminal.

## Ownership and architecture

### `otto-core`

The wasm-safe crate owns only neutral, clock-independent contracts:

- operation ID validation and owned value type;
- disposition, effect-certainty, stop-reason, and retry-safety enums;
- typed operation facts and pure append-only state folding;
- agent-loop propagation and events;
- Pi codec support for Otto operation custom entries;
- deadline values represented as remaining `Duration`, never wall-clock access.

It does not own HTTP retry policy, Tokio timers, process termination, SQLite,
or external capability discovery.

### `crates/otto`

The native crate owns:

- monotonic clocks and deadline calculation;
- HTTP attempt classification and backoff;
- Bash/process cleanup;
- MCP cancellation and capability negotiation;
- built-in tool safety declarations;
- inbox and inbound durable deduplication;
- operation-aware task and workflow recovery;
- content-free metrics.

Concrete policies stay with their domain. There is no global retry executor
that can accidentally replay an arbitrary tool.

### Composition and frontends

The composition root resolves timeout policy once and injects it into provider,
agent, MCP, sub-agent, and workflow construction. Frontends render typed state
and never parse error strings to decide whether retry is safe. HTTP DTO changes
remain separate from internal structs and update the OpenAPI fixture.

## Identity model

### Format

`OperationId` is an opaque, validated ASCII identifier, 1 to 128 bytes. New
IDs use the existing native ID generator with an `op_` prefix. Consumers must
compare the complete string and must not parse embedded meaning from it.

IDs are not secrets, but logs and metrics store only the ID or a one-way
bounded label, never operation arguments.

### Where identity is durable

Identity durability follows the owning source of truth rather than introducing
one global operation database:

| Operation | Identity source | Persistence |
| --- | --- | --- |
| Provider step | generated before dispatch | runtime events and existing usage records; not resumed after crash |
| Tool call | generated after assistant intent commit and before dispatch | `otto.operation` custom facts linked to the session-unique tool-call ID |
| Notification | producer-supplied stable ID | inbox sidecar and context-message metadata |
| Inbound delivery | upstream event ID plus destination | inbound SQLite ledger and notification ID |
| Sub-agent | existing task ID plus attempt ID | task record and child custom entries |
| Workflow | existing run/step/attempt plus operation references | workflow SQLite and attempt transcript |

A provider call is not resumed after process loss, so its ID need not create a
new durable intent log. Its retries within one live call reuse the same ID.
Tool calls require durable identity because their intent already lives in the
session and recovery must describe it.

### Tool-operation assignment

After a provider response validates, the assistant message containing its
provider-supplied tool-call IDs is durably appended unchanged. `Session`
already rejects reuse of a tool-call ID within one session, so the durable
anchor is `(session identity, tool-call ID)`; the transient assistant
`Message.id` is deliberately not used because the file store replaces it with
the Pi entry ID during encoding. Immediately before each call is dispatched,
the agent obtains one operation ID and appends a `created`/`attempt` operation
fact linked to the tool-call ID and tool name. Provider-supplied tool-call IDs
continue to pair calls with model-visible results and become globally useful
only together with the owning session identity.

The operation fact append must succeed before dispatch. The eventual terminal
fact repeats the operation ID. Session validation rejects contradictory links
or terminal transitions when folding operation history. The model-visible tool
result preserves the existing call ID/name pairing and human-readable safety
text; code derives disposition and certainty from operation facts, not that
text.

Old histories without operation facts remain valid. A missing fact is never used
as proof that a call did not start: it can mean a legacy executor ran the call,
or a new executor stopped between assistant append and operation-fact append.
On reopen, the first unanswered call without a terminal fact therefore keeps
`Unknown` certainty; later unanswered calls have `NotStarted` certainty because
tool calls execute serially. Otto may create a new ID only for an explicit new
execution decision by the owning recovery flow. It must not pretend that the
new ID identifies the old unknown external effect.

### Tool operation commit order

Bare custom entries are outside `Session`'s message/context path, so appending
one between an assistant call and its tool result does not violate the rule that
no other *message* may intervene. The durable order for each serial call is:

1. assistant message with the tool-call intent;
2. `created`/first `attempt` operation fact;
3. external dispatch, concrete tool settlement, `CallGuard::after`, any
   failover durability fence, redaction, and final persisted-result selection;
4. terminal operation fact with the resulting disposition and effect
   certainty;
5. model-visible tool result.

Every append is complete before the next step. If step 2 fails, dispatch does
not occur. A `CallGuard::before` refusal settles as `NotStarted`. A loss after
dispatch and before step 4 remains `Unknown`. An `after`/durability-fence
failure cannot be `KnownNoEffect`: execution already occurred, and its concrete
contract decides between `Completed` with an error disposition and `Unknown`.
The terminal fact is written only after this complete registry boundary, never
when `Tool::execute` alone returns.

A loss after step 4 and before step 5 must be recovered from the terminal fact
and must not re-execute the operation. Recovery appends an error tool result
whose fixed text says the operation settled but its original model-visible
result is unavailable; it includes the terminal disposition/certainty in typed
frontend metadata. This synthetic result is always `is_error = true`, even
when the lost original disposition was success, so the model cannot mistake
missing output for successful usable output. After that result commits, serial
execution may continue with later calls that are still proven `NotStarted`.
Session-open repair folds custom facts before it repairs pending calls. A
legacy synthetic missing result already present is authoritative history and
is not replaced or reinterpreted as an operation-fact conflict.

## Pi v3 compatibility

Pi remains version 3 and append-only. Operation lifecycle is stored as bare Pi
`custom` entries, which already stay out of model context. One namespaced type
keeps the extension bounded:

```json
{
  "type": "custom",
  "customType": "otto.operation",
  "data": {
    "schemaVersion": 1,
    "operationId": "op_...",
    "event": "attempt",
    "attempt": 1,
    "kind": "tool_call",
    "toolCallId": "...",
    "toolName": "..."
  }
}
```

Terminal facts add disposition, effect certainty, and optional stop reason.
Exact wire names and bounds are fixed by codec tests before implementation.
Facts contain identities and status only. They do not contain prompt text,
tool arguments, full tool output, credentials, or raw external errors.

Rules:

- operation facts append; they never update or replace prior facts;
- facts for one operation are folded in file order by a pure `otto-core`
  reducer;
- a terminal fact is final; a conflicting later terminal fact produces a
  warning/corrupt state and never silently overwrites the first;
- the operation link is the tuple of session identity, tool-call ID, and
  operation ID; reused or contradictory links are rejected;
- missing facts decode as legacy semantics;
- unknown custom fields and schema versions follow the existing compatibility
  behavior and never enter model context;
- old records are never rewritten and old histories are not assigned invented
  operation IDs;
- compaction may omit old operation facts from provider context because custom
  entries never enter it, but session history and recovery retain the facts;
- open-time synthetic missing results fold to
  `Interrupted + Unknown + ProcessLost` for the first unanswered call and
  `Interrupted + NotStarted + ProcessLost` for later calls;
- current human-readable recovery text remains so older frontends and models
  receive the safety warning, while new code uses typed folded state.

An older Otto version ignores the custom entries and still sees the existing
valid Pi call/result sequence and recovery text. New Otto reads every old
session. If a terminal fact is durable but the model-visible tool result is
missing, recovery must not re-execute the operation. It creates a conservative
synthetic result from the typed terminal state. Recovering the original result
would require a separately designed, redacted, size-bounded persisted result;
this design does not store tool output in operation facts.

## Deadline model

### Absolute monotonic deadlines

A native `Deadline` is an absolute monotonic instant. Configuration durations
are converted once at operation admission. Children inherit the earlier of
their configured deadline and the parent's deadline:

```text
workflow step
└── child task / turn
    ├── provider logical operation
    │   └── HTTP attempts and retry sleeps
    └── tool operation
        ├── Bash process
        └── MCP request
```

Retries consume the same budget. They do not reset the deadline.

`otto-core` receives a cancellation token plus an injected remaining-budget
value or deadline capability; it does not call a native clock. The final API
shape should keep wall-clock and Tokio types out of `otto-core` and preserve
wasm tests.

### Configuration

Durations use the existing Go-duration parser where the surrounding config
already does. Initial defaults are chosen only after measuring current real
turn/task durations; introducing the keys must not silently shorten currently
valid long-running work. The resolved structure supports:

- turn timeout;
- provider logical-operation timeout;
- cancellation grace;
- sub-agent task timeout;
- workflow step timeout;
- retry maximum attempts, base/max backoff, and `Retry-After` cap;
- MCP's existing connect/call timeouts plus cancellation grace.

Zero and overflow are rejected. A child timeout cannot extend a parent
budget. CLI overrides, if added, follow existing config precedence.

### Deadline and completion race

Every boundary uses one deterministic rule:

1. check parent cancellation before dispatch;
2. after dispatch, race completion, parent cancellation, and deadline;
3. if completion and stop become ready together, a completed, validated result
   already obtained by the owner wins;
4. otherwise mark stopping, cancel the child token once, and begin cleanup;
5. race cleanup completion against cancellation grace;
6. if cleanup returns a definitive executor result, classify it by that
   executor contract;
7. if grace expires, detach only when resource ownership makes detachment safe;
   otherwise forcibly close/kill the owned resource and return `Unknown`;
8. late completion cannot overwrite the terminal state.

The state transition is guarded by the existing operation owner, not frontend
state. Tests use paused time and barriers to cover both sides of every race.

### Deadline strength by execution boundary

Deadlines bound how long an owner waits only where execution is cooperatively
cancellable or backed by a resource Otto can close. They are not a universal
preemption primitive:

- provider HTTP and HTTP MCP: the owner stops waiting after deadline plus
  cleanup grace; remote effect certainty remains `Unknown` after dispatch;
- Bash and owned stdio MCP processes: Otto cancels, then closes/kills the owned
  process or connection after grace;
- in-process synchronous filesystem and SQLite tools: the deadline is checked
  before dispatch and after return, but a poll blocked in a syscall cannot be
  preempted. Otto does not detach an effectful blocking closure that could
  mutate state after its session owner moved on;
- a turn, sub-agent, or workflow step containing such synchronous work inherits
  that limitation and may exceed its nominal deadline until the active call
  returns.

A hard wall-clock bound for synchronous effectful tools would require a
separate killable worker-process and commit protocol and is outside this
design. User-facing documentation must distinguish cooperative deadlines from
hard process time limits.

### Resource-specific cleanup

- Provider HTTP: drop the response/request future after grace. Since dispatch
  may have reached the provider, timeout after dispatch is `Unknown`; no claim
  is made that billing stopped.
- Bash: cancel, terminate the complete process group through the sandbox
  executor, wait grace, then force kill. A post-start timeout is `Unknown`
  even when the process was killed, because prior effects remain.
- stdio MCP: send protocol cancellation and wait grace. If it still has not
  settled, the per-server connection owner enters a single server-wide
  `stopping` state, rejects new calls, marks every other dispatched pending
  call `Interrupted + Unknown`, closes/terminates the server exactly once, and
  leaves it disconnected. One timed-out call never independently kills a
  shared transport behind the connection owner.
- HTTP MCP: cancel and drop the local request after grace; the remote outcome
  is `Unknown`.
- Sub-agent/workflow: stop scheduling new calls, let the active child boundary
  perform cleanup, then durably mark the task/attempt interrupted.

No detached task may retain a session writer, lease, inbox claim, child
process, or mutable workflow handle.

## Retry and backoff

The native retry helper owns arithmetic only: attempt budget, remaining
budget, exponential backoff, bounded random jitter, and a capped
`Retry-After`. The provider or tool adapter remains responsible for declaring
a failure transient and for proving retry safety.

Backoff rules:

- exponential growth saturates at configured maximum;
- full jitter is sampled from a bounded range using an injected source so tests
  remain deterministic;
- `Retry-After` is parsed as today, capped, and also limited by remaining
  operation budget;
- a delay that leaves no meaningful execution budget terminates immediately;
- every planned retry emits the existing provider-retry event extended with
  operation ID and attempt metadata;
- credentials and response bodies never enter retry events.

OpenAI-compatible's current pre-output transport retry is a deliberate legacy
availability policy that can duplicate provider work or billing and therefore
does not satisfy the final automatic-retry invariant for an unknown,
non-idempotent operation. Phase 1 makes those attempts visible without
expanding them. Phase 2 removes automatic retries after a request may have
been dispatched unless a concrete trusted adapter proves `KnownNoEffect` or
Phase 5 supplies a verified idempotency capability. Failures known before
request dispatch may still retry within budget. The visible-stream rule remains
an additional prohibition: no completion retries after any reasoning, text, or
tool-call delta. ChatGPT remains no-retry unless its owned backend contract is
separately proven and tested.

## Phase 1: identity, disposition, and effect-certainty contract

### Scope

1. Add neutral operation identity, disposition, effect-certainty, stop-reason,
   and fact-folding types to `otto-core::model` and `otto-core::session`.
2. Append a durable operation fact after the assistant tool-call intent and
   before dispatch; propagate the identity through execution and events.
3. Encode versioned `otto.operation` custom facts and preserve old-history
   behavior.
4. Classify registry preflight refusal as `NotStarted`, definitive returned
   results as `Completed`, and unrecoverable post-dispatch loss as `Unknown`,
   independently of success/error disposition.
5. Replace recovery decisions based on missing-result prose with typed folded
   facts while preserving the prose.
6. Add wire fields and frontend rendering for disposition and effect
   uncertainty.
7. Add content-free counters for attempts and unknown effects only after the
   event contract exists.

Phase 1 does not add new retries. It makes current behavior explicit.

### Presentation

Frontends distinguish at least:

- `did not run`;
- `completed`;
- `failed with a known result`;
- `outcome unknown — check effects before retrying`.

The last form is visually distinct and cannot be downgraded by model-authored
text. Headless errors use stable wording and a non-secret operation ID.

### Acceptance

- operation ID uniqueness and validation tests;
- call/result ID and operation-ID sequencing tests;
- old Pi fixtures decode unchanged;
- operation facts round-trip, remain outside model context, and are ignored
  safely by older Pi-compatible readers;
- fact folding rejects contradictory links and reports conflicting terminal
  facts;
- crash recovery classifies the first unanswered call `Unknown` and later
  calls `NotStarted`;
- no frontend parses error prose to derive certainty;
- wasm architecture gate remains green.

## Phase 2: deadlines and bounded cleanup

### Scope

1. Add resolved timeout/retry-budget configuration with backwards-compatible
   defaults.
2. Thread parent deadlines through controller, agent, provider, tool, child,
   and workflow composition boundaries.
3. Convert OpenAI-compatible retry to consume one logical deadline, cap
   `Retry-After`, add deterministic jitter, and stop generic post-dispatch
   unknown retries; preserve only proven pre-dispatch or idempotent retries.
4. Add cancellation grace and forced cleanup for Bash and MCP.
5. Add task and workflow-step deadlines; preserve explicit workflow retry.
6. Record typed timeout/cancellation reasons and attempt counts.

### Acceptance

- cooperative network/process operations return control after deadline plus
  documented grace;
- synchronous in-process filesystem/SQLite calls check the deadline before and
  after execution but are explicitly not claimed to have a hard upper bound;
- task/workflow deadlines stop admission of later work while waiting for an
  active synchronous call to settle;
- retries never reset budget;
- parent cancellation wins before dispatch;
- simultaneous completion/timeout follows the fixed race rule;
- grace expiry produces `Unknown` after dispatch;
- process/server cleanup leaves no owned child running;
- tests use paused time and no network.

## Phase 3: notification and inbound deduplication

### Notification identity

Every inbox entry gains a producer-stable `notification_id`. Producers derive
it from their durable source:

- reminder: session ID plus reminder ID and firing generation;
- sub-agent completion/report: parent session, task ID, and report sequence or
  terminal generation;
- failover recovery: session ID and takeover epoch;
- inbound message: source, upstream event ID, and destination session;
- callers without a durable source use a generated ID before the first
  sidecar write.

The ID is stored in the sidecar and in typed context metadata. Existing
sidecars without IDs are loaded with generated IDs and retain at-least-once
legacy behavior for that first recovery.

### Inbox commit protocol

Delivery remains append-first, remove-second, but reopen reconciles the gap:

1. snapshot a queued entry and its stable notification ID;
2. call a new wasm-safe `Session::append_context_if_absent` primitive with
   the notification ID and context message;
3. each implementation checks and appends under its own single lock:
   `Store` persists and fsyncs before returning `Appended`, while
   `MemorySession` provides process-lifetime deduplication;
4. `AlreadyPresent` is the same committed delivery outcome as `Appended`;
5. remove that exact sequence/ID from the inbox and atomically persist the
   sidecar.

The session is the delivery source of truth; the sidecar is the pending queue.
There is deliberately no claimed ACID transaction across SQLite, sidecar, and
session JSONL. Stable identity plus append-if-absent and reconciliation close
the duplicate window. An append-before-remove crash therefore deduplicates on
reopen.

A sidecar persistence failure is no longer silently treated as durable
success. Push/remove APIs return or record the failure through the owning
controller, keep recoverable queue state where possible, and emit an
operator-visible warning. The exact error propagation must preserve existing
lock ordering and avoid calling frontends under the inbox lock.

Duplicate IDs with different normalized payload hashes are corruption and fail
closed with a warning; content itself is not put in a metrics store.

`MemorySession` deduplicates for the process lifetime only. `--no-session`
cannot promise crash-durable delivery.

### Inbound ledger

`crates/otto::inbound` owns a local SQLite source-event/per-target outbox
ledger keyed by:

```text
(source, workspace_id, upstream_event_id, destination_session_id)
```

Per-destination identity preserves intentional fan-out. The set of destination
sessions is snapshotted when the validated source event is accepted; recovery
does not re-enumerate sessions and accidentally deliver old events to newly
opened sessions. Archiving a destination resolves its pending rows according
to an explicit terminal `destination_gone` state rather than retargeting them.
Only the current session lease holder may complete a session append.

The outbox stores the normalized, versioned, size-bounded notification payload
needed for recovery, its payload hash, notification ID, state, and timestamps.
It has the same `0600` database and sensitive-data treatment as session
sidecars: payload is never copied into metrics/events and retention removes it
only after delivery reconciliation. States are `claimed`, `enqueued`, and
`destination_gone`; delivery remains owned by the session inbox and is
reconciled through notification ID.

The event-receive transaction inserts the unique source event and all
per-target delivery rows together. Dispatch happens after commit:

1. validate and normalize a true upstream stable ID;
2. snapshot destinations and transactionally insert/read the source event and
   all destination claims;
3. enqueue each delivery with its precommitted notification ID and durably
   persist the inbox entry;
4. mark that destination claim `enqueued`;
5. on recovery, an old `claimed` row with no matching inbox/session ID is
   re-enqueued from its durable outbox payload with the same notification ID;
6. an `enqueued` row is never itself evidence of model delivery; session
   metadata is.

An event without a trustworthy upstream ID is explicitly non-deduplicable (or
rejected by the source adapter); Otto does not hash content/sender/time and
pretend that two legitimate identical messages are one event.

Same key and same payload hash is an idempotent duplicate. Same key and a
different hash is a conflict and is not delivered silently. Retention removes
only rows older than a configured bound after their notification is visible in
the session or their destination no longer exists. Size and TTL have explicit
limits.

### Acceptance

Fault injection covers every boundary:

- before/after sidecar replacement;
- before/after session append;
- after append and before sidecar removal;
- after source-event/per-target transaction commit and before inbox
  persistence;
- after inbox persistence and before the `enqueued` state-update commit;
- after `enqueued` commit and before session append;
- repeated identical event and conflicting reused event ID.

A file-backed session observes one context message per notification ID across
restart. No claim is made for legacy entries without IDs or `--no-session`.

## Phase 4: built-in tool retry contracts

### Registry contract

`Tool` exposes internal execution metadata separately from its provider-visible
schema. The registry validates a conservative base `RetrySafety` supplied by
concrete code. A tool may return a narrower, code-derived per-call safety only
after validating the exact call shape and observed preconditions; model text or
an unvalidated argument value never widens safety. The registry is the only
place that may initiate a built-in tool retry, and it records every attempt
under the same operation ID.

Initial classification is conservative:

| Tool | Safety | Automatic retry |
| --- | --- | --- |
| `read`, `ls`, `grep`, `find`, `memory_search`, status queries | `ReadOnly` | only transient pre-result failures |
| `write` without a validated expected-state precondition | `NonIdempotent` | never after unknown outcome |
| `write` with validated CAS/already-applied state | per-call `Idempotent` | bounded |
| `edit` | conditional idempotency | only when already-applied is proven |
| `remember`, `remind` | `IdempotentWithKey` after durable dedup exists | bounded |
| `forget` by stable record ID | `Idempotent` | bounded |
| `bash` | `NonIdempotent` | never |
| MCP router | `NonIdempotent` by default | never without Phase 5 capability |
| agent/workflow control | domain-owned | no generic registry retry |

Read-only does not mean unbounded retry: failures must still be transient and
consume the deadline and attempt budget.

### `write` compare-and-swap

The write contract gains an explicit expected-state precondition for the call
shape that is eligible for automatic retry. Ordinary replacement without that
precondition remains `NonIdempotent`. The exact provider-visible field receives
a focused tool-schema review, but the behavior is fixed:

- destination already equals target content: idempotent success;
- destination digest equals expected digest: atomically replace and fsync as
  today;
- destination matches neither: conflict, no write;
- absent/present expectations are represented explicitly, not overloaded onto
  an empty digest.

Retries re-read the destination and revalidate the same expected/target state.
They never assume the first rename failed. Only concrete code derives the
per-call `Idempotent` safety after validating this contract; merely supplying a
string that looks like a digest cannot override the tool's base policy.

### `edit`

Existing unique-old-text matching remains the primary precondition. An
already-applied success is allowed only when the full original edit set and
resulting file state prove that the exact transformation happened. Ambiguous
occurrences, overlapping edits, or unrelated concurrent changes return a
conflict. Fuzzy matching remains a decode aid, not proof of idempotency.

### `remember` and `remind`

Their owning stores persist an operation-key uniqueness constraint and a
canonical non-secret argument hash:

- same key and same hash returns the original record/timer;
- same key and different hash is a conflict;
- the dedup record is committed atomically with creation;
- retention cannot expire while a retry from any supported task/workflow
  lifetime can still arrive.

### `bash`

Bash never retries automatically. Its operation ID is for audit and recovery
only. Every post-start timeout, forced cancellation, or process loss is
`Unknown`, regardless of exit-signal observation, because prior effects may
remain.

### Acceptance

- every registered tool has a code-owned safety declaration;
- registry construction rejects missing or inconsistent declarations;
- model arguments cannot change safety;
- CAS and conflict races have adjacent tests;
- keyed stores reject same-ID/different-input reuse;
- no path automatically retries Bash or default MCP;
- architecture contract test scans new tools for an explicit safety choice.

## Phase 5: provider and MCP capabilities

### Capability principle

Sending an operation ID does not establish idempotency. Phase 5 introduces no
user-configurable switch that can promote an arbitrary endpoint or server. An
adapter may use `IdempotentWithKey` only for a concrete protocol extension that
Otto code owns by exact name/version, validates during connection, and covers
with offline contract tests. A generic OpenAI-compatible endpoint or arbitrary
MCP server remains non-idempotent.

### OpenAI-compatible provider

Generic OpenAI-compatible APIs define no portable durable idempotency
capability, so profile configuration cannot assert one and arbitrary headers do
not change retry safety. Phase 5 supports provider idempotency only after Otto
adds a named concrete adapter contract specifying all of:

- exact header and operation-ID encoding;
- canonical request identity and same-ID/different-request conflict behavior;
- complete replay/stream semantics;
- minimum dedup retention greater than Otto's retry window;
- same-origin and downgrade behavior;
- offline fixtures proving duplicate attempts return one logical result.

Until such an implemented contract exists, generic post-dispatch unknown
retries remain disabled by Phase 2. Metrics distinguish logical operations from
HTTP attempts, and all replayed responses still obey stream validation and
secret redaction.

ChatGPT behavior remains unchanged unless its owned backend contract provides
an equivalent supported mechanism. This design does not infer one.

### MCP

Modern MCP `_meta` carrying an operation ID is only transport metadata and
does not change safety. Phase 5 requires one exact Otto-supported extension,
versioned in code and negotiated from structured discovery data. Before any
implementation is approved, its contract fixes:

- extension namespace and version plus the discovery response schema;
- canonical method/tool/argument identity;
- same-ID/same-input replay and same-ID/different-input conflict;
- restart-durable retention and server-identity binding;
- cancellation semantics;
- exact status method and `absent | running | completed | unknown` schema;
- malformed, downgrade, reconnect, and retention-expiry behavior.

Server descriptions, tool annotations, modern era, JSON-RPC request IDs, or
arbitrary configured `_meta` are never capability proof. Without the exact
extension, calls remain `NonIdempotent` and single-attempt.

If the extension supports status lookup, reconciliation asks by operation ID.
Only `completed` supplies a reusable validated result. `Absent` permits
execution only when that versioned extension guarantees it means the operation
never started. `Unknown` never causes automatic replay.

### Acceptance

- capability absent, malformed, or downgraded means no automatic retry;
- same ID is reused across authentication refresh and transient attempts;
- same-ID/different-input conflicts are surfaced;
- status reconciliation precedes replay;
- offline fake servers cover restart retention and conflict behavior;
- docs name only capabilities Otto actually implements and tests, not a list
  of hypothetical providers or servers.

## Phase 6: sub-agent and workflow reconciliation

### Sub-agents

Task identity remains stable. A task gains numbered execution attempts and a
terminal summary of its last operation certainty. `resume` and restart are
separate concepts:

- `resume` continues the interrupted task context and first presents unknown
  operations to the model; it does not silently replay them;
- a new attempt is explicit and keeps the old append-only transcript;
- a keyed operation may be reconciled with its owner before execution;
- a non-idempotent unknown operation remains blocked from automatic replay;
- task deadline and cleanup use Phase 2.

Task custom entries record IDs, attempt links, and typed status but remain
outside model context. Failures to append terminal task metadata must remain
observable instead of being silently discarded where correctness depends on
it.

### Workflows

The existing rule remains: a running attempt found after restart becomes
`interrupted`, the run becomes `paused`, and no automatic retry occurs.

A new inspection use case, shared by CLI and server, reports:

- interrupted attempt and transcript;
- last committed operation;
- every operation with unknown outcome;
- operation ID and code-owned retry safety;
- whether status reconciliation is available and its result;
- local file precondition/CAS state where applicable;
- what a new workflow attempt would execute again.

Inspection contains no secret arguments in list views. Authorized detailed
views reuse existing transcript access controls.

`resume --retry <step>` continues to create a new numbered workflow attempt.
Before scheduling it, the controller:

1. reconciles operations whose concrete adapter supports query;
2. recognizes already-applied local conditional operations;
3. requires explicit acknowledgement for remaining unknown non-idempotent
   effects;
4. records the source attempt and acknowledgement transactionally;
5. never rewrites the old attempt or claims to roll back its effects.

An optional workflow recovery verifier, if later added, is a named read-only
agent step with a snapshotted definition. It may collect evidence but cannot
turn uncertainty into certainty without a concrete operation contract.

### Acceptance

- process loss never automatically replays an interrupted step;
- inspection is deterministic from durable state;
- reconciliation results are durable and auditable;
- explicit retry preserves old transcript and links the new attempt;
- approval idempotency remains unchanged;
- fork continues from a committed workflow boundary and does not claim to
  clone or roll back external state.

## Observability

Existing usage storage remains content-free. Resilience metrics/events may
contain:

- operation ID or a bounded hash;
- operation kind;
- logical operation count;
- attempt count;
- retry reason category;
- timeout/cancellation/unknown counters;
- dedup hit and idempotency conflict counts;
- reconciliation state;
- provider/model identity where already permitted.

They never contain prompts, tool arguments, tool output, response text,
credentials, configured secret headers, or raw external error bodies.

Every retry is observable before its backoff. An `Unknown` transition is
durably observable only when its owning durable session/task/workflow record
successfully commits. Provider operations without a durable owner are
observable during live terminal handling and in content-free usage records
when those writes complete; process loss can occur before such a record and is
not falsely claimed to be recoverable.

## Failure-injection strategy

Each phase adds deterministic seams at these boundaries:

1. before intent persistence;
2. after intent persistence;
3. before dispatch;
4. after dispatch and before response;
5. after external effect and before executor return;
6. after executor return and before durable result append;
7. after result append and before acknowledgement or queue removal;
8. during cancellation;
9. after grace expiry;
10. during restart reconciliation.

Tests assert both state and prohibited behavior: attempt count, no duplicate
external effect, no later tool start, preserved transcript, and no secret
content. Default tests remain offline and deterministic. Cross-process tests
use local fixture binaries and temporary directories. Platform-specific
process and durability claims remain behind `cfg` and their existing gates.

## Delivery plan

The design is implemented as reviewable slices in dependency order. Each slice
uses its own feature worktree and branch after this design is approved.

1. **Operation identity, disposition, and effect certainty**
   - neutral types, Pi `otto.operation` custom facts, agent/tool propagation,
     recovery, wire/UI.
2. **Deadline and cancellation hierarchy**
   - configuration, inherited budgets, bounded cleanup, retry arithmetic.
3. **Inbox and inbound deduplication**
   - notification IDs, session reconciliation, inbound ledger.
4. **Built-in tool retry contracts**
   - safety declarations, CAS, keyed local operations, architecture guard.
5. **Provider and MCP idempotency capability design gate**
   - first approve one exact trusted adapter/extension protocol; only then
     implement operation-key propagation and status reconciliation.
6. **Sub-agent and workflow reconciliation**
   - attempts, inspection, explicit acknowledgement and retry integration.
7. **Consolidation**
   - full fault-injection acceptance matrix, canonical user documentation, and
     removal of superseded design wording.

Every slice updates code, schemas, tests, and the canonical current-behavior
documentation together. Future-phase behavior is not added to the README or
user manual before implementation. Focused tests run first, followed by
`make check-fast` and the relevant full gate.

## Open decisions and later design gates

The following Phase 1–3 implementation-shape decisions need focused
prototypes, but do not change this document's behavior:

1. the exact `otto.operation` fact field names, bounds, and fold warning
   surface;
2. which existing native ID generator supplies operation IDs while preserving
   deterministic test injection;
3. the first timeout defaults, based on measurements rather than arbitrary
   values;
4. whether inbound dedup extends an existing native SQLite database or owns a
   small inbound database, while preserving package ownership and backup
   expectations;
5. how the inbox persistence hook reports failure without violating its current
   under-lock persistence and notification ordering contract.

Phase 5 has a separate behavioral design gate, not an implementation detail.
Before production code for provider or MCP keyed idempotency, the exact trusted
adapter/extension schema and semantics listed in Phase 5 require explicit
approval. If no concrete supported endpoint/server contract is available,
Phase 5 completes by retaining `NonIdempotent` behavior and documenting that no
external idempotency capability is implemented; Otto does not invent one to
satisfy the roadmap.
