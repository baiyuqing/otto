# Trace view: a timeline of turns, provider requests, tool calls and sub-agents

Status: approved 2026-10-11.

User decisions, 2026-10-11:

- The view covers three cases: what a running turn is doing, what a
  finished turn did, and which turn, tool call or notification started
  which sub-agent or wake turn.
- The view is graphical: rows on a shared time axis.
- Periodic refresh is acceptable; no event forwarding is required.
- The entry is `/trace`. There is no key binding.
- The scope is the current session and its sub-agents.
- Per-request input is recorded as part token estimates and memory recall
  record ids, not as recall text (option (a) of the earlier proposal).
- An approval that opens on its own must not cover `/trace`.

## Problem

Most of the data needed to follow a turn is produced, but the TUI shows
only part of it:

| Data | Where it exists | What the TUI shows |
| --- | --- | --- |
| Provider request duration, attempts, status | `Event::ProviderApiCall` (`otto-core/src/agent/events.rs`) | Nothing. `wire/events.rs` drops the event, so neither the local nor the attached TUI receives it. |
| Provider retries | `Event::ProviderRetry` | The status line, while the retry waits |
| Time to first output, idle time | `~/.otto/logs/diagnostic-*.jsonl` | Nothing; readable with `rg` only |
| Tool call start and end | `otto.operation` session entries | Folded call and result; no duration |
| Per-request token usage | Assistant message `usage` | Session totals in the footer only |
| Sub-agent internals | The child's session file | One `/agents` row: last 500 bytes of text, last tool name, step and token counts (`subagent/runner.rs`, `ChildProgress`) |
| Which notification started a wake turn | Session messages with role `context` and `contextMetadata.taskId` | The notification text in the transcript |

The session file does not contain provider request timing. The usage
database's `provider_api_operation` record has no duration. The diagnostics
JSONL has it, but rotates at 5 MiB, is removed after seven days, contains
no content, and is written on the serve host in attach mode.

## Design

The trace is built from session entries only. The same code produces the
view for a running turn and for a turn read back after a restart. A
running view is refreshed by re-reading, not by forwarding events.

### 1. Session record `otto.provider_call`

The agent loop appends one custom entry after each provider operation
settles, at the point where it emits `Event::ProviderApiCall`
(`Agent::emit_provider_api_call`). It covers `ordinary`,
`local_compaction` and `remote_compaction` operations.

```json
{
  "schemaVersion": 1,
  "operationId": "op_7",
  "kind": "ordinary",
  "startedAt": 1791700000000,
  "durationMs": 7900,
  "firstOutputMs": 4400,
  "attempts": 2,
  "retries": [{
    "attempt": 1, "maxAttempts": 3, "delayMs": 2000,
    "reason": "connection interrupted", "interruptedOutput": false,
    "atMs": 1791700001800
  }],
  "disposition": "succeeded",
  "effectCertainty": "completed",
  "finishReason": "tool_calls",
  "usage": { "input_tokens": 14000, "output_tokens": 402, "cached_input_tokens": 11200 },
  "request": {
    "systemPromptTokens": 6100,
    "toolTokens": 6300,
    "messageTokens": 1200,
    "memoryTokens": 400,
    "memoryRecordIds": ["m_12", "m_40"]
  }
}
```

- `startedAt` and `retries[].atMs` are unix milliseconds. The disposition
  fields are the flattened `OperationOutcome`; `stopReason` appears when
  the operation stopped early. `usage` uses the `Usage` field names.
- `firstOutputMs` is measured from the start to the first decoded model
  output event (reasoning, text or tool call). It is absent when no output
  arrived.
- `retries[].reason` is the reason text that `ProviderRetry` already
  carries and that the status line already shows.
- `usage` is copied from the provider report so that the trace does not
  need to match entries to assistant messages. It is absent when the
  provider reported no usage, and `cached_input_tokens` is absent when it
  is zero.
- `request` token counts are estimates from the estimators `/context` uses
  (`context_report::sections`). The recalled memory message is counted in
  `memoryTokens`, not in `messageTokens`. `memoryRecordIds` are the ids of
  the `agent::memory::Record` values recalled for the turn.
- The entry holds no prompt, message, tool argument or result text.
  An entry is about 400 bytes.
- An append failure is handled the same way as an `otto.operation` append
  failure.
- `otto.provider_recovery` is still written for retried or failed
  requests; its retries now carry `atMs`.
- Compatibility: the session codec keeps unknown entries unchanged
  (`decode_pi_v3_preserves_unknown_entry_raw_json`). Older Otto versions
  ignore the entry.

### 2. Sub-agent link: `otto.task_spec` gains `parentToolCallId`

`AgentTool` implements `Tool::execute_call` so that it receives
`ToolCall::tool_call_id`, and passes it in `StartRequest`. The runner
writes it into `TaskSpecData` as an optional `parentToolCallId` (serde
default, skipped when empty). The child's session file already starts with
this entry, so `tasks.db` needs no new column and no migration.

A child file without the field, written before this change, is placed
under the parent turn whose time range contains the child's first entry.

### 3. Wake triggers: no new record

`Agent::deliver_notifications` already appends each delivered
notification as a session message with role `context`, `createdAt` and
`contextMetadata.taskId`. The trace links a turn to the tasks whose
notifications it delivered from these messages.

### 4. Trace builder in `otto-core`

`otto-core` adds `session::trace`. It performs no I/O and builds for
`wasm32-unknown-unknown`, so otto-plus can deserialize the same type.

```rust
pub struct Trace {
    pub session_id: String,
    pub turns: Vec<Span>,
}

pub struct Span {
    pub id: String,                // turn id, operation id, or task id
    pub kind: SpanKind,            // turn | provider | tool | compaction | subagent
    pub label: String,             // "user", "wake", "model #2", "bash cargo test", "t3 review diff"
    pub started_at: i64,           // unix ms
    pub ended_at: Option<i64>,     // None while running or after an interruption
    pub status: SpanStatus,        // running | ok | error | cancelled | interrupted
    pub estimated: bool,           // timing derived from neighbouring entries
    pub provider: Option<ProviderTiming>, // firstOutputMs, attempts, retries, usage, request
    pub tool_call_id: String,      // tool spans; used to match approvals
    pub task_id: String,           // sub-agent spans and spans inside a child
    pub triggered_by: Vec<String>, // wake turns: task ids from delivered notifications
    pub children: Vec<Span>,
}
```

Input: the parent session's entries, and for each child its
`otto.task_spec` and entries. Rules:

- **Turn**: `TurnRecord` `started` to `ended`. Without an `ended` entry the
  turn is `running` while the session runs a turn, and `interrupted`
  otherwise.
- **Tool**: `otto.operation` `attempt` to `terminal` timestamps. Without a
  `terminal` entry the span is running.
- **Provider**: `otto.provider_call`. While a turn runs and neither a tool
  nor a recorded request is open, one running `model` span starts at the
  timestamp of the turn's last entry. In a session written before this
  change, a provider span starts at the entry before an assistant message
  and ends at the assistant message, with `estimated: true`.
- **Compaction**: compaction entries, with timing from the
  `otto.provider_call` of kind `local_compaction` or `remote_compaction`.
- **Sub-agent**: one span per child, nested under the tool span whose
  `tool_call_id` equals `parentToolCallId`. Its children are built from
  the child's own entries with the same rules, and their ids are prefixed
  with `<task id>:`. Delegation depth is one, so the tree has at most three
  levels below a turn. A resumed sub-agent appears once, under the call
  that started it.
- Provider and compaction entries outside any turn are not shown.
- A tool label carries the tool name and an argument preview of at most
  60 characters.

### 5. Transport

- **Local TUI**: `backend::Request::Trace` calls `Controller::trace`, which
  reads the parent session file and every `*.jsonl` file in the session's
  children directory, the directory `subagent::interrupted::scan` already
  reads, so tasks started by an earlier process are included. A file is
  parsed again only when its length or modification time changed since the
  last read. A child is running while its task in the registry has not
  reached a final status.
- **HTTP**: `GET /v1/sessions/{id}/trace` returns the `Trace` as JSON. It
  holds no message or result text; tool labels hold the 60-character
  argument preview.
  `GET /v1/sessions/{id}/trace/{span_id}` returns the content of one span:
  tool arguments and result; for a provider span, the messages that
  preceded it, its reply, and the `request` estimates; for a sub-agent, its
  prompt and result. Content passes the same redaction as
  `GET /v1/sessions/{id}/history`.
- **Attached TUI**: uses the HTTP endpoints through `client`, as
  `/context` does.

### 6. TUI `/trace`

`/trace` opens a centered panel like `/context` and `/agents`. Panel
commands open immediately during a turn (`App::dispatch_line` passes `/`
lines to `Catalog::open` whether or not a turn runs).

```text
┌ Trace · turn 7/9 · user · 48.2s · in 31.4k out 2.1k ─────────────────────────┐
│                               0s       10s      20s      30s      40s   48s  │
│ ▾ turn 7  user                ██████████████████████████████████████████████ │
│   ├ model #1  ttft 3.1s       ░░░███                                         │
│   ├ bash  cargo test …  9.8s        ██████████                               │
│   ├ model #2  retry 1/3                       ░░▒▒▒░░███                      │
│   ├ ▾ agent t3 "review diff"                           ███████████████ 21.0s │
│   │   ├ model #1  ttft 2.0s                            ░░██                  │
│   │   └ bash  git diff main                                ◇◇◇██             │
│   └ model #3  ttft 1.2s                                               ░███   │
│ ▸ turn 8  wake ← t3 finished                                             ██  │
├──────────────────────────────────────────────────────────────────────────────┤
│ model #2 · 7.9s · attempts 2 · retry: connection interrupted, waited 2s      │
│ ttft 4.4s · in 14.0k (cached 11.2k) · out 402 · stop tool_use                │
│ ↑/↓ select · Enter open · ←/→ turn · Esc close                               │
└──────────────────────────────────────────────────────────────────────────────┘
```

- Bar segments: `░` request sent, no output yet; `▒` retry wait; `◇`
  waiting for approval; `█` output arriving or tool running. Failed spans
  are red; `estimated` spans are dim.
- The time axis spans the selected turn. `←/→` selects another turn.
- `Enter` on a sub-agent row expands or collapses it; on any other row it
  opens that span's content in a scrollable view. `Esc` goes back, then
  closes.
- The detail area shows the selected span's timing, attempts, retries,
  token usage and stop reason. For a provider span it also shows the
  `request` estimates and the recalled memory record ids.
- Below 80 columns the bar column is omitted; rows show the tree and
  durations only.
- Refresh: the presenter returns `periodic() == true`, so the existing
  feature refresh re-queries the trace every 2 s while it is the top panel.
  A running span's bar is drawn from its start to the current clock at
  every redraw (every 100 ms during a turn, every 500 ms when idle), so it
  extends between queries.

### 7. Approvals while `/trace` is open

Today an approval that arrives while a panel is open is pushed on top of
it (`App::sync_shared_approval`, `App::show_approval`). Memory review
requests use the same path.

- `Presenter` gains `fn defers_approvals(&self) -> bool`, default `false`.
  The trace presenter returns `true`. Both push sites skip the push while
  the top panel defers approvals.
- The trace title row shows `N approval pending · a open` in the
  queued-input banner style (`render::queued_banner_style`), with the
  remaining time when the approval has `expires_at`.
- The tool span matched by the approval's `subject.tool_call_id` and
  `scope.task_id` draws its wait from `created_at` as `◇`.
- `a` pushes the oldest pending approval over the trace. Approving or
  denying closes it and shows the trace again.
- Closing the trace pushes a pending approval immediately, using the last
  approval snapshot.

## Limits

- A provider request in progress shows its timing details (first output,
  retries) only after it ends. For the parent turn the status line still
  shows the live phase and retry countdown.
- A historical request's input is reconstructed from the messages before
  it. The system prompt and tool definitions are represented by their
  token estimates only. A memory record looked up by id can have changed
  or been deleted since the request.
- With `--no-session` there is no session file, and `/trace` reports an
  error.
- Sessions written before this change show estimated provider spans.

## Out of scope

- Views across sessions.
- Chrome trace export for Perfetto.
- The web UI and the REPL. otto-plus can read the HTTP endpoint later.
- A `started` record for in-progress child provider requests.

## Tests

Each test fails without the corresponding change.

1. `otto-core` agent: a turn with one retried provider request appends one
   `otto.provider_call` with `attempts: 2`, one `retries` item, a
   `firstOutputMs`, the provider usage and the `request` estimates; a
   compaction appends one with its compaction kind.
2. `otto-core` codec: the entry round-trips, and a reader built before the
   type exists keeps it unchanged.
3. `otto-core` trace builder, from fixture sessions: span tree and times
   for a turn with provider, tool and compaction spans; a running turn; an
   interrupted turn; a wake turn's `triggered_by`; a child nested by
   `parentToolCallId`; a child without it placed by time; estimated
   provider spans for a session without `otto.provider_call`.
4. `otto` sub-agent: the child's `otto.task_spec` carries the spawning
   call's `parentToolCallId`.
5. `otto` server: both trace endpoints for an existing session, `404` for
   an unknown session or span, and no message text in the trace response.
6. `otto` TUI: an approval is not pushed while `/trace` is the top panel;
   `a` pushes it; closing `/trace` pushes it; rows and bars render at 100
   columns, and the bar column is omitted at 79 columns.

## Delivery

1. Records: `otto.provider_call` and `parentToolCallId`.
2. Trace builder, local backend request and HTTP endpoints.
3. TUI `/trace` and approval deferral, the user manual
   (`interactive-use.md`, `servers-and-connectors.md`) and the changelog.
