# All sub-agent runs in the TUI and the web UI

Status: approved 2026-09-25.

## Problem

Sub-agent tasks can be observed only inside the session that started them:

- The TUI and REPL have `/tasks`, `/task <id|name>` and `/task cancel`, over
  the session's own `Tasks` registry (`crates/otto/src/subagent/tasks.rs`).
- The web UI has a per-session Tasks panel over
  `GET /v1/sessions/{id}/tasks`.

`Tasks` is in memory and per session. When a session is closed or the process
exits, every task's status, timing, step and tool-call counts, token usage,
result and error are gone. Only the child transcript file remains
([agent observability](2026-09-25-agent-observability.md), slice 2), and only
for file-backed parent sessions. There is no view of the tasks running in
other sessions, other TUI processes, or `otto serve`, and no view of past
tasks.

## Scope

In scope: tasks started by the `agent` tool through `subagent::Runner`, in
every session of every otto process on the machine, running or finished, in
one list in the TUI and in the web UI.

Out of scope:

- Workflow steps. `otto workflow` has its own durable run store and the web
  UI's Workflows view.
- Cancelling a task that belongs to another process. Cancel stays where it is
  today: `/task cancel` in the owning TUI session, and the web Tasks panel for
  sessions the server owns.
- Pruning or exporting records.

No configuration switch: the records are local, like `~/.otto/usage.db`.

## Storage

`~/.otto/tasks.db`, SQLite, mode `0600` like the other databases, one table:

| column | content |
| --- | --- |
| `parent_session` | parent session id; for an in-memory parent, `memory:<pid>:<process start RFC 3339>` |
| `task_id` | the registry id |
| primary key | (`parent_session`, `task_id`) |
| `workspace` | absolute workspace path |
| `parent_session_path` | parent session file, empty when in memory |
| `name`, `agent`, `description`, `model`, `context` | as in `Task` |
| `prompt` | first 64 KiB of the prompt |
| `status` | `queued`, `running`, `succeeded`, `failed`, `canceled` |
| `created_at`, `started_at`, `finished_at` | RFC 3339 UTC, empty until set |
| `steps`, `tool_calls`, `last_tool` | as in `Task` |
| `input_tokens`, `output_tokens`, `cached_tokens` | from `Task::usage`, when present |
| `result`, `error` | first 64 KiB each |
| `session_path` | child transcript file, empty when in memory |
| `pid`, `process_started_at` | the owning process |

Writes:

- `Tasks` gets an optional recorder, injected at the composition root. `add`,
  `mark_running`, `record_provider_step`, `record_tool_call` and `finish`
  upsert the task's row after the registry mutex is released. Tests and
  `--no-session` headless runs without a recorder behave as today.
- An open or schema error prints one warning to stderr at startup, before
  any UI starts, and the process runs without a recorder. A later write
  error disables the recorder for that process without output, like the
  `usage.db` collector, because stderr output would overwrite the TUI
  screen. Neither fails a task or a turn.
- The schema carries a `user_version`. A database with an unknown version is
  not written; the recorder is disabled with a warning.
- Several processes write the same file. SQLite serialises the writes; each
  row is written only by the process that owns its task, so there is no
  conflict on a row.

Liveness is computed on read, never written back: a `queued` or `running` row
whose `pid` is not alive, or is alive with a different start time (pid
reuse), is shown as `interrupted`.

## Server API

| method and path | behaviour |
| --- | --- |
| `GET /v1/tasks` | Rows newest first by `created_at`. Query: `status` (one of the stored statuses or `interrupted`), `workspace` (exact path), `limit` (default 100, max 500), `before` (a `created_at` cursor). Each item carries `cancelable: true` only when the task belongs to a session this server owns and is not final. |
| `GET /v1/tasks/{parent_session}/{task_id}` | One row plus the child transcript read from `session_path`, in the same history shape as `GET /v1/sessions/{id}/tasks/{task_id}`. A missing transcript file returns the row with an empty history and `transcript_missing: true`. |

Both read `tasks.db` directly; they work for tasks from any process. The
existing per-session task endpoints do not change.

List response:

```json
{
  "tasks": [
    {
      "parent_session": "01J...",
      "task_id": "t1",
      "workspace": "/Users/me/src/app",
      "parent_session_path": "/Users/me/.otto/sessions/.../01J....jsonl",
      "name": "reviewer",
      "agent": "code-reviewer",
      "description": "review the diff",
      "model": "gpt-5.1",
      "context": "fresh",
      "prompt": "...",
      "status": "running",
      "created_at": "2026-09-25T10:00:00Z",
      "started_at": "2026-09-25T10:00:01Z",
      "finished_at": "",
      "steps": 3,
      "tool_calls": 5,
      "last_tool": "read",
      "input_tokens": 12000,
      "output_tokens": 800,
      "cached_tokens": 9000,
      "result": "",
      "error": "",
      "session_path": "/Users/me/.otto/sessions/.../01J.../t1-01K....jsonl",
      "cancelable": true
    }
  ],
  "next_before": "2026-09-25T09:58:00Z"
}
```

`status` is the displayed status, so it can be `interrupted`. `next_before`
is empty when there are no older rows. The detail response is one such object
under `task`, plus `history` (the per-session task endpoint's history array)
and `transcript_missing`.

## Web UI

- A fourth view button, **Agents**, beside Chat, Usage and Workflows
  (`ui/src/App.tsx`), backed by a new `ui/src/AgentsView.tsx`.
- A table: status, agent (or `default`), description, workspace (basename,
  full path in the title), parent session, created, duration, steps, tool
  calls, tokens. Status and workspace filters map to the query parameters.
  "Load more" uses `before`.
- It polls every 3 seconds while any visible row is `queued` or `running`,
  the same interval as the Tasks panel, and stops otherwise.
- Selecting a row shows the prompt, result or error, and the child transcript
  rendered with the existing `TranscriptView`. **Cancel** is shown when
  `cancelable` is true and calls the existing per-session cancel endpoint.
- The parent session links to Chat with that session opened when the server
  owns it; otherwise the path is shown as text.

## TUI

- `/agents` opens a modal over `tasks.db`, newest first, with the same
  columns as the web table narrowed to the terminal width.
- Keys: up/down to move, `s` to cycle the status filter (all, queued,
  running, succeeded, failed, canceled, interrupted: the values `GET
  /v1/tasks` accepts), `w` to toggle between this workspace and
  all workspaces (default: this workspace), Enter to open the selected task,
  Esc to close.
- The detail pane shows the prompt, result or error, and the child transcript
  read from `session_path`, scrollable.
- It refreshes every 2 seconds while open.
- The REPL gets `/agents` as a plain table of the latest 50 rows.
- `/tasks` and `/task` keep their current per-session behaviour.

## Code placement

- `crates/otto/src/subagent/record.rs`: the SQLite recorder and the reader
  (query, liveness).
- `crates/otto/src/subagent/tasks.rs`: the optional recorder hook.
- `crates/otto/src/cli`: opens `tasks.db` and passes the recorder into every
  runner it builds; `/agents` in the REPL.
- `crates/otto/src/server`: the two endpoints.
- `crates/otto/src/tui`: the `/agents` modal.
- `ui/src/AgentsView.tsx`, `ui/src/api.ts`, `ui/src/App.tsx`.
- User manual: `/agents` in the TUI and REPL command lists, the Agents view,
  the two endpoints, and the `tasks.db` file with what it stores.
- AGENTS.md task map: "persistent task records" in the `subagent` entry.

## Tests

All offline and deterministic.

- Recorder: each lifecycle call produces the expected row; prompt, result
  and error are truncated at 64 KiB on a character boundary; a write error
  disables the recorder with one warning and the task still finishes; an
  unknown `user_version` is not written.
- Liveness: a running row with a dead pid, and with a reused pid (different
  start time), reads as `interrupted`; a finished row never does.
- Two registries in one test, standing in for two processes, write to one
  database and both rows are listed.
- Server: filters, `limit` bounds, the `before` cursor, `cancelable` true only
  for an owned non-final task, a missing transcript file.
- Web: `AgentsView` renders rows, applies filters, polls only while a row is
  active, and shows Cancel only when `cancelable`.
- TUI: the modal lists rows, filters by status and workspace, opens the
  detail pane, and fits an 80-column terminal.

## Privacy

`tasks.db` stores sub-agent prompts, results and errors (each capped at 64
KiB) in plaintext under `~/.otto`, next to the session files that already hold
the full text. Nothing leaves the machine.
