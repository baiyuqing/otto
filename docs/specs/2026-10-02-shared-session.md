# Shared sessions through `otto serve`

Status: proposed 2026-10-02, not approved. No code has been written for it.

Extends the [chat connector design](2026-10-02-otto-connect.md): its "Not in
this change" list names session listing and switching from the chat; this
document adds them.

## One session is held by `otto serve` and used by Telegram, the TUI and the web UI

The user wants one otto session that Telegram (through `otto-connect`), the
TUI and the web UI use at the same time: a message from any of them is a turn
in the same conversation, and the model sees all of it.

The split:

- `otto serve` is the only process that opens the session. It runs the model,
  the tools and the approvals, and it orders turns.
- `otto acp --attach` is an ACP agent on stdio that holds no session. It
  forwards each ACP method to `otto serve` over the Unix socket.
  `otto-connect` starts it instead of `otto acp`; the connector's ACP code is
  unchanged.
- `otto --attach` runs the TUI as a client of `otto serve`.
- The web UI already is a client of `otto serve`.

Defaults the user accepted on 2026-10-02:

- a chat receives replies only to the messages it sent; the model context is
  shared;
- a message that arrives while the session is running a turn is queued in
  `otto serve` and runs after the turns before it;
- a chat is bound to a session by a chat command;
- an approval request can be answered from any client, and the first answer
  is the one applied.

## Facts this design relies on

otto at `6de063d`.

Sessions and `otto serve`:

- Opening a session takes an exclusive non-blocking `flock` on the session
  file; a second process gets "session is already open by another Otto
  process" (`crates/otto/src/session/fsops.rs:84-103`). The TUI, `otto acp`
  and `otto serve` each open sessions in their own process, so two of them
  cannot use one session at the same time.
- `otto serve` keeps open sessions in a registry. `POST /v1/sessions
  {"resume": id}` returns an already-open session with 200. Any client can
  start a turn, cancel a turn, and read any turn's events.
- One turn runs per session. `POST /v1/sessions/{id}/turns` during a running
  turn or a compaction returns 409 `turn_active`
  (`crates/otto/src/server/mod.rs:928-939`, `1572-1576`).
- Only the newest turn of a session is kept. `GET .../turns/{turn_id}` and
  its `/events` return 404 for an older turn (`server/mod.rs:1599-1607`,
  `server/turn.rs:10-11`).
- A turn's SSE stream ends by closing. No frame carries the final status or
  error (`server/mod.rs:1780-1828`). The response to `POST .../turns` does not
  name the turn; the web UI reads the id from `GET /v1/sessions/{id}`
  (`ui/src/App.tsx:398-402`).
- No frame carries the prompt text. The web UI adds its own prompt to the
  transcript (`ui/src/App.tsx:403-408`), so a turn it follows from another
  client shows no prompt until the history is reloaded.
- An elevated bash command ends with a tool result that contains
  `Approve in Otto: /approve <id>`. `POST /v1/sessions/{id}/approvals/{aid}`
  grants it only while no turn runs, and returns `{"prompt": ...}`, which the
  client sends as the next turn (`server/approvals.rs`,
  `app/mod.rs:1202-1212`). A second POST for the same id also succeeds
  (`tool/bash.rs:199-209`). There is no route to deny.
- `otto acp` runs the approval request, the client's answer and the retry
  inside one `session/prompt` (`crates/otto/src/acp/mod.rs:301-361`).
- Resume by id searches only the newest 20 sessions of each loaded workspace
  (`cli/serve.rs:484-500`; `MAX_LIST_SESSIONS` in `session/list.rs:19`).
  `otto acp` opens `<session dir>/<id>.jsonl` directly (`acp/mod.rs:229-264`).
- `GET /v1/sessions` rows carry no last user text and no modification time
  (`server/mod.rs:1277-1290`). ACP `session/list` needs both for `title` and
  `updatedAt`.
- Turn errors are stored, returned and logged without secret redaction
  (`server/mod.rs:966-985`). `otto acp` passes errors through
  `redact_error` (`acp/mod.rs:176-180`).
- `GET /v1/status` is an SSE stream that sends a full snapshot on every
  change. Each row is one open session: `id`, `workspace`, `turn` (a status),
  `approvals` (a count), `tasks` (`server/mod.rs:742-779`, `1305-1317`).
- While idle, the web UI polls `GET /v1/sessions/{id}` every 1000 ms and
  follows a running turn it did not start (`ui/src/follow.ts:3-18`). When its
  own `POST .../turns` gets 409, it follows the other turn and does not send
  its text (`ui/src/App.tsx:410-421`).
- `otto serve` listens on `--socket PATH` (Unix socket, no token, directory
  mode 0700) or on `--listen HOST:PORT` (loopback TCP, bearer token), not
  both (`docs/user-manual.md:249-250`). A browser can only use TCP.
- SIGTERM cancels running turns.

TUI:

- `App::apply_event` takes the agent `Event` (`crates/otto/src/tui/app.rs:1420`).
  The transcript reducer in otto-core takes `WireEvent`, which implements
  `Deserialize`.
- The TUI holds `&Controller` and calls 33 distinct `Controller` methods by
  name (grep of `crates/otto/src/tui/`). Async work goes through the `Action`
  enum (`tui/app.rs:133-164`).
- Text typed during a turn goes to the running turn's inbox
  (`Controller::queue_user_message`, `app/mod.rs:1079`); the model reads it at
  its next step.

ACP and the connector:

- ACP v1 defines `$/cancel_request` (`agent-client-protocol-schema` 1.10.2,
  `src/v1/protocol_level.rs`). `coder/acp-go-sdk` v0.13.5 cancels the
  handler's context when it receives one (`connection.go:388-415`,
  `542-560`). `otto-connect`'s permission wait returns when that context ends
  and does not tell the chat (`connect/internal/bridge/bridge.go:533-544`).
- `otto-connect` sends `session/load` when a chat's session is not open in
  the current agent process, and starts the agent again on the next message
  after it exits.
- reqwest 0.12.28, already a dependency of `crates/otto`, connects over a
  Unix socket with `ClientBuilder::unix_socket` (`#[cfg(unix)]`).

## `otto serve` listens on the Unix socket and TCP at the same time

- `otto serve --socket PATH --listen HOST:PORT` binds both. One `Server`
  serves both listeners. Requests on the socket need no token; requests on
  TCP need the token, as today. `--exit-on-stdin-close` and `--open` still
  require `--listen`.
- Combining is available on the command line only. With neither flag, the
  `[server]` precedence is unchanged (`listen`, then `socket`).
- Attach clients use the socket. The web UI uses the TCP URL.
- A session that `otto serve` holds cannot be opened by the local TUI, the
  REPL or a plain `otto acp` at the same time; the `flock` error is unchanged.
- Every client of a shared session works in a workspace that `otto serve` has
  loaded. `POST /v1/sessions {"workspace": path}` loads a trusted workspace,
  as today.

## A turn started while the session is busy is queued in `otto serve`

- `POST /v1/sessions/{id}/turns` accepts `"queue": true`. When no turn is
  running or queued and no compaction runs, the turn starts at once.
  Otherwise it is appended to the session's queue with status `queued`.
- A session queues at most 16 turns. The 17th gets 409 `queue_full`.
- Without `"queue": true` the route returns 409 `turn_active` as today.
- A queued turn can be read, followed and cancelled through the existing turn
  routes. Its event stream sends nothing until it starts. Cancelling it
  removes it from the queue.
- When a turn ends, the next queued turn starts. The wake loop starts a
  `task` turn only when no turn is running or queued; a queued user turn reads
  pending notifications like any turn.
- `POST .../compact` while a turn is running or queued returns 409, as today
  for a running turn.
- On shutdown, queued turns end with status `canceled`.
- Each `GET /v1/status` row gains `turn_id` (the running turn, or else the
  newest) and `queued` (a count).

## Each turn stream starts with the prompt and ends with the result

- `POST .../turns` responses carry the header `Otto-Turn-Id`.
- The first frame of a turn started by a client is `user_message` with
  `text`, and `image: true` when an image was attached. The image data is not
  repeated. `task` turns have no `user_message` frame.
- The last frame of every turn is `turn_end` with `status` (`ok`, `error`,
  `canceled`) and `error` (redacted, empty unless `status` is `error`). Then
  the stream closes. Both frames are stored in the turn's event buffer, so a
  reader that reconnects with `?after=N` receives them too.
- The otto-core transcript reducer, which the web UI (through otto-web) and
  the TUI use, renders `user_message` as a user item and `turn_end` with
  status `error` as an error item. Other `turn_end` frames add nothing.

## Approvals are decided inside the waiting turn, by any client

- When a model step of a serve turn ends with a pending elevated bash request
  (detected as `otto acp` detects it), the turn does not end. It emits
  `approval_requested` with `approval_id`, `tool_call_id`, `command` and
  `justification`, and waits.
- `POST /v1/sessions/{id}/approvals/{aid}` takes `{"decision": "allow"}` or
  `{"decision": "deny"}`:
  - The first decision for a waiting request returns 200 `{"decision": ...}`.
    A later one returns 409 `approval_decided`. An id that is not waiting
    returns 409 `approval_failed`, as today.
  - `allow`: serve grants the command and runs the retry prompt inside the
    same turn, with the same turn id and stream, as `otto acp` does.
  - `deny`: the turn ends with status `ok`.
- Every outcome emits `approval_decided` with `approval_id` and `decision`
  (`allow`, `deny`, `timeout`).
- A request with no decision after 10 minutes is decided `timeout`, which
  denies. Cancelling the turn during the wait ends it `canceled`.
- While a turn waits, the session is busy: other turns queue behind it.
- The response no longer contains `prompt`, and clients no longer send a
  retry turn. "Approve always" (exclude the program from the sandbox, then
  grant) is not offered through the API in this change; the local TUI keeps
  it.
- The loop (prompt, detect the request, wait for a decision, grant, retry)
  moves into `app` as one function with a decision callback. `otto acp`
  passes a callback that sends `session/request_permission`; serve passes one
  that waits for the route or the timeout. `otto acp` behavior is unchanged.

## Other `otto serve` changes

- Resume by id: the id must be 32 lowercase hexadecimal characters (the check
  `otto acp` applies). Serve then opens `<session dir>/<id>.jsonl` in each
  loaded workspace, so a session older than the newest 20 can be resumed.
- `GET /v1/sessions` rows add `last_user_text` (at most 80 characters) and
  `modified` (RFC 3339).
- Turn errors pass through `redact_error` before they are stored, returned in
  `turn_end` and `GET .../turns/{id}`, or logged.

## `otto acp --attach` forwards one ACP connection to `otto serve`

`otto acp --attach [--socket PATH]` speaks ACP v1 on stdio with the same
`initialize` response, workspace rule (the process working directory) and
`cwd` checks as `otto acp`. It opens no session, runs no model or tool, and
reads no provider credentials. The socket defaults to `[server].socket`, then
`~/.otto/otto.sock`. TCP is not supported, because it needs the token.

At start it sends `GET /healthz`. If that fails it prints
`otto serve is not reachable at <path>: <error>` to stderr and exits with
status 1; `otto-connect` reports the exit status and stderr to the chat that
sent the message.

| ACP method | `otto serve` requests |
| --- | --- |
| `session/new` | `POST /v1/sessions {"workspace": cwd}` |
| `session/load` | `POST /v1/sessions {"resume": id, "workspace": cwd}`, then `GET /v1/sessions/{id}/history`, sent as `session/update` notifications with the function `otto acp` uses, then the response |
| `session/list` | `GET /v1/sessions?workspace=cwd`; `title` is the name or else `last_user_text`, `updatedAt` is `modified` |
| `session/prompt` | `POST /v1/sessions/{id}/turns {"text": ..., "queue": true}`, read until `turn_end` |
| `session/cancel` | `POST /v1/sessions/{id}/turns/{turn_id}/cancel` for the prompt's turn, queued or running |

During a prompt:

- Frames are mapped to `session/update` by the function `otto acp` uses,
  which this change rewrites to take `WireEvent`; local `otto acp` converts
  with `to_wire` first. `user_message` is not forwarded.
- `turn_end` with `ok` answers `end_turn`, with `canceled` answers
  `cancelled`, and with `error` answers a JSON-RPC error carrying the
  redacted message.
- `approval_requested` sends `session/request_permission` with the options
  `otto acp` sends. `allow_once` posts `allow`. Any other outcome, an error
  response or a response that does not parse posts `deny`. A 409
  `approval_decided` is ignored. If `approval_decided` arrives while the
  request is open, the relay sends `$/cancel_request` for it and ignores the
  client's later answer.
- Turns started by other clients are not forwarded.

If the connection to `otto serve` fails or a stream ends without `turn_end`,
every open prompt gets the JSON-RPC error `connection to otto serve lost`,
and the process exits with status 1.

## `otto-connect` binds a chat to a session with `/sessions` and `/use`

Configuration for a shared session:

```toml
[agent]
command = ["otto", "acp", "--attach"]
workspace = "/Users/me/work"   # a workspace otto serve loads
```

New connector commands, handled before the queue like the existing ones:

| Command | Effect |
| --- | --- |
| `/sessions` | Lists up to 10 sessions from `session/list`: the first 8 characters of the id, the update time, the title. `*` marks the chat's session. |
| `/use <id>` | Binds the chat to a session: a full 32-character id, or a prefix of at least 4 characters that matches exactly one listed session. Sends `session/load`; on success stores the binding in `state.json` and replies with the id and title. Refused while the chat has a running or queued prompt. |

- An unknown or ambiguous prefix gets a reply naming the problem.
- `session/list` returns the newest 20 sessions, so an older session is bound
  by its full id.
- Both commands also work with a plain `otto acp`; there, `/use` of a session
  that another process holds fails with the agent's lock error.
- When the context of a pending permission request ends while the connector
  keeps running (the agent sent `$/cancel_request`), the chat gets
  "permission request answered elsewhere".
- A session is shared with the TUI or the web UI only if they use the
  connector's workspace.

## `otto --attach` runs the TUI as a client of `otto serve`

`otto --attach [--socket PATH] [--resume ID | --continue]`:

- no session flag: a new session in the working directory's workspace;
- `--continue`: the newest session of that workspace;
- `--resume ID`: that session in any workspace serve has loaded.

`--attach` cannot be combined with `--prompt`, `--no-session`, `--archive`,
`serve` or `acp` (which has its own `--attach`).

The TUI's async operations go through one backend type with two
implementations: `Controller` (local mode) and a serve client (attach mode).
`App::apply_event` takes `WireEvent`; local mode converts with `to_wire`.

| TUI function | Attach mode |
| --- | --- |
| Prompt, image prompt | `POST .../turns` with `"queue": true` |
| Text typed during a turn | Queued as the next turn; not inserted into the running turn |
| Withdrawing queued text | Cancels the newest queued turn this TUI started |
| Esc (cancel) | `POST .../turns/{id}/cancel` |
| `/compact` | `POST .../compact` |
| `/new`, `/resume`, session picker | `POST /v1/sessions`, `GET /v1/sessions` |
| `/rename` | `PATCH /v1/sessions/{id}` |
| Context view | `GET .../context` |
| Agents view | `GET .../tasks`, `GET /v1/tasks` |
| Approval dialog | Opened by `approval_requested`; Yes posts `allow`, No posts `deny`; closed by `approval_decided`. No "always" option. |
| `/sandbox reload` | `POST /v1/sandbox/reload` (applies to all of serve) |
| Status line | `GET /v1/sessions/{id}`, `GET /v1/info` |

`/archive`, `/model` and profile switching, `/thinking`,
`/sandbox allow|network|exclude`, `/approve always`, `/login`, `/logout`,
`/mcp login`, `/memory` and `/remember` print
`not available with --attach`.

Turns from other clients: the TUI reads `GET /v1/status`. When its session's
`turn_id` changes to a turn this TUI did not start, it reloads the history and
follows that turn's events from 0. The prompt appears through `user_message`.

When the connection is lost, the TUI shows `disconnected from otto serve` and
retries every 1 s. On reconnect it resumes the same session and reloads the
history. Input while disconnected is refused with that message.

## Web UI changes

- Turns are sent with `"queue": true`. The turn id comes from
  `Otto-Turn-Id`. The footer shows `queued` until the turn's first frame
  arrives.
- The prompt text is displayed from `user_message`; the UI no longer adds it
  itself. An attached image is still added by the UI.
- `/approve <id>` posts `allow` and sends no prompt. `/deny <id>` posts
  `deny`. Both work while a turn runs.
- `approval_requested` and `approval_decided` are rendered by the shared
  reducer as notices.
- The 1000 ms idle poll is unchanged.

## Restart and upgrade

1. `git pull --ff-only && make install` (`make connect-build` as well when
   `connect/` changed).
2. Stop `otto serve` with SIGTERM. Running and queued turns end `canceled`,
   with a `turn_end` frame; waiting approvals end with them.
3. Start `otto serve` with the same flags.

Clients:

- `otto acp --attach` exits with status 1. A chat whose prompt was running
  gets the exit report; the next message starts a new relay, which loads the
  session from the new serve. `otto-connect` itself is restarted only when
  its binary changed.
- `otto --attach` reconnects (1 s retry).
- The web UI's status stream reconnects after 1000 ms (existing
  `STATUS_RECONNECT_MS`). An open turn stream ends; the 1000 ms poll follows
  the next turn.

Running `otto serve` as a login service is #267.

## Tests

Offline, no credentials, no network. In-process tests use the existing server
harness with a fake provider.

- serve:
  - queue: three turns from two clients run in arrival order; the 17th
    queued turn gets 409 `queue_full`; cancelling a queued turn removes it
    and its stream ends with `turn_end` `canceled`; a `task` turn does not
    start while a turn is queued; shutdown cancels queued turns;
  - stream: `Otto-Turn-Id` names the turn; frame 0 is `user_message`; the
    last frame is `turn_end`; `?after=N` past the end returns `turn_end`;
  - approvals: the turn waits; an allow from a second client runs the retry
    in the same turn id; a second decision gets 409 `approval_decided`;
    deny ends the turn `ok`; the 10-minute timeout (paused tokio time)
    denies; cancel during the wait ends `canceled`;
  - resume of the 21st-newest session by id; a non-hex id gets 404 without a
    file access outside the session directory;
  - list rows carry `last_user_text` and `modified`; status rows carry
    `turn_id` and `queued`;
  - a provider error containing the API key and the home path is redacted in
    `turn_end`, `GET .../turns/{id}` and the log;
  - both listeners: the socket needs no token, TCP without the token gets
    401.
- `otto acp` local: the existing approval tests pass unchanged on the shared
  loop.
- `otto acp --attach`, against an in-process serve on a temporary socket:
  frames map to the same updates as local `otto acp` for the same fake
  provider script; `session/load` replays history before responding;
  `session/list` titles; two relays on one session: their prompts queue and
  each receives only its own text; an approval decided over HTTP by another
  client makes the relay send `$/cancel_request` and the prompt ends with the
  retry's text; serve stopping mid-prompt gives the error and exit status 1;
  no serve at start gives exit status 1 and the stderr line.
- `otto-connect`, with the fake agent: `/sessions` output; `/use` with a full
  id, a unique prefix, an ambiguous prefix, an unknown prefix, during a
  running prompt; the binding survives a connector restart; a cancelled
  permission request sends the notice. End to end with `OTTO_BIN`: start
  `otto serve --socket` with the Go fake provider, bind two chats to one
  session with `/use`, send from both, and check the order and that each chat
  receives only its own reply.
- otto-core reducer: `user_message`, `turn_end`, `approval_requested`,
  `approval_decided`.
- TUI attach: `App` tests with frames from a turn another client started
  (prompt and reply rendered after a history reload); every command in the
  unavailable list prints the message; the approval dialog closes on
  `approval_decided`. One PTY test starts `otto serve --socket` and
  `otto --attach`, sends a prompt, starts a turn over HTTP from the test, and
  checks that the TUI shows its prompt and reply.
- web UI: queued send reads `Otto-Turn-Id`; `/approve` and `/deny` post the
  decision and send no turn; the prompt comes from `user_message`.

Manual acceptance before the last change merges: Telegram, the TUI and the web
UI on one session. A message from each; a Telegram message during a TUI turn
runs after it; an approval requested in a Telegram turn and answered in the
web UI; a serve restart with each client resuming.

## Docs

- `docs/user-manual.md`: "Agent server" (two listeners, queue, frames,
  approvals, status fields, resume by id), "Web UI" (`/deny`, queued
  sends), "ACP agent server" (`--attach`), "Chat connector" (shared-session
  setup, `/sessions`, `/use`), TUI (`--attach` and the unavailable
  commands), the flag table (`--attach`; `--socket` is no longer `serve`
  only).
- `openapi.yaml` for the changed routes and frames.
- `AGENTS.md` task map: the serve client module used by `acp --attach` and
  the TUI.

## Delivery in four changes

1. serve and web UI: two listeners, queue, `Otto-Turn-Id`, `user_message` and
   `turn_end`, approvals inside the turn (with the shared loop in `app` used by
   `otto acp`), resume by id, list and status fields, error redaction, the
   reducer, the web UI changes, their docs.
2. The serve client module, `otto acp --attach`, `otto-connect` `/sessions`,
   `/use` and the cancelled-permission notice, the end-to-end test, their
   docs.
3. `otto --attach` with the functions in the table, the PTY test, its docs.
4. Only on request: serve routes for the TUI commands listed as unavailable.

## Decisions

- D1. One `otto serve` holds shared sessions; the TUI and the ACP relay
  attach over its Unix socket. The alternative, several processes sharing a
  session file, is excluded by the `flock` and the append-only session
  format.
- D2. `--socket` and `--listen` can be combined on the command line; the
  token applies to TCP only. The alternative is attach over TCP, which needs
  the token in the client.
- D3. Busy sessions queue a turn only when the request asks for it
  (`"queue": true`), at most 16 per session; other requests keep the 409.
- D4. Approvals are decided inside the waiting turn by any client; the first
  decision wins; 10 minutes without one denies. The approval route's response
  changes from `{"prompt"}` to `{"decision"}`, and a client that sends the
  retry prompt itself would now start a separate turn. This changes a
  documented API.
- D5. The ACP relay forwards only turns its client started. A chat shows
  replies to its own messages; the TUI and the web UI show every turn.
- D6. A chat is bound to a session with `/sessions` and `/use`, stored in
  `otto-connect`'s state file. There is no config key for it.
- D7. The TUI in attach mode supports the functions in the table; the others
  print `not available with --attach`. Text typed during a turn becomes a
  queued turn instead of being inserted into the running turn.
- D8. On a lost serve connection, `otto acp --attach` exits and
  `otto-connect` restarts it; `otto --attach` reconnects every 1 s.

## Not in this change

- Attach over TCP, to another host, or to more than one serve process.
- Sending other clients' turns or `task` turn results to a chat.
- "Approve always" through the API, and the TUI commands listed as
  unavailable.
- Inserting attach-client text into a running turn.
- Replacing the web UI's 1000 ms poll with the status stream.
- Running `otto serve` as a login service (#267), the Feishu adapter, and the
  removal of `[inbound.feishu]`.
