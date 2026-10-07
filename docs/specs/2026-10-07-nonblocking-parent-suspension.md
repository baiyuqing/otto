# Runtime-owned non-blocking parent suspension

Status: approved requirements; implementation in progress (2026-10-07).

## Problem

The parent-facing `agent_wait` capability and `agent(wait: true)` run a long-lived
future inside one model tool call. The assistant tool call cannot receive its
terminal tool result until that future ends, so the parent agent loop cannot
make another provider step in the meantime. Although the runtime is asynchronous,
the logical parent turn is occupied by the tool call.

The old implementation also described active user input as a wake condition but
watched a generic parent notification Inbox. Real active-turn guidance uses the
exact-turn steering mailbox instead. Its direct unit test injected a synthetic
Inbox `UserMessage` and therefore did not prove the frontend-to-steering path.
The local TUI could additionally hold active-turn admission behind a general
mutation lock.

## Approved requirements

The user explicitly rejected a patch that only adds steering to the existing
`agent_wait` select loop. Waiting belongs to the runtime, not in a model tool
future. While children run, user guidance, cancellation, approval decisions,
frontend lifecycle work, and child events must remain independently admissible.
The feature is enabled by default and must preserve append-only transcript and
operation-ledger integrity.

After implementation and required verification, update the patch version,
push the development branch, and open a PR without asking again.

## Decision

### Remove synchronous model waiting

Remove `agent_wait` from the installed model tool set and remove the `wait`
argument from `agent`. `agent` starts or resumes a child and always returns a
terminal tool result immediately. A provider response therefore never leaves
an assistant tool call pending merely because a child is running.

This is an intentional capability break rather than a deprecated blocking
alias. Keeping the old name with non-blocking behavior would make old prompts
believe they had observed completion when they had only polled state. Callers
that need an immediate snapshot use `agent_status`; callers that need to alter
a child use `agent_send` or `agent_cancel`.

The model policy tells the parent to continue independent work after delegation
and otherwise finish its current response. It must not poll `agent_status` to
simulate waiting. Child completion or a child report arrives later as a
`[task-notification]` context message.

### Runtime-owned suspension and wake

The existing session mailbox and frontend wake-turn machinery own suspension:

1. `agent` records a complete successful tool operation and returns the task id.
2. The parent may continue useful independent work. If none remains, it ends
   the current turn normally; no future remains parked in a tool executor.
3. Child completion/report is queued in the parent's mailbox. The mailbox
   change signal is observable independently of the finished parent turn.
4. REPL, TUI, ACP, and server wake loops claim the session through the normal
   operation admission rule and start an empty-text wake turn.
5. Core delivers queued notifications as append-only context before the wake
   provider request. The provider can then integrate the result and continue.

The mailbox is the durable source of wake work. No separate persisted
"suspended" bit is introduced: a running task alone does not require a provider
turn, while a pending report/completion does. Process restart restores task and
mailbox state through their existing repositories; an interrupted child keeps
its existing explicit resume contract.

A direct one-shot CLI invocation may keep the process alive until its children
settle and then run wake turns. That is a frontend lifecycle guarantee, not a
model tool call, and remains cancellable by the process operation token.

### Transcript and race invariants

- Every assistant tool call gets exactly one terminal operation fact and one
  persisted tool result before the core loop can end or dispatch again.
- Starting a child is the effect reported by `agent`; child completion is not
  part of that tool's result.
- A completion that lands before the parent turn ends remains queued and is
  consumed at the next normal notification checkpoint. A completion that lands
  after it ends starts a wake turn. It is delivered once in either race.
- Active-turn steering keeps its exact-turn mailbox and safe-boundary contract.
  It must not be converted to a later wake or queued prompt.
- Cancellation settles the active provider/tool boundary normally and does not
  detach an effectful child operation.
- Wake admission remains serialized with ordinary turns, so a notification
  cannot create two concurrent provider requests for one session.

### Control-plane concurrency

Frontend request scheduling must distinguish control-plane operations from
ordinary session mutations. Active-turn input admission, explicit steering,
steering withdrawal, turn cancellation, and approval decisions must not wait
behind the long-running prompt they control. Session replacement, profile and
configuration mutation, and new ordinary turns remain serialized.

The TUI must submit ordinary active-turn text directly to `AdmitInput` even
while a prompt mutation is active. HTTP and ACP retain their existing atomic
active-input admission endpoints and prompt-slot rules; tests must prove these
paths rather than pushing directly into an Inbox.

## Compatibility and discovery

This change removes a model-callable capability and changes the `agent` schema,
so all generated capability descriptions, system policy, `otto_help` output,
embedding/tool-definition expectations, user documentation, development
contracts, and changelog entries change in the same release. Historical specs
remain historical and receive a superseding note instead of being rewritten as
if the old behavior never existed.

Unknown calls to `agent_wait`, and `agent` calls that still include `wait`, fail
schema admission clearly. They never regain the old blocking behavior.

## Acceptance

Offline behavioral tests cover:

- the installed tool set and provider schemas contain no `agent_wait` or
  `agent.wait` field;
- `agent` and resumed `agent` return before a blocked child completes;
- every start call has a complete operation ledger/tool result immediately;
- child completion before and after parent turn completion is delivered exactly
  once, with the latter starting a wake turn;
- real local TUI active-turn input reaches exact-turn steering while the prompt
  is active and is not held as a later queued prompt;
- server and ACP admission remain responsive while their prompt is active;
- cancellation, process-close, interrupted-child resume, and one-shot process
  lifetime remain bounded and do not lose durable notifications;
- capability documentation agrees with the registered tools after restart and
  compaction.

Run focused tests, `make rust-wasm-check` if shared core changes, then
`make check-fast`. Do not run the full local CI gates.
