# Durable turn queue

Status: approved 2026-10-07 with the contract change below and the
recommended answers to all three open questions.

This design closes the last durable-input gap that
[durable inputs](2026-10-06-durable-inputs.md) left as a non-goal: the
`otto serve` and local ACP turn queues. It reuses that design's mailbox
journal and input ids.

## Problem

Facts from the code at `e065d23`.

- **Two queues hold prompts that wait for the active turn.**
  - **Serve.** `POST /v1/sessions/{id}/turns` with `"queue": true` appends a
    `QueuedTurn` to `SessionState.queue`, at most 16 per session.
    - It carries text, an optional image, channel, origin and `input_id`.
    - The attached TUI and attached ACP always queue this way.
  - **Local ACP.** `/queue <prompt>` appends a `QueuedPrompt` to the
    connection's `PromptState.queue`, at most 16.
    - It carries text, channel and origin.
    - The request is acknowledged at once, and the answer arrives later as
      session updates.
- **Both queues are memory only.**
  - **Process loss.** On a crash, `kill -9` or OOM, every waiting prompt is
    lost without a trace.
  - **SIGTERM.** Serve's graceful shutdown ends queued turns `canceled`, and
    the user manual says so.
  - **Closing the connection.** Closing a local ACP connection clears its
    queue.
- **A client cannot fully recover by itself.**
  - `input_id` makes a resubmission safe, but only a client that kept its own
    copy can resubmit.
  - The attached TUI and the chat connector do not keep one after their
    request is accepted.
- **Every other accepted input is already durable:** steering, `task_queue`
  follow-ups, sub-agent messages and notifications (durable inputs,
  crash-recovery designs).

## Goals

1. A prompt accepted into a queue runs exactly once, in FIFO order, even if
   the Otto process stops before it starts.
2. A restored queue runs only when a client opens the session again: no
   unattended provider spending at startup, as in crash recovery.
3. It runs after the interrupted turn's automatic resume, if there is one.
4. Explicit cancellation still removes a prompt for good.
5. Admission reports a prompt that could not be saved, instead of accepting
   it and losing it later.
6. Clients keep their handles: a restored serve turn keeps its turn id, so
   `GET /turns/{id}` and `input_id` retries keep working.

## Non-goals

- **The local TUI's single waiting prompt** (`queued_input`). The user is at
  that terminal and sees it.
- **The chat connector's own queue.** It is a separate Go process and a
  client of Otto; it already sends `input_id`, so its resubmissions are safe.
- **Reopening sessions at `otto serve` startup.** This stays a non-goal, as
  in crash recovery.
- **Moving a queue to another session**, or merging queues.

## Change to an approved contract

[Active-task guidance](2026-10-04-active-task-guidance.md) says: "Queue
storage remains in memory; disconnect/shutdown must not silently restart
requests in a different session." The durable-inputs design kept that
contract as a non-goal.

**Becomes:** queue storage is the session's `<id>.turns.json` journal.

- A restored prompt runs in **its own session only**, and only after a client
  opens that session. A notice names how many prompts are resuming.
- The "never in a different session" half of the contract is unchanged.

The user manual lines "The `otto serve` and ACP turn queues remain in memory"
and "On shutdown, queued turns end `canceled` without starting" change to
match (see open question 1).

## Design

### Journal `<id>.turns.json`

- **Storage.** A per-session mailbox journal from `Builder::session_journal`,
  the same sidecar adapter as `inputs.json` and `followups.json`. It is
  written atomically on every change.
- **Entries.** One entry per waiting prompt, in FIFO order.
  - **Fields:**
    - `id`: the prompt's input id;
    - `kind`: `queued_turn`;
    - the prompt text;
    - channel and origin;
    - `turnId` (serve only);
    - the producer, `serve` or `acp`.
  - **Input id.** The client's `input_id` when it gave one. Otherwise it is
    `queued:<turn id>` (serve) or `queued:<random>` (ACP).
  - **Images.** An image is not saved. See open question 2.
- **Admission.**
  - A prompt is accepted into the queue only after its entry is saved.
  - If the save fails, nothing is queued:
    - serve answers `503 input_not_persisted`;
    - ACP answers "the prompt could not be saved; nothing was queued."
  - The existing limits (16) and the `409 queue_full` / `turn_active` /
    `duplicate_input` answers are unchanged.
- **Exactly once.**
  - **Starting a prompt.** When a queued prompt starts, its input id becomes
    the user message's input id (`set_next_input_id`, as today).
  - **Removing the entry.** The entry is removed only after that user message
    is in the session file.
  - **On restore.**
    - An entry whose input id is already in the transcript (`has_input`)
      already started, so it is removed and not run again.
    - A prompt that had started counts as part of the interrupted turn, which
      turn resume handles.
- **Removal for good.** These remove an entry without running it:
  - serve `POST /turns/{id}/cancel`;
  - the attached TUI's withdraw;
  - ACP `session/cancel` (which clears every queued prompt, as today);
  - `DELETE /v1/sessions/{id}`;
  - archiving the session.
- **Kept on disk.**
  - Process loss always keeps entries.
  - Graceful shutdown keeps them too, depending on open question 1.

### Restore

- **When.** When a frontend opens a session whose `turns.json` has entries:
  - serve `resume_or_create`;
  - local ACP `session/load`;
  - the TUI's `/resume` or `--resume`.
- **Restore steps.**
  1. Entries whose input id is in the transcript are removed.
  2. The rest become queued prompts again, in order.
     - Serve rebuilds each `QueuedTurn` with its original turn id, status
       `queued`, and adds it to the remembered `input_id` map.
     - Local ACP rebuilds its `QueuedPrompt`s.
  3. A notice is pushed: "Resuming N queued prompts saved before Otto
     stopped."
- **Order.** The interrupted turn's recovery wake turn, if any, runs first.
  Today a wake turn refuses to start while the queue is non-empty, so restored
  prompts are held until the recovery turn has started. After that, the queue
  drains as usual: approval pending still blocks it, and a running turn is
  still waited for.
- **TUI.** The TUI has no turn queue. It runs restored prompts one after
  another, in order, after its recovery turn, through the same path as a
  queued follow-up. Each is shown as a user message.
- **`--no-session`.** No journal and no restore; the behaviour is unchanged.

### Surfaces

- **HTTP.**
  - `GET /turns/{id}` for a restored turn answers `queued` again.
  - The status stream's `queued` count includes restored turns.
  - A queued turn's SSE still sends nothing until the turn starts.
- **ACP.** A restored prompt's answer arrives as session updates on the
  connection that loaded the session, as for `/queue` today.
- **Docs.** The user manual's turn queue, ACP `/queue` and Sessions sections
  name the journal and the restore behaviour.

## Compatibility

- **Older Otto** never reads `turns.json`. Its entries wait until a newer
  Otto opens the session.
- **Archiving** removes the file with the other sidecars.
- **Lease takeover.** The new owner restores the file like any reopen.

## Ownership

| Area | Change |
| --- | --- |
| `crates/otto/src/runtime/` | `turns.json` journal wiring, `queued_turn` entries |
| `crates/otto/src/app/` | restore on open, recovery-first ordering, TUI run path, removal helpers |
| `crates/otto/src/server/` | save on admission, remove on start/cancel/delete, restore with turn ids, `503 input_not_persisted` |
| `crates/otto/src/acp/` | local `/queue` save/remove/restore on `session/load`, `session/cancel` clears |
| `crates/otto/src/storage/backends/posix/session/` | archive removes `turns.json` |
| docs | user manual, active-task guidance and durable-inputs contract notes, development.md, CHANGELOG |

## Acceptance

- **Serve.**
  - Two queued turns survive process loss. After a reopen they run once each,
    in order, after the recovery turn, with their original turn ids, and
    `input_id` retries return those turns.
  - A queued turn whose user message reached the transcript before the crash
    is not run again.
  - A cancelled queued turn is gone after a restart.
  - A failed save answers `503` and queues nothing.
- **Local ACP.** A `/queue` prompt survives the process. `session/load` in a
  new process runs it once, and its answer arrives as session updates.
  `session/cancel` leaves nothing to restore.
- **TUI.** Prompts queued by an attached client run after a local `--resume`.
- **Spending.** Opening nothing runs nothing: no provider request happens
  until a client opens the session.
- **Graceful shutdown.** It follows the answer to open question 1.

## Resolved questions

1. **Graceful shutdown (SIGTERM).** Today queued turns end `canceled`. Keep
   them for the next start instead?
   - Recommended: keep them. A restart for an upgrade, or lease migration, is
     the most common way a queue is lost today.
   - Live clients still see the connection end; the turn reappears as
     `queued` after the restart.
   - The running turn is still cancelled as today.
2. **Queued prompts with an image.** Saving a 10 MB image would rewrite up to
   about 200 MB on every queue change.
   - Recommended: save the text only.
   - On restore, such a prompt still runs, with a notice that its image was
     not kept.
   - The alternative is to keep image turns in memory only and report them
     lost.
3. **Notice wording and placement.** Recommended: one context notice in the
   session, the same mechanism the recovery notice uses, so every client sees
   it and the model knows the prompts are resumed rather than fresh.
