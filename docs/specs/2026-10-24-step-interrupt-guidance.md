# Esc with queued guidance interrupts the current step

Status: approved 2026-10-24.

## Problem

Guidance typed while a turn runs (`Command::AdmitInput`,
`POST /v1/sessions/{id}/inputs`, `POST /v1/sessions/{id}/tasks/steer`) is
queued in `Steering` (`crates/otto-core/src/agent/steering.rs`) and appended to
the session at three points of the agent loop
(`crates/otto-core/src/agent/turn_run.rs`): before the first provider call,
after each tool batch, and when the model would otherwise end the turn. A
provider response that streams for minutes or a tool call that runs for
minutes delays delivery by that much.

The only way to act sooner is `Esc` (`crates/otto/src/tui/app/keys.rs:152`,
`Action::CancelTurn`). It stops the turn with `OperationStopReason::UserCancellation`,
the turn ends `Cancelled`, the queued guidance is discarded and reported, and
its text is put back into an empty composer. The user then sends it as a new
turn, and the model has lost the partial state of the cancelled turn.

## Terms

- Turn: unchanged. The work between one `otto.turn` start record and its end
  record; one `TurnOutcome` (`Completed`, `Failed`, `Cancelled`), one
  `turn_end` hook, one `turn_timeout`.
- Step: one provider call and the tool calls of its response. The agent loop
  in `run_turn` runs one step per iteration.
- Step interrupt: the new operation. It stops the step that is running, delivers
  every queued guidance message, and continues the same turn with the next
  provider call.

## Decisions

- 2026-10-24 (user): `Esc` with queued guidance delivers the guidance inside the
  same turn (option B); it does not start a new turn.
- 2026-10-24 (user): a tool that is executing when `Esc` is pressed is
  terminated; the agent does not wait for it to finish.
- 2026-10-24 (user): the terms above.
- Proposed here, for approval:
  - Partially streamed reply text of an interrupted provider call is not stored
    in history; the TUI keeps the streamed text on screen. This is what a
    cancelled turn does today (`provider_step.rs:505-507`: failed stream
    fragments never become model context).
  - `Ctrl+C` keeps cancelling the whole turn. `Esc` without queued guidance
    keeps cancelling the whole turn.
  - A step interrupt is refused while an approval is pending; `Esc` then does
    not cancel the turn either. The TUI shows the refusal and names `Ctrl+C`.
  - The interruption reuses `OperationStopReason::UserCancellation`. No new
    stop reason: the operation facts and events already carry this value for a
    user-initiated stop, and no consumer needs to tell the two apart.
  - The route exists on serve for `otto --attach`; ACP and chat connectors are
    not changed.

## Behaviour

### Trigger

In the TUI, at the composer, while a turn is running (`App::busy()`):

| Key | Queued guidance | Effect |
| --- | --- | --- |
| `Esc` | none | Cancel the turn (unchanged). |
| `Esc` | at least one `Status::Queued` message in `app.pending_steers` | Request a step interrupt. |
| `Ctrl+C` | any | Cancel the turn (unchanged). |

Queued guidance means the TUI's view (`app.queued_steer`). The run is the
authority: the backend refuses the request when the run has no queued guidance
at that moment (another client withdrew it), and the TUI then shows
`No queued guidance; Esc again cancels the turn.` and refreshes the steering
state. The TUI never turns a refused interrupt into a turn cancel.

The composer title with queued guidance changes from
`Queued for current turn · Ctrl+U recall to edit · Esc cancels turn` to
`Queued for current turn · Ctrl+U recall to edit · Esc delivers now`.

`Esc` inside a feature screen or the approval dialog keeps its current meaning
(dismiss).

### Agent loop

`run_turn` gives every step a step control: an `OperationControl` whose
cancellation token is a child of the turn token, whose `remaining()` and
`stop_reason()` come from the turn control, and whose `admission_stop_reason()`
returns `Some(UserCancellation)` when its own token is cancelled and the turn
control is not stopped. The loop registers the step token with the steering run
before the provider call and before each tool call (`arm`), and unregisters it
after the call returns (`disarm`).

`Steering::interrupt_step()`:

1. Fails with `Inactive` when no run is active, with `NoGuidance` when the run
   has no `Queued` message.
2. Otherwise sets `interrupt_pending` on the run, cancels the registered step
   token if one is registered, and returns the number of queued messages.

`interrupt_pending` makes the request sticky: `arm` cancels the new token at
once while `interrupt_pending` is set and guidance is still queued, so a tool
call that starts between the request and the next delivery is settled as not
started instead of running. Delivery (`Run::drain`, `Run::take_or_close`) and a
withdrawal that leaves the queue empty clear `interrupt_pending`.

What the loop does when the step token fires:

- During the provider call: the provider returns `ProviderError::Cancelled`.
  `complete_normal_provider_attempt` records the provider call (`otto.provider_call`
  with disposition `cancelled`, stop reason `user_cancellation`) and an
  `otto.provider_recovery` record with `outputStarted`, as it does today for a
  cancelled turn. No assistant message is appended. `run_turn` sees
  `AgentError::Provider(Cancelled)` with a cancelled step token and an
  unstopped turn control: it emits `Event::StepInterrupted`, delivers the
  queued guidance (`deliver_steering`), and continues the loop. Any other
  error, or the same error with a stopped turn control, fails the turn as
  today.
- During a tool call: the executor sees its cancellation token fire. `bash`
  kills the process group and returns the partial output with
  `ToolResult::cancelled_unknown()` (disposition `cancelled`, effect certainty
  `unknown`, `crates/otto/src/tool/bash/execute.rs:61-65`); other tools return
  through their existing cancellation paths. The tool result is persisted and
  `ToolCallFinished` is emitted as for any cancelled tool. The remaining tool
  calls of the batch are settled by `admission_stop_reason()` as
  `stopped_tool_execution(UserCancellation)`: result text
  `ProviderError::Cancelled`, disposition `cancelled`, effect certainty
  `not_started`. Every assistant tool call therefore still has a tool result.
  After the batch the loop reaches `deliver_steering` as today; before it, the
  loop emits `Event::StepInterrupted`.
- During compaction inside `dispatch_normal_provider_step` (proactive or
  overflow): the compaction's child control observes the cancelled token and
  returns `Cancelled` before it writes a summary; the context stays as it was.
  The loop treats this as the provider-call case above. `proactive_attempted`
  stays set, so the turn does not attempt proactive compaction again; overflow
  compaction is unaffected.
- Between steps (no token registered): nothing is cancelled. Delivery happens
  at the boundary the loop is already heading for; `arm` of the next step is
  the fallback described above.

The progress guard (`state.progress`, `tool_dispatch.rs:275-287`) does not
observe a tool execution whose `outcome.stop_reason` is `Some(..)`: a stop
requested by the user or a deadline is not evidence of the model repeating
itself. Without this rule each interrupted tool adds one consecutive failure
(`progress.rs:179-183`).

The turn-level checks are unchanged: `stopped_provider_error(control)` reads
the turn control, so a step interrupt never ends the turn; the turn deadline,
`turn_timeout`, `TurnOutcome` and the `turn_end` hook are unchanged. A turn
that was step-interrupted and then finished normally ends `Completed`.

`Event::StepInterrupted { guidance: usize }` is emitted once per accepted
interruption that cut a step, immediately before the guidance is appended.
`guidance` is the number of messages that will be appended. The wire event is
`{"type":"step_interrupted","queued_guidance":N}`.

### What the model sees

Nothing new. After a tool-call interrupt the next request holds the assistant
message with its tool calls, one tool result per call (cancelled, or not
started), then the guidance as user messages. After a provider-call interrupt
the next request holds the messages up to the last completed step, then the
guidance. The guidance text is the user's text after redaction, as today.

### TUI

- Local mode: `Esc` with queued guidance enqueues
  `Request::Command(Command::InterruptStep)` (the path `Ctrl+U` uses for
  `WithdrawSteer`, `keys.rs:163-168`). The local backend calls
  `Controller::interrupt_step()`. A success reply shows no system line (the
  `step_interrupted` event reports the result); a refusal shows the reason.
- Attached mode: the remote backend calls `Client::interrupt_steering`, which
  posts the route below.
- On `step_interrupted` the TUI closes the open streaming reply and reasoning
  rows (so the next reply starts a new row) and pushes the system line
  `Step interrupted; delivering 1 queued message.` (`N queued messages` for
  N > 1). The delivered guidance then arrives as `user_message` events and
  replaces the `[Queued for current turn]` rows as today.
- Cancelled tool rows show `cancelled` as they do for a cancelled turn.

### HTTP

| Route | Behaviour |
| --- | --- |
| `POST /v1/sessions/{id}/tasks/steer/interrupt` | Interrupt the running step and deliver the queued guidance inside the current turn. `200 {"queued_guidance": N}`; `409 no_queued_guidance` when the active run has no queued message or no run is active; `409 approval_pending` while an approval waits; `404` unknown session. No body. |

`Controller::interrupt_step() -> Result<usize, String>`: refuses while
`pending_approval().is_some()`, then calls
`runner()?.steering().interrupt_step()` and maps `Inactive` and `NoGuidance`
to the messages above.

### Session records

No new record type. An interrupted provider call leaves its `otto.provider_call`
settlement and an `otto.provider_recovery` record; an interrupted tool call
leaves its terminal operation fact; the delivered guidance is an ordinary user
message with its `input_id`. A reader of the transcript can tell an interrupted
step from a cancelled turn because the turn continues.

## Changes by crate

### otto-core

- `agent/steering.rs`: `State` gains `step: Option<CancellationToken>` and
  `interrupt_pending: bool`; `Run` gains `arm(&self, token)` and `disarm(&self)`;
  `Steering` gains `interrupt_step(&self) -> Result<usize, InterruptError>` and
  `pub enum InterruptError { Inactive, NoGuidance }`. `drain`, `take_or_close`
  and `withdraw` clear `interrupt_pending` as described. `messages()` is
  unchanged.
- New `agent/step_control.rs`: `StepControl<'a> { token, parent: &'a dyn OperationControl }`
  with the `OperationControl` impl described above and
  `fn interrupted(&self, turn: &dyn OperationControl) -> bool` (own token
  cancelled, `turn.admission_stop_reason()` is `None`).
- `agent/turn_run.rs`: arm and disarm around `dispatch_normal_provider_step`
  and each `run_tool_call`; the `Err(AgentError::Provider(Cancelled))` branch;
  `Event::StepInterrupted` before `deliver_steering` on both paths. If the file
  would cross 800 lines, the step body moves to a child module.
- `agent/tool_dispatch.rs`: skip progress observation when
  `execution.outcome.stop_reason.is_some()`.
- `agent/events.rs`: `Event::StepInterrupted { guidance: usize }`.
- `wire/events.rs`: type `step_interrupted`, field `queued_guidance: Option<u32>`.

### otto

- `app/steering.rs`: `Controller::interrupt_step`.
- `tui/backend.rs`: `Command::InterruptStep`; `backend/local_command.rs` and
  `backend/remote.rs` arms.
- `tui/app/keys.rs`: the `Esc` branch.
- `tui/app/events.rs`: `step_interrupted`.
- `tui/render.rs` (or where the composer title is built): the title text.
- `server/task_control.rs`: `interrupt_steering` handler; `server/mod.rs` route.
- `client/mod.rs`: `interrupt_steering(session_id) -> Result<u32, Error>`.
- `herdr.rs` has a wildcard arm and needs no change.

### Documentation

- `docs/manual/interactive-use.md`: the composer-title text, the `Esc` key
  table row (line 181), the attached-mode `Esc` bullet (line 237), the
  ordinary-guidance paragraph (lines 71-100).
- `docs/manual/servers-and-connectors.md`: the route row; the event type where
  SSE event types are listed.
- `CHANGELOG.md`: one entry under Unreleased.

## Tests

otto-core, new `agent/run_tests/step_interrupt.rs` with a `BlockingExecutor`
(waits on the control token, returns a cancelled/unknown execution with the
partial text) and the existing `CancellingProvider`:

1. Interrupt during a tool call with two tool calls in the batch: the first
   result has disposition `cancelled` and effect certainty `unknown`; the second
   has `not_started`; the guidance user message follows both tool results;
   `StepInterrupted { guidance: 1 }` is emitted once; the next provider request
   contains the guidance; the turn ends `Completed` with one `turn_end`.
   Without the change the first assertion fails: the executor is never
   cancelled and the test times out, or the guidance appears only after the
   tool result.
2. Interrupt during a provider call: no assistant message is appended for the
   interrupted call; the `otto.provider_call` record is `cancelled`; the
   guidance is appended; the turn completes. Without the change `run` returns
   `Err(Provider(Cancelled))`.
3. `interrupt_step` with no queued guidance returns `Err(NoGuidance)` and
   cancels nothing; with no active run returns `Err(Inactive)`.
4. Sticky interrupt: `interrupt_step` is called between steps; the next tool
   call is settled `not_started` without the executor being invoked; the
   guidance follows. Without the change the tool runs.
5. A turn cancel (turn token) during a tool call still ends the turn
   `Cancelled` and discards queued guidance (regression).
6. Progress guard: twelve interrupted tool results do not produce
   `AgentError::NoProgress`. Without the change the thirteenth step fails
   `RepeatedFailure` (`DEFAULT_CONSECUTIVE_FAILURES = 12`).

otto:

7. `tui/app/tests`: `Esc` while busy with `queued_steer` enqueues
   `Command::InterruptStep` and returns no action; without a queued steer it
   returns `Action::CancelTurn`; `Ctrl+C` returns `Action::CancelTurn` in both
   cases; the title reads `Esc delivers now` with queued guidance.
8. `tui/app/tests`: a `step_interrupted` event pushes the system line and a
   following `text_delta` starts a new assistant row.
9. `tui/backend/tests.rs`: `InterruptStep` routes to the controller locally
   and to `POST /v1/sessions/{id}/tasks/steer/interrupt` remotely.
10. `server/tests`: the route returns `409 no_queued_guidance` on an idle
    session and `404` for an unknown session (these run outside the command
    sandbox, which blocks local port binding).

## Out of scope

- ACP and chat connectors: they keep boundary delivery. They can call the HTTP
  route later.
- Keeping partially streamed reply text in history.
- A new `OperationStopReason` variant.
- otto-plus: it receives a new SSE event type; the browser client is a separate
  repository and is not changed here.
