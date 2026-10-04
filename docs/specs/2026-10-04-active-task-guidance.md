# Active-task guidance and queued follow-ups

Status: approved requirements; implementation in progress (2026-10-04).
Tracks [issue #305](https://github.com/baiyuqing/otto/issues/305).
Execution status and evidence: [TODO](2026-10-04-active-task-guidance-todo.md).

## Approved requirements

1. Otto has actual model-callable capabilities, not just commands or API docs.
2. The user speaks naturally; Otto judges whether to guide active work or
   arrange a follow-up, without requiring command names from the user.
3. The model receives complete, accurate, actionable capability information on
   every request. History compaction/reload must not remove that information.
4. Preserve these requirements, decisions, and implementation progress in
   repository design/TODO documents rather than relying on conversation history.
5. New requested features are enabled by default. This delivery adds no
   default-off flag or opt-in setup: model task tools, durable capability
   instructions, natural-message routing, and command discovery are available
   after the ordinary installation/upgrade. Runtime state, actual permissions,
   and compatibility can restrict an operation; an enable switch cannot hide it.
6. After implementation and required verification, create the PR automatically
   and link it from the issue/TODO; do not ask again for PR creation approval.

The user approved these requirements in conversation. Implementation details
below must satisfy them; record material contract changes here before coding.

## Natural conversation and model discoverability

The user must not have to remember slash commands. Otto must have actual
model-callable operations and judge when to use them from natural dialogue.
Slash commands are optional deterministic entry points into the same behavior.

Every provider request must include the current callable tool schemas and a
short system-level capability policy describing when to use each operation.
These are reconstructed from installed runtime definitions, not stored as
ordinary transcript messages, memory, or skills. Transcript compaction, session
reload, and approval resumption must retain this policy and the applicable
schemas. Unavailable tools must not be described as callable.

The complete actionable contract includes argument schemas, target identity,
allowed states, source/authority, limits, result meaning, cancellation behavior,
and recovery on errors. `otto_help` can provide details, but its tool output is
not the sole durable capability reminder. Definitions/help/policy must agree
with registered tools, with tests capturing requests after compaction/reload.

Reuse `agent_send` for parent-to-child task guidance. Model-originated follow-up
requests must be labelled as agent-originated; they cannot manufacture user
authority or approve execution. Avoid exposing self-steering as a redundant
tool when the agent can directly revise its current plan.

A natural message arriving during active work is processed in a serialized,
bounded control dialogue using the session's provider. This dialogue has its own
in-memory transcript, the active task identity/context, and only the tools needed
to guide the captured turn, request a queued follow-up, read capability help,
or explicitly choose a read-only reply (for example, a progress question).
It cannot run ordinary task tools concurrently or grant approvals. Explicit
`/steer` and `/queue` bypass model interpretation. If routing makes no valid
decision or fails, report the outcome accurately; do not silently drop input or
claim that guidance was applied.

Control dialogue uses a 30-second bound and checks the captured turn generation
again before effects. An old dialogue cannot steer a newer turn. Pending
approval uses the existing approval-only dialogue for natural messages, with
ordinary task work queued until the human decides.

Steer/queue routing tools act on the original user message without model rewriting.
Only one routing choice may take effect per message. A reply-only choice does
not change the active task or queue. Successful tool acceptance is authoritative;
generated prose must not override a failed or missing routing decision.

The normal task model can use `task_queue` to schedule agent-originated follow-up
work within the user's authorized objective. Limit it to the current session,
16 accepted follow-ups per logical turn, 32,000 bytes per request. Follow-ups run
only after the current task and any approval retry finish, through the existing
agent execution path. Failure/cancellation discards pending model follow-ups.
Store delivered model follow-ups as `Role::Context` with `agent_followup` source,
not manufactured user requests. Do not add a self-steering tool to the ordinary
task model; it can revise its plan directly or guide children with `agent_send`.

ACP/server hold the logical follow-up scope across their existing approval
callbacks. Direct terminal prompt calls return to the frontend when approval is
needed and have no continuous approval callback. They must not run follow-ups
ahead of approval: if such a call ends awaiting approval, clear its remaining
model follow-ups with a persisted, visible notice that they were not run and
must be resubmitted after approval. Preserve the existing terminal approval
flow; continuous terminal follow-up resumption is outside this first chat/ACP
delivery. State this boundary in the tool contract rather than hiding it behind
a configuration switch. Denial also clears remaining model follow-ups.

## Scope

Add `/steer <guidance>` and `/queue <prompt>` to ACP and Telegram/Feishu,
and ensure ACP tool calls reach a terminal display state when a turn ends.
Structured plans/usage, session fork/resume, and pagination are deferred.

## User contract

- `/steer` accepts text only for an active user turn. Idle, closing, and
  approval-waiting sessions reject it with a clear explanation. It does not
  interrupt an in-flight provider request or tool, undo effects, or grant
  permission. The next safe provider boundary consumes guidance in FIFO order
  as a user message. Multiple tools already requested by one provider response
  finish before that boundary. Acknowledgement means accepted, not applied.
- Guidance belongs to the exact active turn. Turn completion and acceptance
  must be synchronized: guidance accepted before completion is consumed before
  successful completion; acceptance after completion is rejected. Cancellation
  or failure discards undelivered guidance, which never wakes a later turn.
  Delivered guidance stays in append-only history. Pending guidance is not
  persisted across process restart.
- `/queue` submits a distinct normal text prompt for the same session. Idle
  sessions start it immediately; busy sessions execute accepted prompts in FIFO
  order after the active operation ends. Pending approval continues to block
  execution. The command acknowledgement is separate from the eventual answer.
- Reuse existing server and connector queue limits and lifecycle behavior.
  Local ACP uses the server's existing limit of 16 waiting prompts. Full queues
  reject input explicitly. Queue storage remains in memory; disconnect/shutdown
  must not silently restart requests in a different session. A connector `/stop`
  cancels its active work and clears its pending prompts, as today. Individual
  server turn cancellation retains its existing semantics.
- Inputs must contain non-whitespace text and at most 32,000 UTF-8 bytes.
  Attachments are outside this command's scope. ACP/model capability information
  describes only implemented operations. Telegram's static menu lists the
  connector's implemented commands; an older agent reports explicit steering
  as unsupported with upgrade guidance, without receiving it as an ordinary
  model prompt. Natural messages fall back to the existing follow-up queue.
  Do not add a startup capability probe or dynamic menu framework for this
  compatibility case.

## Ownership and compatibility

The application/agent layer owns turn-bound steering acceptance, delivery, and
cleanup. Expose the same operation through local ACP and the HTTP client/server
for attached ACP. Do not use the persistent notification Inbox for undelivered
steering, because its wake/recovery semantics deliberately survive a turn.

Reuse existing normal-turn admission and queue execution paths for follow-ups;
do not introduce a competing scheduler or copy the agent loop into adapters.
Connector routing must process steering immediately while an ordinary prompt
is running, rather than placing it behind that prompt. Preserve the existing
restricted approval dialogue for ordinary messages.

## Implementation map

- `otto-core::agent::steering`: transient mailbox, monotonic run generation,
  bounded FIFO, atomic acceptance/final close, and cleanup guard. `Options`
  carries the shared mailbox; safe-point delivery stays in the agent loop.
- `cli::runtime_builder` and `app::task_control`: wire model tools at runtime,
  own shared control dialogue/follow-up behavior, and expose controller methods.
- `cli::prompt`: capability policy derived from registered definitions, present
  outside the mutable transcript. `otto_help` remains an installed-version view.
- HTTP/client and ACP: thin adapters for the same controller operations, with
  explicit capability negotiation and session binding.
- `connect/internal/agent` and `connect/internal/bridge`: receive natural input
  while a task is running, call control dialogue without waiting behind the
  ordinary prompt, and respect the returned route. Keep replies from control
  dialogue separate from the running task's streamed reply.

Wire entry points: HTTP `POST /v1/sessions/{id}/tasks/steer` (`{text}` ->
`{accepted:true}`) and `/tasks/control` (`{text,channel?}` -> nullable
`{text,queued}`). ACP exposes `_otto/tasks/steer` and `_otto/tasks/control` with
the same bodies plus `sessionId`, negotiated with `_meta.otto.taskControl:true`.
Extension replies are JSON-only; they must not pollute the running prompt's
stream. Adapters validate text size, channel enum, and session access.

Local ACP's explicit `/queue` needs a small bounded FIFO around its existing
prompt slot (16 waiting prompts); drain through the same prompt runner. Normal
concurrent prompt refusal stays compatible, `/steer` bypasses the occupied slot,
and session cancellation clears queued local prompts. Attached ACP uses the
existing server FIFO rather than duplicating it.

Existing persistent Inbox is insufficient for exact-turn guidance: accepted
input can race final completion and wake a subsequent turn, or arrive before
the initial prompt is appended. The new transient mailbox must prevent both.

ACP v1 supports `completed` and `failed`, not a separate cancelled tool status.
Represent cancellation/denial as failed with explicit content. Persistence
failure after tool start must close the display without inventing durable
success or claiming effects were undone.

## Tool-call display completion

Audit local and attached ACP through the shared event mapper. A started call
must get exactly one terminal display update when its turn ends while the
connection is still deliverable. Cover success, cancellation, approval denial,
execution errors, and persistence failure after tool start. A fallback display
closure reports failure/unknown completion, never invents a successful result
or changes durable operation facts. Disconnect cannot guarantee delivery;
history recovery continues to use stored facts.

## Acceptance

Behavior tests cover guidance during a blocked provider/tool, ordering,
completion races, cancellation/failure cleanup, idle and approval rejection,
queued answers and limits, session isolation, and old-agent compatibility.
Cover local ACP, attached ACP, direct server ACP TCP, and connector routing;
use offline fake providers. Update HTTP schemas if wire fields/routes change,
ACP capability advertisement, Telegram menu, and user manual together.

Run focused tests, `make check-fast`, `make connect-check`, and the full macOS
acceptance gate. No new dependencies or generic command framework.
Verify default configuration exposes the capabilities and natural-message
routing without extra flags. Do not introduce a default-disabled feature gate.
