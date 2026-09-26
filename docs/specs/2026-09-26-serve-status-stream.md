# Process-wide session status stream for `otto serve`

Status: approved 2026-09-26.

## Problem

The Web UI sidebar lists sessions grouped by working directory
([2026-09-26-web-sidebar-by-workspace.md](2026-09-26-web-sidebar-by-workspace.md))
but shows only `open` per row. To see whether another session is running a
turn, waiting for a Bash approval, failed, or has sub-agent tasks running,
the user has to open it. Facts from the current code:

- The only live stream is per turn:
  `GET /v1/sessions/{id}/turns/{turn_id}/events` (`server/mod.rs:1447`,
  `stream_sse` at `mod.rs:1599`), driven by a `watch::Sender<u64>` version
  counter in `Turn` (`server/turn.rs`).
- The sidebar's session list (`GET /v1/sessions`) is re-read only after the
  user's own create, rename, or close (`ui/src/App.tsx:159,173,290,444`), so a
  session opened by another client or by a server wake does not appear.
- Turn status is `running`, `ok`, `error`, or `canceled`
  (`server/turn.rs:26-29`); the latest turn stays in `SessionState.turn` until
  the next one starts.
- A pending elevated-Bash approval exists only inside `BashApprovals`
  (`tool/bash.rs`), keyed by session id, 5-minute expiry; no session field
  reports it.
- Running sub-agent tasks are visible only through
  `GET /v1/sessions/{id}/tasks`. The per-session wake loop
  (`Server::start_wake_loop`, `mod.rs:664`) already wakes on the task
  registry's update channel.
- `Server.sessions` (`mod.rs:340`) is the one map that holds every open
  session in every loaded workspace.

## Scope

In scope: one SSE route that reports the status of every open session in this
process, and the sidebar showing it.

Status values per session, decided for this step:

| Field | Meaning | Source |
| --- | --- | --- |
| `turn` | `running`, or the last finished turn's `ok` / `error` / `canceled`, or `null` if the session has had no turn since it was opened | `SessionState.turn` summary status |
| `approvals` | number of unexpired pending Bash approvals | `BashApprovals` |
| `tasks` | number of sub-agent tasks in `queued` or `running` | `Controller::tasks()` |

Out of scope:

- "Unread" state. It needs a per-client record of what was seen.
- Sessions that are not open in this process. They have no turn, approval,
  or task state in memory; the sidebar shows them without a status, as today.
- Other Otto processes. `GET /v1/tasks` already covers cross-process tasks.
- Replacing the Web UI's 1-second `GET /v1/sessions/{id}` poll of the open
  session (`ui/src/follow.ts`). It keeps working unchanged; replacing it is a
  later change once this stream has been used.
- Turn text, tool calls, or any other turn content. Those stay on the
  per-turn stream.

## Server

### Route

`GET /v1/status`, `text/event-stream`, same bearer token as every route.

Each event is a full snapshot, not a delta:

```
event: status
data: {"sessions":[{"id":"...","workspace":"/abs/path","turn":"running","approvals":0,"tasks":1}, ...]}
```

- Sessions are sorted by `workspace`, then `id`.
- The first event is sent immediately on connect. After that, an event is
  sent when the snapshot differs from the last one sent on that connection.
- No `id:` field and no replay. A reconnecting client receives the current
  snapshot as its first event, which is all it needs.
- Full snapshots keep the client free of ordering and missed-event logic. The
  payload is about 100 bytes per open session.

### Change notification

`Server` gets one `tokio::sync::watch::Sender<u64>` (`status_changed`),
the same pattern `Turn` uses. It is bumped at:

1. `register` and `remove` (a session opened or closed).
2. Turn start in `start_turn` and `wake_turn`.
3. Turn finish (the place that sets the turn's final status).
4. The wake loop's task-registry update (`start_wake_loop`), which already
   fires on every task status change.
5. `POST /v1/sessions/{id}/approvals/{approval_id}` after a grant.

A new pending approval is created during a running turn, and the bump at that
turn's finish reports it. An approval that expires after 5 minutes produces no
bump; the next snapshot for any other reason drops it.
ponytail: expiry is observed lazily; add a timer if a stale approval count
matters.

Each connection's stream waits on `status_changed.subscribe().changed()`,
builds the snapshot from `all_sessions()`, and sends it if it differs from the
previous one. `watch` coalesces bursts, so a client never falls behind.

`BashApprovals` gains `pending_count(session_id) -> usize`, which applies the
same expiry filter as `approve`.

## Web UI

- `api.ts`: `streamStatus(signal)` over the existing `readSSE`, since the
  token is sent as a header and `EventSource` cannot set one. Types
  `SessionStatus` and `StatusSnapshot` live in `api.ts`, like
  `WorkspaceEntry`.
- `App.tsx`: open the stream on load; on close or error, reconnect after 1s
  while the page is open. Keep the latest snapshot in state as a map by
  session id. When a snapshot contains an id that is not in the session list,
  call `refreshSessions` once for that snapshot.
- `Sidebar.tsx`: each row with a status shows, after the name:
  `running` while `turn` is `running`; `approval` when `approvals > 0`;
  `error` when `turn` is `error`; `N tasks` when `tasks > 0`. Each is a small
  text badge with an `aria-label` that states it in words. `●` for `open`
  stays. A group header shows the count of running sessions in that
  directory when it is greater than zero.

## Tests (TDD order)

Rust, `crates/otto/src/server` tests with the existing fake factory:

1. `GET /v1/status` without a token is `401`.
2. With no open sessions, the first event is `{"sessions":[]}`.
3. Creating a session produces a snapshot containing it with `turn: null`.
4. Starting a turn produces `turn: "running"`; finishing produces `"ok"`;
   a failing turn produces `"error"`.
5. A session in a second workspace appears with that workspace.
6. Closing a session removes it from the next snapshot.
7. `BashApprovals::pending_count` counts unexpired entries only.
8. Two identical snapshots in a row are sent once.

UI (vitest):

9. `Sidebar` renders the four badges from a status map, and none for a
   session without status.
10. The group header shows the running count.
11. The status reader in `App` calls `refreshSessions` when a snapshot has an
    unknown id, and reconnects after the stream ends.

Docs: `testdata/server/openapi.yaml` gets `/v1/status`; the user manual's
`### HTTP API` and `### Web UI` sections describe the route and the badges.

Gate: `make check`.
