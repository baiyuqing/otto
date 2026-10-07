# Durable inputs

Status: approved 2026-10-06 with both contract changes below; open questions
resolved as recommended. Sections 1 to 4, 6 and 7 ship together; section 5
(model follow-ups) ships in a separate PR under this design. See
[Implementation decisions](#implementation-decisions) for the details the
implementation settled.

It is the next slice after
[crash recovery and interrupted-turn resume](2026-10-06-crash-recovery-and-turn-resume.md).
It implements Phase 3 of
[resilience, retry, and idempotency](2026-09-28-resilience-retry-idempotency.md)
and amends [active-task guidance](2026-10-04-active-task-guidance.md).

The comparison point is again Pi Durable (`earendil-works/pi`,
`packages/durable`):

- **Submissions.** Every submission is a durable record, deduplicated per
  conversation by `requestId`.
- **Queued input.** Queued steer, follow-up and write items live in a durable
  inbox document and are placed at defined run boundaries.
- **A failed or aborted run** leaves its queued items for the next submission.

Otto should get the same guarantee, that an input it accepted is neither
lost nor delivered twice, without copying Pi's storage model.

## Problem

Facts from the code at `203e5da`. Paths are relative to `crates/`.

### `Mailbox` and `Inbox` are one queue at two layers

- **`Mailbox`** (`otto-core/src/mailbox.rs`, added in #372) is the port:
  - first-in, first-out;
  - a `queued()` snapshot;
  - `ack(seq)` only after the durable effect, which gives at-least-once
    delivery;
  - `load()` for durable adapters.
- **`Inbox`** (`otto-core/src/agent/inbox.rs`) is its only implementation:
  - an in-memory queue;
  - a `PersistHook` that runs under the lock on every change;
  - `impl Mailbox for Inbox` and `type InMemoryMailbox = Inbox`.

  The mailbox module calls `Inbox` a compatibility adapter "while consumers
  migrate".
- **The migration is half done.** The agent loop (`Options.mailbox`) and
  `remind` delivery use `dyn Mailbox`. The following still name the concrete
  `Inbox`:
  - `failover/recovery.rs`;
  - `subagent/runner.rs`;
  - `subagent/tasks.rs` (`Tasks::notifications()`);
  - `runtime/wiring.rs`, which installs the hook and loads the sidecar.
- **Persistence.** Only the top-level session installs the hook. It rewrites
  `<id>.inbox.json` with temp file, `fsync`, rename and directory `fsync`
  (`remind::replace_private`).
  - A write failure is dropped (`write_inbox_file`).
  - An unreadable sidecar loads as empty (`load_inbox`), which silently loses
    the queued entries.

### Duplicate delivery

`deliver_notifications` (`otto-core/src/agent/mod.rs`) works in three steps:

1. append a context message;
2. `ack`;
3. persist the sidecar.

A crash between steps 1 and 3 delivers the notification again on reopen.
`Notification` has no stable identity, so nothing can recognise the
duplicate. `Entry.seq` is not written into the delivered message.

### Inputs that bypass the mailbox and are lost on restart

| Input | Where it lives today | On process loss |
| --- | --- | --- |
| Steering (`/inputs` when steered, `/tasks/steer`, TUI, ACP) | `agent::steering`, in memory, bound to a run generation; discarded on cancel or failure | lost |
| Model follow-ups (`task_queue`) | `app::task_control::ActiveRun.followups`, in memory, at most 16 | lost |
| Parent to child (`agent_send`) | per-task `Inbox` in `subagent::tasks`, no hook ("Child inboxes are never persisted") | lost; `agent resume` starts without them |
| Serve queued turns (`"queue": true`) | `server` `QueuedTurn`, at most 16 | lost |
| Local ACP queued prompts | `acp` prompt slot, at most 16 | lost |

### No request identity on inbound calls

The following carry no idempotency key, so a client that retries after a
timeout or a restart can submit the same input twice:

- `POST /v1/sessions/{id}/turns`, `/inputs` and `/tasks/steer`;
- ACP `session/prompt`.

`chat-connector` relies on the Feishu SDK's in-process event deduplication.
Telegram updates have no deduplication.

## Goals

1. **One queue abstraction.** `Mailbox` is the only name consumers depend on.
   The native runtime supplies one durable adapter, and `Inbox` remains the
   in-memory adapter.
2. **Delivered to the model exactly once.** Every mailbox entry has a stable
   input id. Delivery appends the session message only if that id is absent.
   A reopen that finds the id already in the transcript acknowledges the entry
   without appending again.
3. **Accepted inputs survive process loss** within the bounds set below:
   steering for the turn it targets, model follow-ups for their logical task,
   and parent messages for their child.
4. **Retry-safe inbound calls.** Clients may send a `request_id`. A retry with
   the same id in the same session returns the original outcome and never
   enqueues twice.
5. **Persistence failures are reported**, not dropped.
6. **Compatibility.** History stays append-only and Pi v3 compatible. Old
   sidecars and sessions keep working with today's at-least-once behaviour for
   their first reopen.

## Non-goals

- **Serve and ACP queued turns stay in memory.** This is the approved
  active-task contract: "disconnect/shutdown must not silently restart
  requests". Instead, `request_id` makes a client's resubmission after a
  restart safe.
- **No new storage backend.** The sidecar file remains the pending queue and
  the session file remains the delivery record. A non-POSIX mailbox adapter
  waits for a non-POSIX session store.
- **No cross-file transaction.** As in Phase 3, a stable id plus
  append-if-absent closes the duplicate window.
- **No change to when steering or follow-ups are placed**, to admission
  results (`steered`, `idle`, `approval_pending`), or to the 16-item limits.
- **Inbound deduplication is per session.** There is no global inbound
  ledger.

## Design

### 1. Input identity

`Notification` gains `id: String`. The serde default is empty, for legacy
sidecars. The id is a producer-stable **input id**, at most 128 bytes of
`[A-Za-z0-9._:-]`, derived from the input's durable source:

| Producer | Id |
| --- | --- |
| sub-agent completion | `task:<task id>:finished` |
| sub-agent report | `task:<task id>:report:<n>` (n = report ordinal) |
| reminder firing | `remind:<reminder id>:<fire_at unix ms>` |
| recovery notice | `recovery:<turn id or epoch>` |
| SIGTERM migration notice | `moved:<lease epoch>` |
| steering | the client `request_id` when given, else `steer:<turn id>:<n>` |
| model follow-up | `followup:<turn id>:<n>` |
| parent message to child | `parent:<task id>:<n>` |
| queued in-turn user message (TUI) | the client `request_id` when given, else generated |
| anything without a source | generated (`new_operation_id`) before the first sidecar write |

A legacy sidecar entry without an id gets a generated id on load. The first
reopen then behaves as today (at least once).

### 2. Delivery: `Session::append_input_if_absent`

There is a new WASM-safe session method:

```rust
async fn append_input_if_absent(&self, input_id: &str, message: Message)
    -> Result<AppendOutcome, SessionError>; // Appended | AlreadyPresent
```

- **Where the id is stored.** It goes in the appended message's Otto details
  as `inputId` (`PiOttoDetails`), which serves both user and context messages.
- **`Store`** keeps a set of input ids from the active path. It is built at
  open and updated on append. The check and the append run under the store
  lock, and the record is `fsync`ed before `Appended` returns.
- **`MemorySession`** deduplicates for the process lifetime.
- **Wrappers** forward. The method has no default body, like
  `interrupted_turn`.

`deliver_notifications` and steering delivery use this method. `ack` follows
either outcome. A crash between the append and `ack` is then harmless: the
reopened entry is acknowledged as `AlreadyPresent`.

The resilience spec named this primitive `append_context_if_absent`. It is
renamed because steering is appended as a `user` message, not as context.

### 3. `Mailbox` changes and the durable adapter

The port in `otto-core` changes in three ways:

- **`enqueue` returns `Result<u64, MailboxError>`**, where `u64` is the
  sequence. An entry whose id is already queued is not added again;
  `enqueue` returns the existing sequence. This in-queue deduplication is
  what makes `request_id` retries safe before delivery.
- **`Entry` gains `scope: Option<Scope>` and `kind` gains three values**, so
  that steering, follow-ups and parent messages can share the queue:
  - `Steer`: `scope = Turn(turn id)`;
  - `Followup`: `scope = Turn(turn id)`;
  - `ParentMessage`: `scope = None`, and the queue is the child's mailbox.

  Existing kinds have `scope = None`.
- **New method `wakes(&Entry) -> bool`.** It is true only for unscoped
  entries. `Controller::prepare_wake` counts waking entries only, so a pending
  steer never wakes a later turn, which is the reason the active-task spec
  gave for keeping steering out of the inbox.

**Native adapter.** `SidecarMailbox` in `crates/otto/src/runtime` implements
`Mailbox`:

- it wraps an `Inbox` and owns the sidecar path;
- it writes with `replace_private`;
- it returns persistence errors instead of dropping them.

On an unreadable sidecar it moves the file aside to
`<id>.inbox.json.corrupt-<unix>` and returns a warning, rather than loading
an empty queue.

**Who uses which adapter.**

- **Durable (`SidecarMailbox`):** the top-level session and every child
  transcript. Child sidecars are `<child>.inbox.json` beside the child file.
- **In memory (`Inbox`):** `--no-session` only.

**Finishing the migration.** `failover/recovery.rs`, `subagent/{runner,tasks}.rs`,
`tool/remind.rs` and `runtime/wiring.rs` take `Arc<dyn Mailbox>`.
`Inbox::set_persist` and `PersistHook` become private to the adapter. The
sidecar file name stays `inbox.json` for compatibility.

**Reporting failures.**

- Producers that run inside a tool call return the error as the tool result:
  `agent_send`, `agent_report` and `task_queue`.
- Background producers (timers, child completion) emit a warning event and
  log it, and the entry stays in memory.
- Admission endpoints return `503 input_not_persisted`. The input is not
  admitted.

### 4. Steering bound to a turn

**Today.** Accepted steering is in `agent::steering`, keyed by run generation.

**After.** Admission (`Steering::submit_for`) also enqueues a `Steer` entry
scoped to the active `otto.turn` id. The in-memory structure stays the
source for listing and withdrawing during the turn. The mailbox entry is the
durable copy.

| Event | Effect on the `Steer` entry |
| --- | --- |
| Delivery at a safe boundary | appended through `append_input_if_absent`, then acknowledged |
| Withdraw | removed |
| Turn ends in any way (completed, failed, cancelled) | the turn's remaining `Steer` entries are removed before its end record is written |
| Process loss, then a recovery turn that `resumes` this turn | the recovery turn restores them into its steering run and delivers them at its first boundary |
| Process loss and no automatic resume (resume exhausted, or no turn record) | removed on open, with a warning naming the count |

A completed, failed or cancelled turn removes its `Steer` entries, so
steering still never outlives its turn. Process loss is the one exception,
and only for the turn's own automatic resume, which continues that same
logical turn.

### 5. Model follow-ups bound to the logical task

`TaskControl::queue_followup` enqueues a `Followup` entry scoped to the turn
id of the logical user task. The in-memory deque remains the run-time view.

- `run_followups_held` acknowledges each entry after its follow-up step's
  `agent_followup` message is appended. That message carries the entry's input
  id, so a crash in between does not run the follow-up twice.
- Clearing follow-ups because approval is pending, or because of a denial
  (existing rule), removes the entries.
- **After process loss.** When the recovery turn of that task completes, the
  wake path runs its remaining `Followup` entries in FIFO order with the same
  approval rule as `run_followups_held`. Today only the prompt path runs
  follow-ups.
- **With no automatic resume**, the entries are removed on open, and a
  persisted, visible notice says they were not run. This reuses the existing
  "follow-ups were not run" notice.

### 6. Parent messages to a child

- `agent_send` enqueues a `ParentMessage` entry into the child's
  `SidecarMailbox`.
- When the child is interrupted, its sidecar keeps the undelivered messages,
  and `agent resume` delivers them before the resume prompt.
- The recovery notice lists the number of pending parent messages for each
  interrupted child.

### 7. `request_id` on inbound calls

These calls accept an optional `request_id` (same character set, at most 128
bytes):

- `POST /v1/sessions/{id}/turns` (JSON field);
- `POST /v1/sessions/{id}/inputs` and `/tasks/steer`;
- ACP `session/prompt` (`_meta.requestId`).

**Where the id goes.**

- *Prompts.* It becomes the input id of the turn's user message
  (`inputId` in Otto details).
- *Steering.* It becomes the input id of the `Steer` entry.

**Duplicate handling.**

- *A live duplicate*, still queued, running, or answered in this process, is
  served from a bounded per-session map (last 256 ids):
  - turns return the original turn summary with `200`;
  - steering returns the original admission.
- *A duplicate after a restart* is found in the transcript. It returns
  `409 duplicate_request` with the original message's entry id. It does not
  start a turn.
- *Serve queued turns.* A queued turn with a `request_id` is listed in the
  per-session map, so a retry returns the queued turn instead of a second
  queue slot. The queue itself stays in memory (non-goal).

**chat-connector.** It sends `request_id = chat:<platform>:<chat id>:<message id>`,
which deduplicates Telegram redelivery and Feishu retries across connector
restarts.

## Changes to approved contracts

These need explicit approval:

1. **Active-task guidance**, user contract: "Pending guidance is not
   persisted across process restart."
   - *Becomes:* "Pending guidance is persisted only for its turn's automatic
     resume after process loss; every other turn end discards it, as before."
2. **Active-task guidance**, ownership: "Do not use the persistent
   notification Inbox for undelivered steering, because its wake/recovery
   semantics deliberately survive a turn."
   - *Becomes:* steering uses the mailbox as turn-scoped, non-waking entries
     (sections 3 and 4). These entries do not survive a turn's end, and they
     never wake a turn.

The serve and ACP queue contract ("queue storage remains in memory") is
unchanged.

## Compatibility

- **Sidecar.** The entry gains `id`, `scope` and kind values. Old Otto
  ignores the unknown fields. Old Otto cannot decode the new kinds, so its
  load fails, which today means an empty queue. To limit this, `Steer`,
  `Followup` and `ParentMessage` entries are written to a second file,
  `<id>.inputs.json`, and old Otto never reads that file. `inbox.json` keeps
  only kinds old Otto knows.
- **Session.** `inputId` is an optional Otto detail that old Otto and Pi
  ignore.
- **HTTP and ACP.** `request_id` is optional. The OpenAPI document gains the
  field and the `409 duplicate_request` and `503 input_not_persisted`
  responses.

## Ownership

| Area | Change |
| --- | --- |
| `crates/otto-core/src/mailbox.rs`, `agent/inbox.rs` | id, scope, kinds, `wakes`, fallible `enqueue`, in-queue dedup |
| `crates/otto-core/src/session/` | `append_input_if_absent`, `inputId` detail, id index in `MemorySession` |
| `crates/otto-core/src/agent/` | delivery through the new primitive; restore `Steer` entries for a recovery turn; remove them before the end record |
| `crates/otto/src/runtime/` | `SidecarMailbox`, wiring, child sidecars, corrupt-file handling |
| `crates/otto/src/storage/backends/posix/session/store.rs` | input id index and append-if-absent |
| `crates/otto/src/app/` | follow-up entries; wake path runs a resumed task's follow-ups; `prepare_wake` counts waking entries |
| `crates/otto/src/subagent/`, `failover/recovery.rs`, `tool/remind.rs` | `dyn Mailbox`, producer ids, pending parent messages in the notice |
| `crates/otto/src/server/`, `crates/otto/src/acp/` | `request_id`, duplicate map, error responses |
| `chat-connector/internal/` | send `request_id` |

## Acceptance

Offline, deterministic tests that fail if the behaviour breaks:

- **Exactly-once delivery**
  - A notification appended but not acknowledged before a simulated crash is
    acknowledged on reopen without a second message.
  - A legacy sidecar entry without an id is delivered at least once and given
    an id.
- **Steering**
  - Steering accepted during a turn, then process loss, is delivered once by
    the recovery turn.
  - After a completed, failed or cancelled turn, no `Steer` entry remains.
  - When the resume is exhausted, the entries are removed with a warning.
  - A pending steer never starts a wake turn.
- **Follow-ups**
  - A follow-up queued before process loss runs once after the recovery turn.
  - A follow-up whose step message was appended before the crash is not run
    again.
  - Approval pending clears follow-ups and shows the existing notice.
- **Parent messages.** A message sent to a child that is then interrupted is
  delivered on `agent resume`.
- **`request_id`**
  - A repeated turn `request_id` returns the original turn while it is live,
    and `409 duplicate_request` after a restart.
  - A repeated steer `request_id` is admitted once.
  - A connector redelivery of the same message runs once.
- **Persistence failures**
  - A sidecar write failure returns `503 input_not_persisted` from `/inputs`,
    and a tool error from `agent_send`.
  - A corrupt sidecar is moved aside, and a warning is shown.
- **Compatibility**
  - Old Otto loading a new `inbox.json` still decodes it, because new kinds
    live in `inputs.json`.
  - The architecture guard: no module outside `runtime` and `otto-core::agent`
    names the concrete `Inbox`.

Docs: user manual (sessions, server API, ACP), `docs/development.md`, OpenAPI
document, CHANGELOG; capability descriptions for `agent_send`, `task_queue`
and `agent_report` (new error outcomes).

## Implementation decisions

Recorded 2026-10-06 while implementing sections 1 to 4, 6 and 7; they refine
the design above without changing its guarantees.

- **`input_id`, not `request_id`.** Otto already uses "request id" for log
  correlation (`X-Request-Id`, ACP `_meta.otto.requestId`), and
  chat-connector fills it with a random value per message. The idempotency
  key is therefore `input_id` in HTTP bodies and `_meta.otto.inputId` in ACP,
  and a retry of a delivered input answers `409 duplicate_input`.
- **Steering has its own mailbox instance**, `<id>.inputs.json` (open question
  1). Because it is a separate queue, no new `NotificationKind` and no
  `wakes()` filter are needed: the notification mailbox never holds steering,
  so steering cannot wake a turn. Entries use kind `user_message` and carry
  the turn id in `scope`.
- **Child mailboxes keep kind `message`** and the file name `inbox.json`
  beside the child transcript; old Otto reads them. No `ParentMessage` kind.
- **The durable adapter** is `runtime::sidecar_mailbox` (`install`, `load`)
  over the in-memory `Inbox`, not a separate struct. `Inbox::enqueue` returns
  `Result<u64, MailboxError>`; acknowledgement failures stay unreported,
  because an entry acknowledged in memory but still on disk is recognised by
  its input id on reopen.
- **Where `Inbox` is named.** Consumers take `dyn Mailbox`; the in-memory
  adapter is constructed only in `runtime`, `subagent::tasks` (the parent and
  child queues it owns) and the runner's fallback. A test enforces this.
- **Stale steering** from a turn that will not resume is removed when the
  next turn starts, not when the session opens, and without a warning.
- **Ids.** Reminder firings use `remind:<id>:<fire_at ms>`, recovery notices
  `recovery:<turn id>` or `takeover:<epoch>`; every other entry gets
  `in_<32 hex>` from the durable adapter. Sub-agent completions use a
  generated id, because a resumed task finishes again under the same task id.
- **Turn prompts.** The input id reaches the agent through
  `Agent::set_next_input_id`, which the next run always consumes. The server
  remembers the last 256 input ids per session.

### Section 5 (model follow-ups), implemented 2026-10-06

- The journal is its own mailbox, `<id>.followups.json`, like steering's.
  Entries carry the logical task's random scope and the id
  `followup:<scope>:<n>`, which the step's `agent_followup` message records.
- `TaskControl::begin_resuming` adopts journaled entries when the run is a
  wake turn that resumes an interrupted turn whose automatic resume is not
  used (`Controller::resumable_interrupted_turn`); `begin` discards them and
  the caller stores the "not run" notice with their count. Server and ACP
  wakes adopt them in `run_with_channel`; the terminal wake
  (`WakeOperation::run`) runs them itself when no task is active.
- A crash between two steps leaves no interrupted turn, so those follow-ups
  are discarded with the notice rather than resumed, as the design states.

## Open questions

1. **New kinds in a second file `inputs.json`**, or in `inbox.json` with a
   version bump that old Otto rejects? Recommended: a second file, so old Otto
   keeps working on the same session.
2. **A duplicate `request_id` after a restart.** Return `409
   duplicate_request`, or `200` with a synthetic summary that points to the
   original entry? Recommended: `409`, because the turn object no longer
   exists and pretending otherwise misleads clients.
3. **Should `request_id` become required** for chat-connector traffic only
   (the connector always has a platform message id)? Recommended: the
   connector always sends it; the server keeps it optional.
4. **The follow-up resume path (section 5) is the largest piece.** Ship it in
   this slice, or split it into its own PR after sections 1 to 4, 6 and 7?
   Recommended: same spec, separate PR.
