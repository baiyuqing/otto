# Sub-agent context from an earlier turn

Status: approved 2026-10-18 with the recommended answers to all three open
questions.

This design lets a sub-agent start from the parent conversation as it was
before an earlier user prompt. It reuses the fork points of
[session fork at a turn](2026-10-06-session-fork.md), which lists "Forking a
sub-agent transcript" as a non-goal; this design does not change that
non-goal. It copies the parent's history into a new child; it does not fork
a child.

## Problem

Facts from the code at `bed8154e`.

- The `agent` tool's `context` input is `fresh` or `inherit`
  (`crates/otto/src/subagent/tools.rs`).
- `inherit` copies the parent's live message list, `Session::messages`
  (`crates/otto/src/runtime/wiring.rs:1103`), up to the assistant message
  that holds the `agent` call (`inherit_snapshot`,
  `crates/otto/src/subagent/inherit.rs:10`). The copy is appended to the
  child transcript before the child's first request
  (`crates/otto/src/subagent/runner.rs:1596`).
- The cut point is always the current call. A parent cannot leave out turns
  that do not apply to the delegated task, and it cannot give the child turns
  that compaction has already replaced with a summary.
- `fork_points` and `plan_fork` (`crates/otto-core/src/session/fork.rs`)
  already compute cut points that end at a turn boundary, over the entries of
  a session file. `build_context`
  (`crates/otto-core/src/session/context.rs:187`) turns kept entries into the
  message list a session opened from them would have.
- The model never sees entry ids. No tool returns them, and they are not sent
  to the provider. A model-facing input therefore cannot name a fork point by
  id.

## Decisions

- 2026-10-18: the parent model chooses the point through the `agent` tool.
- 2026-10-18: the model names the point by quoting the text of an earlier
  user prompt.

## Contract

### Input

The `agent` tool gains one optional string input, `fork_before`.

- **Allowed points.** The same fork points `/fork` offers: user prompts that
  open a `user`-trigger turn on the parent's active path, plus any user
  message in sessions written before turn records. Steering text and
  wake-turn prompts are not fork points. The prompt of the turn that is
  making the `agent` call is a fork point.
- **Matching.** Both sides are whitespace-trimmed. A fork point matches when
  `fork_before` equals its full prompt text or the first line of that text.
  Exactly one fork point must match.
- **Context.** A non-empty `fork_before` sets `context` to `inherit`.
  `fork_before` together with an explicit `context: "fresh"` in the same call
  is rejected. A named definition whose default is `fresh` does not block
  `fork_before`; the call-level input takes precedence, as `context` does
  today.
- **Resume.** `fork_before` must be absent or empty when `resume` is set, like
  `agent`, `model`, `name` and `description`.

### What the child receives

The child receives `ResolvedContext.messages` built from the parent session
file's active path, cut before the `otto.turn` start of the matched prompt's
turn (`plan_fork` with that prompt's entry id). This is the same message list
a session created by `/fork` at that point opens with.

- **Compaction after the point.** The child receives the original messages,
  not the summary that later replaced them.
- **Compaction before the point.** The child receives the summary and the
  retained messages that were in effect at that point.
- **Provider-managed checkpoints and `context_edit`.** These are handled as
  `inherit` handles them today: the copy is the unmodified message list, not
  the model projection.

The copy is appended to the child transcript exactly as the `inherit`
snapshot is today. A resumed task replays nothing, because its transcript
already holds the copy.

### Errors

Each error is returned as the `agent` tool's error result, and no task is
created.

| Condition | Error text |
| --- | --- |
| No session file: `--no-session`, or a lazy session before its first write | `fork_before: needs a saved session` |
| `fork_before` with `context: "fresh"` | `fork_before requires context "inherit"` |
| No fork point matches | `fork_before: no user prompt matches; prompts that can be used, newest first:` followed by the listing |
| More than one fork point matches | `fork_before: <n> user prompts match; prompts that can be used, newest first:` followed by the listing |
| The session file cannot be read or resolved | `fork_before: <error>` |

The listing has one line per fork point, `- <first line>`, newest first, at
most 20 lines. A first line longer than 120 characters is cut to 120 and
ends with `…`. When points are left out, a last line `and <n> more`
follows; a session with no fork points lists `(none)`. This keeps the
listing under 3 KiB. A prompt whose text occurs more
than once with the same first line cannot be selected; the model can use
`context: "inherit"` or a fresh prompt instead.

## Design

| Area | Change |
| --- | --- |
| `crates/otto-core/src/session/fork.rs` | `fork_point_by_prompt(points, text)`: the matching rule above, I/O-free; returns the point or a no-match/ambiguous error carrying the listing. |
| `crates/otto/src/session/repository.rs`, `crates/otto/src/storage/backends/posix/session/` | `fork_context(reference, prompt) -> Result<Vec<Message>, String>`: reads entries once with the read-only `Store::read_entries`, then `fork_points`, `fork_point_by_prompt`, `plan_fork`, `build_context`. The source file and its lock are not touched. |
| `crates/otto/src/subagent/runner.rs` | `StartRequest.fork_before`; `Config.parent_fork: Option<ParentFork>` where `ParentFork = Arc<dyn Fn(&str) -> Result<Vec<Message>, String> + Send + Sync>`; `start_resolved_with_deadline` uses it in place of `inherit_snapshot` when `fork_before` is set; new `StartError` variants for the errors above. |
| `crates/otto/src/runtime/wiring.rs` | Builds `parent_fork` from the parent session's path and the session repository; `None` with `--no-session`. |
| `crates/otto/src/subagent/tools.rs` | `fork_before` parameter and description; `AGENT_CONTEXT_DESCRIPTION` and `AGENT_RESUME_DESCRIPTION` updated; resume rejects `fork_before`. |
| `crates/otto/tests/testdata/tool_definitions.jsonl` | Regenerated `agent` definition. |
| Docs | `docs/manual/safety-and-tools.md` sub-agents section; `CHANGELOG.md`. |

Parameter description for `fork_before`:

> Text of an earlier user prompt in this conversation that started a turn:
> its full text or its first line. The sub-agent receives this conversation
> as it was just before that prompt, including turns that compaction has
> since summarized, and context becomes inherit. Exactly one prompt must
> match; otherwise the error lists the prompts that can be used.

## Non-goals

- Naming a point by entry id, turn number or relative offset. Entry ids are
  not visible to the model, and the model's own turn count differs from the
  runtime's after compaction and wake turns.
- A user-facing way to choose the point (TUI, HTTP, ACP).
- Forking a child's transcript, or a child forking from its parent on its
  own.
- Durable workflow steps: they never set `fork_before`.
- A new task record field. The child transcript holds the copied prefix.

## Acceptance

Focused tests; all offline.

- **Same as `/fork`.** For a parent with three turns, `fork_before` set to the
  second turn's prompt gives the child the same messages as
  `build_context` over the entries `plan_fork` keeps before that prompt.
  Fails if the cut is taken from the live list or lands one turn off.
- **Compaction after the point.** With a compaction checkpoint after the
  chosen turn, the child receives the original first-turn messages and no
  summary message.
- **Compaction before the point.** With a checkpoint before the chosen turn,
  the child receives the summary and the retained messages.
- **Matching.** Full text and first line both match; leading and trailing
  whitespace is ignored; a steering message and a wake-turn prompt do not
  match; two equal prompts return the ambiguous error; the listing is newest
  first and bounded.
- **Context interplay.** `fork_before` without `context` records the task's
  context as `inherit`; with `context: "fresh"` it is rejected and no task is
  created; with `resume` it is rejected.
- **No session.** A runner without `parent_fork` rejects `fork_before`.
- **Source unchanged.** The parent session file is byte-identical before and
  after the child starts.

## Open questions

1. **First-line matching.** Recommended: yes. The error listing shows first
   lines, so a listed line must be accepted as input. Without it, a prompt
   whose full text was compacted away cannot be selected.
2. **Ambiguous matches.** Recommended: reject. Choosing the newest match
   would cut at a point the model may not have meant, with no error.
3. **Recording the point.** Recommended: no new `otto.task_spec` field. Add
   one when a consumer (trace view, task details) needs it.
