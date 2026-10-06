# Session fork at a turn

Status: approved 2026-10-06 with the recommended answers to all three open
questions.

This design closes the "session fork at an entry" item listed under "Later
slices" in [crash recovery and interrupted-turn resume](2026-10-06-crash-recovery-and-turn-resume.md).
It follows [workflow time travel](2026-09-21-workflow-time-travel-design.md),
whose `fork` creates a new run from a committed boundary and leaves the source
untouched.

## Problem

Facts from the code at `5191e9f`.

- A session is a Pi v3 JSONL file at
  `<session_root>/<workspace_key>/<id>.jsonl`. Every entry has an `id` and a
  `parentId`, so the format is a tree. However, Otto always appends onto the
  last entry (`store.rs` `state.leaf_id`), so in practice each session is one
  line of history.
- To try a different direction from an earlier point, a user has two options
  today:
  - start a new session and lose the context;
  - keep going and carry the unwanted turns in context.
- No session copy exists anywhere. The only forks are workflow runs
  (`POST /v1/workflows/{id}/fork`) and workspace identity (`otto workspace fork`).
- Entry ids reach clients as `Message.id`:
  - on the HTTP `/history` route and in the TUI;
  - not over ACP.
  - Some ids are synthetic: retained-tail messages are `<id>-tail-N`, and
    repaired tool results also get made-up ids.
- An arbitrary cut point is unsafe:
  - **Mid-turn cut.** A cut inside a turn leaves an `otto.turn` start with no
    end. Opening the copy would then report an interrupted turn and resume
    it on its own, spending provider calls the user never asked for.
  - **Cut after a tool call.** A cut between an assistant tool call and its
    result leaves a dangling call. On open, repair replaces it with a
    synthetic result.

## Goals

1. Let a user create a new session whose history is the source session's
   history up to just before a chosen user prompt, then continue it
   independently.
2. Never change the source session.
3. Make the copy a valid session that opens like any other:
   - no interrupted turn;
   - no dangling tool calls;
   - context identical to the source's context at that point, compaction
     included.
4. Offer it where sessions are already managed:
   - over HTTP, which otto-plus and `--attach` use;
   - in the TUI.

## Non-goals

- **Branching inside one file.** Otto keeps one line of history per file. A
  fork is a new file.
- **Tree navigation, labels or branch summaries.** No `/tree` command, and
  no `label` or `branch_summary` writes.
- **Copying sidecars.** Inbox, reminders, inputs and follow-ups belong to the
  source session; queued work stays there.
- **Copying sub-agent children.** Child transcripts and task rows stay with
  the source. Earlier tool results that name a task id remain as text.
- **Forking a sub-agent transcript.**
- **ACP fork.** See open question 2.
- **A CLI flag.** The TUI and HTTP cover interactive use.

## Design

### Fork points

A fork point is named by the id of a user message on the source's active path.

- **The kept history is everything before that message's turn.** The copy keeps
  every entry on the active path before the `otto.turn` start record of the
  turn that the message opens.
  - The turn's start record, its messages and its end record are not copied.
  - So the copy ends at a turn boundary: no interrupted turn and no dangling
    tool call.
- **Only turn-opening messages are allowed.** The message must be the first
  message after a `user`-trigger `otto.turn` start. Steering text delivered
  inside a turn is not a fork point.
- **Sessions without turn records.** Sessions written before `otto.turn`
  existed have no start records. For them, any user message on the active
  path is a fork point when the history before it has no assistant tool call
  without a result. The cut is just before the message.
- **Omitted fork point.** The copy keeps everything up to the last turn
  boundary. A running turn, or an interrupted one, is left out.
- **Rejected requests.** The request is rejected (HTTP 409 `invalid_fork_point`)
  when the id is:
  - not on the active path;
  - synthetic (`-tail-N`, a repaired result);
  - a summary created by compaction;
  - not turn-opening.

### Contents of the copy

- **Header.**
  - A new random id.
  - The source's `cwd`.
  - A new timestamp.
  - `parentSession` set to the source file's path. Pi uses this field for the
    same purpose. Otto already writes it for child transcripts, which live
    in a different directory.
- **Entries.** The kept entries are copied byte for byte, with their ids and
  `parentId`s unchanged. That covers:
  - the source's `otto.runtime` root, so profile, model and effort restore as
    on `--resume`;
  - compaction checkpoints, whose `firstKeptEntryId` always lies earlier on
    the path;
  - `context_edit` targets;
  - operation facts and capability records;
  - input ids in message details.

  Keeping ids keeps every reference valid without rewriting.
- **Fork record.** One `otto.fork` custom entry is appended after the copied
  prefix:

  ```json
  {"schemaVersion":1,"sourceSessionId":"…","sourceEntryId":"…","prompt":"…"}
  ```

  - `prompt` is the text of the message forked at, so a client can put it in
    the editor. It is absent when no fork point was given.
  - The record never enters model context.
- **Name.** `<source name> (fork)` when the source has a name; otherwise no
  name.
- **Input ids.** A retry of an `input_id` already delivered before the fork
  point is deduplicated in the copy. This is correct, because that input is
  in the copy's history.

### Writing

`SessionRepository::fork(source, before: Option<&str>) -> Result<Prepared>`
reads the source and writes the copy in one go:

1. Read the source's committed entries with the existing read-only
   `read_entries`, so an open source and its lock are not disturbed.
2. Resolve the active path and the cut. Validate the fork point.
3. Write the header and kept entries to a temporary file in the session
   directory, fsync it, and `rename_excl` it to `<new id>.jsonl`.
   - A crash leaves either no copy or a whole one.
   - The temporary name does not end in `.jsonl`, so `list` ignores a
     leftover.
4. Open the copy as a normal `Prepared`.

### Surfaces

- **HTTP.** `GET /v1/sessions/{id}/fork-points` lists the fork points, oldest
  first, as `{entry_id, prompt}`; the attached TUI's picker reads it.
  `POST /v1/sessions/{id}/fork` with body `{"before": "<message id>"}`;
  `before` is optional.
  - Success: `201` with the new session object, plus `forked_from:
    {session_id, entry_id}` and `prompt`.
  - Unknown source: `404`. Invalid fork point: `409`.
  - The session object gains an optional `forked_from`, read from the
    `otto.fork` record.
  - OpenAPI and `testdata/server/openapi.yaml` are updated.
- **TUI.** `/fork` opens a picker of the current session's fork points,
  newest first. Each row shows the prompt's first line.
  - Choosing one creates the fork, switches to it and puts the prompt in the
    editor.
  - `/fork` with no fork points says so.
  - It works the same when attached to `otto serve`, through the HTTP route.
- **Session details.** `/session` and the session list show "forked from
  <name or id>".

## Compatibility

- **Older Otto versions.** They read a forked file as an ordinary session:
  - `parentSession` is kept but not interpreted;
  - an unknown `otto.fork` custom entry is skipped.
- **Pi.** Pi reads the file as a session with a parent.
- **The source.** It is never written.
- **Other features.** No existing route, flag or command changes.

## Ownership

| Area | Change |
| --- | --- |
| `crates/otto-core/src/session/` | fork-point resolution over entries (`fork.rs`), `otto.fork` record codec, `forked_from` fold |
| `crates/otto/src/storage/backends/posix/session/` | `fork` writer (temp file, fsync, `rename_excl`), list ignores temp files |
| `crates/otto/src/session/repository.rs` | `fork` on `SessionRepository` |
| `crates/otto/src/server/` | route, handler, session object field, OpenAPI |
| `crates/otto/src/app/`, `crates/otto/src/tui/` | controller `fork_session`, `/fork` picker, editor prefill, attached path |
| docs | user manual (Sessions, HTTP API), development.md session contract, CHANGELOG |

## Acceptance

- **The copy.** A fork before turn N has the same model context as the
  source had before turn N:
  - with and without a compaction before the cut;
  - with a `context_edit` before the cut.
- **Clean open.**
  - The copy reports no interrupted turn and no dangling tool call.
  - Opening it starts no turn.
- **Source unchanged.** The source file is byte-identical before and after.
- **Rejected fork points.** These all return `409` and create no file:
  - a steering message;
  - a synthetic or compaction-summary id;
  - an id off the active path.
- **Omitted fork point.** On a session whose last turn was interrupted, the
  copy ends before that turn.
- **Older sessions.** A session without turn records forks before any user
  message that has no dangling call before it.
- **Crash safety.** A failure before the rename leaves no `.jsonl` and nothing
  in `list`.
- **Surfaces.**
  - The HTTP route returns `forked_from` and `prompt`.
  - TUI `/fork` switches to the copy with the prompt in the editor, both
    local and attached.
- **Compatibility.** A forked file opens with the `otto.fork` record ignored
  by context.

## Resolved questions

1. **Keep the turn, or cut before it?** Should a fork point mean "before this
   prompt" (re-ask it differently) or "after this turn" (keep the answer)?
   - Recommended: "before". It matches Pi's `/fork` and supports editing a
     prompt.
   - "After this turn" is already reachable by passing the next turn's
     prompt, or by omitting `before`.
2. **ACP.** `agent-client-protocol-schema` 1.10.2 has `session/fork` behind
   `unstable_session_fork`, with no fork-point field.
   - Recommended: leave ACP out of this slice. The method is unstable, and a
     whole-session copy is all it could express without `_meta`.
3. **`otto.fork` record or header only?** Recommended: write both.
   - `parentSession` keeps Pi compatibility.
   - The custom record carries the source entry and prompt, which the header
     cannot hold, and is visible to the existing custom-record folds.
