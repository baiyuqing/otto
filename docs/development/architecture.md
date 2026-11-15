# Architecture

[Development guide](../development.md) · [Code ownership map](../code-organization.md)

## Package boundaries

- `otto-core` owns portable contracts, formats and the agent loop. It must
  remain I/O-free: no filesystem, network, process, environment,
  stdio or wall-clock access. Provider-specific wire types stay in
  `openaicompat` and `openairesponses`, behind the neutral provider contract.
- `otto` is a library. It owns native I/O, tools, sandboxing, storage,
  authentication, the `embed` API and the terminal UI. `app` owns shared use
  cases; frontends adapt presentation and transport rather than
  reimplementing domain behavior.
- `otto-server` (HTTP serve frontend), `otto-acp` (ACP stdio and attach
  frontends) and `otto-cli` (the `otto` binary and composition root) are
  separate packages. Dependencies are one-way:
  `otto-core` <- `otto` <- `otto-server` <- `otto-acp` <- `otto-cli`. A crate
  never imports one to its right. A one-line change in a frontend crate
  rebuilds and retests only that crate and those to its right, not the `otto`
  library test binary.
- The `otto` public API is what the frontend crates need. Widening an item
  from `pub(crate)` to `pub` is how a frontend crate reaches it; new frontend
  code goes into the frontend crates, not into `otto`. `cli::{login, repl,
  repl_commands}` stay in `otto` because `tui` uses them.
- Test-only items that a dependent crate's tests need are public and gated
  `#[cfg(any(test, feature = "test-support"))]`, not `#[cfg(test)]`. Dependent
  crates enable `otto/test-support` in `[dev-dependencies]` and take
  scaffolding from `otto::fixtures`.
- `runtime/startup.rs` is the shared composition root for CLI, serve and embed.
  Inject domain ports there. CLI-only flag/frontend steps stay in
  `otto_cli::run`.
- `chat-connector/` is an independent Go ACP client; it links no Otto runtime
  code. The browser UI lives in [otto-plus](https://github.com/baiyuqing/otto-plus).

Use the [ownership map](../code-organization.md) for module entry points rather
than duplicating their inventory here. Dependencies point toward shared
contracts; interfaces belong at actual consumer boundaries, not speculative ones.

File-tool confinement belongs in native `tool`; Bash delegates to `sandbox`
and starts in the selected workspace. Only explicit sandbox `off` uses direct
execution. Provider HTTP/OAuth and credential persistence remain native.

Chat channel metadata uses `app::Channel`, passed through admitted HTTP/ACP
turns. Render fixed guidance for supported channels, never client-authored
system instructions. Per-run context applies to every request/retry without
mutating the configured prompt or session history; absent metadata means no
channel context.

Memory consumers use the service's bound accessors or the controller facade,
never repositories directly. Files are the default memory backend and are not
a storage domain. Model writes become review candidates; human management
follows its own explicit contract. External memory services run as plugins,
not as a memory backend. See the
[memory manual](../manual/memory-and-extensions.md#memory).

## Workspace ownership

`workspace` owns the versioned identity marker, initialized before session
lookup, including serve startup. Read-only lookup derives the same keys when
no marker exists; configured memory IDs are adopted once and conflicts fail.

Use pinned `fsops` descriptors, refuse symlinks, serialize changes with the
workspace lock and publish complete markers atomically. Corruption fails
closed. Historical paths record ownership, not execution authority: tool and
approval boundaries remain path-based. Session candidates still validate their
stable directory, file identity and recorded cwd. Never rewrite transcripts
when paths change. Identity UUIDs are not session IDs or process lease epochs.
Forking leaves old state untouched and requires stopped workspace processes.

## Native storage contracts

`storage/{contracts,sql,files}.rs` define guarantees; drivers implement I/O;
`storage/backends/` owns schemas and row mappings. `runtime/storage.rs` opens
repositories at their local paths and injects domain-owned ports. Storage is
local SQLite (bundled through `rusqlite`) and POSIX session files.

Domain services own validation, state transitions and recovery. Repositories
own atomic mutations, revisions/claims, codecs and schemas. SQL drivers must
validate dialect, immediate serializable transactions, durability and writer
concurrency. An uncertain commit is `CommitUnknown`, not rollback. Never replay
an uncertain external effect. Async session writes preserve append-only Pi v3.

Preserve synchronous domain ports. Adapters that need async I/O may use the
bounded `storage::worker::Worker`; async callers await `call`, while
`call_blocking` requires a multi-thread runtime or a separate blocking thread.
Accepted work is not withdrawn by cancellation; close/drop drain owned work.
Lost acknowledgements retain existing command/receipt and unknown-effect rules.
Run `otto_core::conformance::{session,mailbox}` against every implementation
and wrapper, with reopen support for durable state.

## Core contracts

- Use neutral model validation and owned `Clone` values. Both `MemorySession`
  and native `Store` enforce validation and tool-call/result sequencing on
  append. Transient messages may lack IDs/timestamps; Pi codec restrictions
  stay in the session layer. Update ownership tests for new fields.
- `Response::message` owns finish reason and usage. Missing usage differs from
  explicitly reported zero; cached-input presence is separately tracked.
  Preserve both distinctions through events, persistence and aggregates.
  `Usage.input_tokens` includes cache reads and writes. Optional
  `cache_write_input_tokens` and its `cache_write_1h_input_tokens` subset retain
  missing versus zero; aggregate them only across complete reports. Claude's
  cumulative SSE counters are disjoint and replace prior counts on updates.
  Pi assistant/checkpoint details preserve optional counts that mandatory Pi
  zeroes cannot represent; any duplicate counts must agree with Pi usage.
  Old serialized usage remains readable; Rust callers constructing `Usage`
  literals must supply optional fields or use `..Default::default()`.
- Usage storage contains content-free records only, never prompts, responses,
  tool bodies or opaque context. Logical calls, transport attempts and token
  reports are distinct counts. Frontends query the shared API.
- Typed `ContextMetadata` owns associations; use `task_id`, with text parsing
  only for legacy history. Preserve append-only Pi v3 and optional namespaced
  metadata; never invent missing historical fields.
- Custom entries are not ordinary model context. `append_custom` implementors
  explicitly write or drop entries. Provider-owned compaction is returned only
  to its producer, never projected as UI/model text. Turn and provider-call
  records remain versioned and redacted; their codecs live in
  `otto-core/src/session/{turn,provider_call,memory_recall}.rs`. Memory recall
  records and checkpoint memory fields hold record IDs and revisions only.
- Interrupted calls distinguish possible execution from known nonexecution.
  Recovery uses typed operation facts and tool-owned retry safety, never tool
  names or output text. Automatically replay only genuine read-only effects;
  uncertain effectful operations require reconciliation.
- Inputs flow through the mailbox port. Keep admission, ordering, cancellation,
  steering and wake behavior shared across frontends. Durable inboxes and
  session wrappers must retain the same conformance guarantees. A step interrupt
  with queued guidance continues the same turn, not a new turn; pending approval
  refuses interruption. Failed partial provider fragments do not become history,
  and stopped tools retain their real effect certainty rather than becoming
  safe to replay.

The executable contracts and tests beside `agent`, `session`, `mailbox` and
native `operation` own detailed record shapes and state machines. Changes to
those contracts update codecs, compatibility tests and consumers together.

## Provider retry boundary

`retry.rs` owns budget/backoff arithmetic; provider adapters own transient-error
classification and retry safety. The current native adapters deliberately retry
some dispatched requests even when provider work or billing may repeat. This
is not an exactly-once guarantee and does not authorize replay of Otto tools.

- OpenAI-compatible uses at most four attempts, with jittered delays bounded by
  1/2/4 seconds and a numeric `Retry-After` capped at 30 seconds. Connection failures, selected
  transient transport interruptions and model-output idle timeouts can retry;
  transport interruptions and idle timeouts may retry after partial output.
  HTTP 429/503 retries require no visible model output.
- ChatGPT uses at most seven attempts with delays capped at 30 seconds. Its
  owned transient HTTP/stream/transport classes can retry after partial output.
- Failed fragments never enter assistant history or authorize tool execution.
  Retry events reset pending stream presentation. Deadlines/cancellation still
  bound the logical call, and lost transport does not prove no provider effect.

See `provider/{openaicompat,chatgpt}.rs` and their adjacent retry tests for exact
classifiers; do not generalize one adapter's policy to another endpoint.

Provider/MCP **keyed idempotency remains a separate design approval gate**.
Before implementing it, approve the concrete trusted adapter/extension schema,
receiver deduplication and retention semantics, request/key binding, and unknown
outcome reconciliation. Without a supported receiver contract, retain
`NonIdempotent` behavior rather than inventing an external capability.

## Lifecycle and frontend contracts

Lifecycle and task/authentication capabilities belong in `app`, not UI code.
`app/lifecycle.rs` owns Controller transitions: `Open` carries an idle,
reserved-wake or active admission; `CloseRequested` retains only an active
admission; `Disposing` gives one caller cleanup ownership; `Closed` retains
the cleanup result. Admission generations reject stale releases and replacement
commits. Reserve a wake atomically with admission, and change it to active before
executing it. Dropping a reservation releases it; requesting close revokes an
unstarted reservation and defers disposal until active work releases admission.
`request_close` only rejects admission and requests shutdown; it never disposes
resources itself. Disposal runs outside the Controller mutex, and concurrent
`close` callers wait for the same result.

`otto-acp/src/prompt_slot.rs` owns connection-local prompt admission. Its activity
is `Idle`, `Prompt(token)` or `Wake(token)`; the occupied states carry the token
that cancellation signals. Admission, wake handoff and release transition under
the same slot mutex. Cancellation clears queued prompts and signals active,
waiting and follow-up tokens, but only the active owner's completion releases
the slot. Queue draining and ordered follow-up admission remain separate from
activity; saved inputs retain their deduplication and recovery ordering.

`otto-server/src/session_activity.rs` owns HTTP session arbitration. Activity is
`Idle`, `Turn(handle)` or `Compacting(token)`; admission is `Open` or `Closing`.
Closing retains active cancellation ownership until completion, and completion
never reopens admission. The latest turn is retained separately for status and
SSE replay; it does not authorize new execution. Start, completion and close
transitions run under the session mutex. Pending/restored queues, approval waits
and input receipts keep their own ordering and identity contracts; recovery
wakes still run before restored prompts.

The attached TUI's connection owns its status stream while connected. Its
transport request is idle, reading, sending, reconnecting or canceling; a send
owns any deferred cancellation intent. Disconnect drops replaceable reads and
retains send/cancel ownership until settlement. Session, epoch and transcript
checks fence result adoption, and reconnect publishes connected state only
after authoritative reload of the same session. Normal exit drains owned requests,
including a cancellation produced by a completed send. Following a turn and
running compaction remain independent activities.

The Go connector's per-chat queue ownership is `Idle`, `Switching`, `Working`
or `Stopping`, with transitions under the chat mutex. A worker retains ownership
through session-lock waits, prompt execution and reply delivery. `/stop` clears
the existing queue and requests cancellation without releasing that worker;
later arrivals can queue and start with fresh stop state when dequeued.
`/use` owns `Switching` through session loading and binding publication and
releases it on success or failure. Approval and task-control workers retain
their independent ownership and generations.

Connector task control has `Idle`, `Active` and `Draining` phases. A task record
binds session, generation, cancellation and queued controls; its worker handle
owns that record and a completion channel. Main-task completion drains control
replies before releasing its task. `/stop` revokes routing and cancels the task,
while captured workers retain ownership until their work and admitted replies
settle. Stale completion can close only its own worker and cannot clear a newer
task's queue or wait on its worker. Natural control remains available during
draining; `/steer` requires Active. All transitions run under the chat mutex.

Connector approval dialogue is idle when no record is attached. Each running
record binds session, generation, FIFO input, cancellation and completion, and
retains ownership through RPC and reply delivery. Routing uses the live prompt's
session, or the saved chat binding for background approvals. `/stop` or a session
replacement cancels and detaches the record; stale results and completion affect
only their captured record. Empty-queue completion detaches under the chat mutex
so the next enqueue starts a fresh worker. Shutdown rejects new input and drains
admitted dialogue workers before closing the agent. Dialogue never grants tool
permission; existing decision commands and cards remain the authority.

Session replacement must preserve shared state and admitted work; avoid
frontend-specific execution paths. `runtime/session_resources.rs` owns one
session's inbox, tasks, timers, output leases, MCP connections and workflow
notification binding. Runner replacement for model/thinking changes or a
background MCP swap borrows these resources; it must not close them or cancel
existing children. Children retain their original executor, while new calls
use the replacement. The child concurrency semaphore belongs to the shared
task registry, so replacement cannot multiply the session's parallelism limit.

`SharedSession::close`/`close_resources` own teardown. Closing only a runner is
not closing its session. Keep append-only history and recoverable operation
facts separate from transient presentation.

Approval presentation is not authority: use the shared decision service and
exact request/version checks described in [contracts](contracts.md#unified-approval-contract).
Authentication captures startup credentials; changing those requires restart.
Model switches refresh backend state even when saving the default fails.
Frontends never persist credentials themselves.

## Embedding API

`otto::embed` builds the shared native runtime in process. Its module rustdoc
is the API contract:

- The compatibility surface is `embed` and the types its signatures name or
  re-export; other native `pub` items have no blanket stability promise.
- Changes are additive even before 1.0: no removals, renames or incompatible
  signatures. Extend non-exhaustive enums and builder methods, not public fields.
- Component switches only disable into an already-supported runtime state;
  introduce that state before exposing a new switch.
- Use one `Otto` per process because the config lock is per-process `fcntl`.
  Construction tests hold the shared lock in `embed.rs`.
- New shared startup steps go in `runtime/startup.rs`, not only in an embed or
  CLI wrapper.
