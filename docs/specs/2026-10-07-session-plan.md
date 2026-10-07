# Session plan

Status: approved 2026-10-07 with the recommended answers to all three open
questions.

This design closes the "structured todo/plan state as session `custom`
records" item listed under "Later slices" in
[crash recovery and interrupted-turn resume](2026-10-06-crash-recovery-and-turn-resume.md).
It follows [session fork](2026-10-06-session-fork.md), whose copies keep
every record before the fork point.

## Problem

Facts from the code at `374dc8f`.

- The model has no structured place for a multi-step plan. Today a plan can
  only exist in two forms:
  - **Prose in an assistant message.** A compaction summarizes it away, and no
    client can show it as a list.
  - **`task_queue` follow-ups.** These queue prompts to run later. They are
    not a checklist the user can follow.
- Clients cannot show progress through a task.
  - ACP defines `SessionUpdate::Plan` (stable in the pinned
    `agent-client-protocol-schema` 1.10.2). Editors such as Zed render it as a
    checklist. Otto never sends it.
  - HTTP and the TUI have nothing equivalent.
- A crash, a resume or a fork keeps the transcript. A plan held only in prose
  is not kept in any form the model or a client can reliably read back.

## Goals

1. Give the model one tool to state and update a short plan: steps, each
   `pending`, `in_progress` or `completed`.
2. Store the plan in the session journal, so it is:
   - atomic with the transcript;
   - kept across a crash, a reopen, turn resume and a fork;
   - kept by the model after compaction.
3. Show the plan where sessions are shown:
   - ACP `plan` updates;
   - HTTP session object and SSE;
   - TUI.
4. Turn it on by default, with the descriptions and policy text the
   [capability description](../development.md#capability-descriptions)
   rules require.

## Non-goals

- **Executing the plan.** The tool records intent and progress only. It
  starts nothing, unlike `task_queue` or workflows.
- **Editing the plan from a client.** Clients only display it.
- **ACP `PlanUpdate` / `PlanRemoved`.** Both sit behind the
  `unstable_plan_operations` feature. The stable `Plan` update replaces the
  whole plan, which is all this design needs.
- **Plans for sub-agents.** See open question 2.

## Design

### Tool `update_plan`

- **Input.**
  - `steps`: an array of 0 to 20 items, each `{text, status}`.
    - `text` is 1 to 200 characters on one line.
    - `status` is `pending`, `in_progress` or `completed`.
  - At most one step may be `in_progress`.
  - An empty array clears the plan.
- **Effect.** Replaces the whole plan. There are no partial edits, so a retry
  of the same call has the same result. The retry class is `Idempotent`.
- **Output.**
  - Success: `Plan updated: 2 of 5 completed; in progress: <text>.`
  - Invalid input: an error result naming the rule broken, and the plan is
    unchanged.
- **Description.**
  - Says to use it for work of three or more steps, to keep exactly one step
    `in_progress` while working, and to mark steps `completed` as soon as
    they are done.
  - Says it is not for single-step requests.
  - Separates it from `task_queue` (which runs later prompts) and from
    `request_approval` (which asks the user to approve an approach).
- **Policy section.** A short policy section in `system_prompt_for` says the
  same. It is rendered only when the tool is registered.

### Record `otto.plan`

The tool appends one custom entry through `Session::append_custom`:

```json
{"schemaVersion":1,"steps":[{"text":"…","status":"in_progress"}]}
```

- **Folding.** `build_context` folds the latest valid `otto.plan` on the
  active path into `ResolvedContext::plan`.
  - It folds before compaction narrows the path, as it already does for
    `otto.capabilities`.
  - A malformed record adds a warning and is skipped.
  - A newer schema version is skipped.
- **Reading it back.** `Session::plan()` is a required trait method with no
  default body.
  - Every implementation and wrapper must implement it: `MemorySession`,
    `Store`, `MemoryHandle`, `SharedSession`, `SharedTranscript` and the
    test doubles.
  - Conformance gains `session.plan` (latest record wins, malformed ignored)
    and a check that the plan survives a reopen.
  - A wrapper that forgets to forward the method then fails a named check
    instead of hiding the plan.
- **Fork and resume.**
  - A fork keeps the latest plan before its cut, because fork copies records
    unchanged.
  - A resumed turn reads the same plan.

### Model context

- **Before compaction.** The model already sees its plan as the
  `update_plan` call in the transcript. Nothing is added to each request, so
  the system prompt stays stable and provider prompt caching keeps working.
- **After compaction.** A compaction appends the current plan to the summary
  as a `<plan>` block, next to the existing `<capability-updates>` block in
  `summary_details`. That keeps the plan in context after the call that set
  it has been summarized away. An empty plan adds no block.

### Surfaces

- **Live event.**
  - A successful call emits an agent event `plan_updated` carrying the
    steps.
  - It reaches the wire as `WireEvent.steps`. The field is named `steps`
    because `WireEvent.plan` already means the compaction plan.
- **ACP.**
  - On `plan_updated`, Otto sends `SessionUpdate::Plan` with every step at
    priority `medium`.
  - `session/load` sends the current plan after replaying history, when the
    plan is non-empty.
- **HTTP.**
  - SSE carries the `plan_updated` frame.
  - The session object gains `plan: [{text, status}]`, omitted when the plan
    is empty.
  - OpenAPI is updated.
- **TUI.**
  - One line above the footer: `Plan 2/5 · <in-progress step>`. It is shown
    while a plan exists and dropped first when the screen is short, like the
    sub-agent panel.
  - `/plan` shows the whole plan as a document.
  - Both work when attached to `otto serve`, through the session object and
    the SSE frame.

## Compatibility

- **Older Otto versions** skip the unknown `otto.plan` custom entry.
- **Pi** reads it as an ordinary custom entry.
- **Sessions without a plan** behave exactly as before.
- **Tool lists.**
  - A resumed session registers the new tool.
  - The existing `capability_update` notice tells the model about it.
- **Tool name.** `update_plan` passes `safe_prompt_tool_name`.

## Ownership

| Area | Change |
| --- | --- |
| `crates/otto-core/src/session/` | `plan.rs` (record codec, validation, fold), `ResolvedContext::plan`, `Session::plan`, `MemorySession` |
| `crates/otto-core/src/conformance.rs` | `session.plan`, reopen check |
| `crates/otto-core/src/agent/` | `plan_updated` event, `<plan>` compaction block |
| `crates/otto/src/tool/` | `plan.rs` tool, `BASE_SAFETY`, golden definitions |
| `crates/otto/src/runtime/` | registration with the session, policy section and prompt test, wrapper forwarding |
| `crates/otto/src/acp/`, `server/`, `client/`, `tui/` | ACP plan updates and replay, session field, SSE, TUI line and `/plan` |
| docs | user manual (tool, `/plan`, HTTP, ACP), development.md, CHANGELOG |

## Acceptance

- **Tool validation.**
  - Valid input replaces the plan and returns the summary line.
  - These are rejected with the plan unchanged: more than 20 steps, text
    that is empty, longer than 200 characters or multi-line, two
    `in_progress` steps, and an unknown status.
- **Durability.**
  - The latest record wins.
  - The plan survives a reopen of the POSIX `Store`.
  - The conformance checks pass for every implementation.
  - A fork before the plan has no plan; a fork after it keeps it.
- **Compaction.**
  - The summary carries a `<plan>` block with the current steps.
  - An empty plan adds no block.
- **Policy.** The policy section is present when the tool is registered and
  absent when it is not.
- **ACP.**
  - A tool call sends a `plan` update.
  - `session/load` replays the current plan.
- **HTTP.** The SSE `plan_updated` frame and the session `plan` field.
- **TUI.** The plan line, `/plan`, and the attached path.

## Resolved questions

1. **Tool name.**
   - Recommended: `update_plan`. It says what the call does, and agents
     already use this name for the same purpose.
   - The alternative is `todo_write`.
2. **Sub-agents.**
   - Recommended: parent only, listed in `EXCLUDED_CHILD_TOOLS`.
   - Child transcripts are not shown in ACP or the TUI plan line, so a
     child's plan would be invisible, and children already get one delegated
     task.
3. **Re-inject the plan every request, or only through compaction?**
   - Recommended: only through compaction, as described above.
   - Injecting it into each request would change the system prompt on every
     plan update and defeat provider prompt caching. The transcript already
     carries the plan until a compaction.
