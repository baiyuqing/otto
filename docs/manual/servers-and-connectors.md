# Servers and connectors

Previous: [Safety and tools](safety-and-tools.md) · [Documentation index](../README.md) · Next: [Automation](automation.md)

## Agent server

`otto serve` runs Otto as a long-lived HTTP+JSON+SSE frontend, instead of the
TUI or REPL. One process holds the startup workspace (`--cwd`, default `.`)
and, when `[server].workspace_roots` or a trusted directory admits others, any number of additional
workspaces loaded on first use; it manages any number of sessions across all
loaded workspaces. Turns in different sessions run concurrently. A session
runs one turn at a time; a turn requested while another runs is queued when
the request asks for it, and otherwise returns `409` (see
[Turn queue](servers-and-connectors.md#turn-queue)). It listens on a Unix domain socket (the default),
a TCP port, or both.

Clients authenticated by the server token or Unix-socket permissions share
one user's authority over the server; there is no per-client session ACL.
Different sessions have separate histories, turn queues and event streams.
Clients loading the same session intentionally share its conversation and
approval state. A resume request naming a workspace must match the session's
workspace, including when the session is already open, or it returns `404`.
New sessions use the selected workspace's resolved provider and model settings
and sandbox; a loaded workspace has its own sandbox reload and read grants.

```bash
otto serve [--socket PATH] [--listen HOST:PORT]
```

`serve` accepts the same startup flags as the interactive frontends
(`--config`, `--cwd`, `--model`, `--base-url`, the deprecated
`--provider`, `--thinking`, `--sandbox`, `--shell-timeout`, `--max-output-bytes`) plus
`--socket` and `--listen`. It rejects `--ui`, `--prompt`, `--resume`, `--continue`,
`--archive`, and `--no-session`.

Release builds abort on panic: a panic in any task, including one handling a
single request, ends the whole `otto serve` process with SIGABRT (exit status
134) after Otto kills the sandboxed bash and MCP stdio process groups it
started. Run a long-lived server under a supervisor that restarts it, such as
[`scripts/otto-serve-connect.sh run`](#the-serve-and-connect-supervisor-script). Debug and test builds unwind.

### First-run configuration

After installing Otto with `make install` (see [installation](../../README.md#install-from-source)),
clients may launch `otto serve --listen 127.0.0.1:0 --cwd /absolute/workspace`
with an empty HOME and no selected model. The normal authenticated server is
available for setup; no provider is selected implicitly. Read
`GET /v1/config/providers` for provider tables and their configured models.
`GET /v1/config/models?provider=NAME` queries that provider's live catalog and
still requires its connection and credentials; ChatGPT does not support catalog
listing. API keys remain environment variables, never config values.

Review changes with `POST /v1/config/changes`, then confirm each returned `id`
with `POST /v1/config/changes/{id}`. Configure a provider, model and default
model. Confirmation returns `saved_restart_required`: stop and restart the
server with the same HOME/workspace and the required API-key environment (or
use `otto login` for ChatGPT). Until that restart, session creation returns
HTTP `503` with error code `configuration_required` and setup instructions.
`otto setup` followed by restart is also supported. Other frontends still
require a model at startup. Existing listener authentication and workspace
permissions also apply during setup; do not expose an unauthenticated proxy.

To verify setup after restarting, create a session with `POST /v1/sessions`;
missing credentials or an invalid configured model must be corrected rather
than silently falling back. Upgrades use the same stop/reinstall/restart
procedure described in the installation guide.

### Listener

With both `--socket` and `--listen` on the command line, Otto opens both
listeners and serves the same sessions on each: requests on the socket carry
no token, requests on the TCP port need the token. A browser uses the TCP
URL; local clients can use the socket at the same time. Otherwise one
listener is opened, resolved in this order: `--listen` > `--socket` >
`[server].listen` > `[server].socket` > the built-in default
`~/.otto/otto.sock`. The config file cannot select both.

**Callers.** Every caller that authenticates, with the TCP token or through the
Unix socket (including `otto acp --attach`), is the owner: it
has the authority of the account Otto runs as and may perform every operation,
including trusting directories and changing provider configuration. Keep the
token as secret as that account's credentials. Each API operation is still
authorized before its handler runs, and the `http_request` log line names the
caller as `principal=owner`.

**Unix socket.** Otto creates a missing parent directory with mode `0700`,
creates the socket file with mode `0600`, and refuses to start if a live
server already owns that path. A socket file left by a process that did not
run the normal shutdown, for example after `SIGKILL` or a second `SIGTERM`,
is replaced at the next start. File permissions are the only access control;
requests carry no token.

**Loopback TCP.** `--listen HOST:PORT` with a loopback host (`127.0.0.1`,
`::1`, or the literal `localhost`, mapped to `127.0.0.1` without a DNS lookup)
is the default TCP form and needs no configuration. Port `0` picks a free
port. Otto generates a random access token for the process and prints one
line to stdout before serving:

```
otto serve: http://127.0.0.1:PORT/?token=<token>
```

Use the printed host/port and token to connect API clients or the separate
otto-plus frontend. The printed URL itself no longer serves a page.

Every `/v1/` request must then carry `Authorization: Bearer <token>`; a
missing or wrong token returns `401` with `WWW-Authenticate: Bearer`. The
token is accepted from that header only, never from a query parameter. On a
loopback listener `/healthz` and `/metrics` need no token; `/` and `/assets/`
are not served. The random token is never logged and is not persisted;
restarting the process issues a new one. There is no CORS: a loopback port is
meant for a browser or client on the same machine.

**Token from the environment.** When `OTTO_SERVER_TOKEN` is set in the
environment of `otto serve`, that value is the access token instead of a
random one, so clients keep working across restarts. The value is 32 to 4000
bytes of printable ASCII without spaces (bytes 33 to 126); any other value
fails startup. The token is never written to stdout; the startup line then
reads `otto serve: http://HOST:PORT/ (token from OTTO_SERVER_TOKEN)`. The
variable is redacted from logs and errors and is not passed to sandboxed
commands. Rotating the token means changing the variable and restarting the
process. The variable is ignored when no TCP listener is bound.

**TLS.** `[server].tls_cert` and `[server].tls_key` name PEM files (`~` is
expanded) holding the certificate chain and the matching private key. Both
keys are set together, or neither; one without the other fails config
resolution. With both set, the TCP listener terminates TLS (TLS 1.2 and 1.3,
server certificate only, no client certificates, HTTP/1.1) and the startup
line uses `https://`. The files are read once at startup; a renewed
certificate needs a restart. The settings are ignored when only the Unix
socket is served.

```toml
[server]
listen = "0.0.0.0:8443"
tls_cert = "/etc/otto/tls/fullchain.pem"
tls_key = "/etc/otto/tls/privkey.pem"
```

**Non-loopback TCP.** A `--listen` or `[server].listen` host other than a
loopback address (for example `0.0.0.0`, `::`, or the machine's own IP
literal; hostnames are not resolved) is accepted only when TLS is configured
and `OTTO_SERVER_TOKEN` is set. Without both, startup fails with an error
naming the three requirements, so a plain-HTTP or random-token listener can
never be exposed to the network. On such a listener every path except
`GET /healthz` needs the token, including `/metrics` (its session labels
carry session ids and model names) and unmatched paths, which answer `401`
instead of `404`. Each `http_request` log line carries the client address as
`peer`. The token grants the same authority the host user has over the
server, including approving elevated Bash commands, so treat it as that
user's credential: restrict the source addresses that can reach the port with
a firewall or security group, inject the token from the supervisor's secret
store, and use a certificate the client trusts (a private CA or a public
one). A TLS handshake that fails or does not finish within 5 seconds is
closed and logged as `tls_handshake_failed` with the peer address. At most 64
handshakes are in flight at once; a further connection is closed and logged as
`tls_handshake_dropped`. Established connections are not limited by that
count.

```bash
OTTO_SERVER_TOKEN=<token> otto serve --cwd /srv/work --listen 0.0.0.0:8443
# prints: otto serve: https://0.0.0.0:8443/ (token from OTTO_SERVER_TOKEN)
curl -s --cacert ca.pem -H "Authorization: Bearer $OTTO_SERVER_TOKEN" https://host.example:8443/v1/info
```

### Workspaces

The startup workspace is loaded when the process starts. `[server].workspace_roots`
(default empty) lists directories under which another workspace may be
loaded on first use, from a session create naming it or from
`POST /v1/workspaces`. A path is admitted when it is absolute and, after
resolving symlinks and canonicalizing it, it is an existing directory that is either the startup
workspace, a descendant of one canonicalized root (a root itself is
admitted), or a trusted directory or its descendant. An unadmitted path returns `403` with `code: "WORKSPACE_NOT_ADMITTED"`;
a relative, missing, or non-directory path returns `400` with
`code: "INVALID_WORKSPACE"`.
With the default empty `workspace_roots` and no trusted directories, only
the startup workspace is admitted and the server behaves as a
single-workspace process.

Trusted directories come from the config file's `[projects]` tables, which
`otto trust <dir>` writes:

```bash
otto trust ~/src/app   # prints: Trusted /Users/me/src/app.
```

`otto trust` requires an existing directory and stores its canonical path.
Running it again for a directory already listed writes nothing. The write
takes a backup and is refused if another process changed the file in
between (see [Backups of the config file](getting-started.md#backups-of-the-config-file)).
A running `otto serve` re-reads `[projects]` on every admission, so a
directory trusted after it started is admitted without a restart. A trusted
path that no longer resolves is skipped. If the config file cannot be read
or parsed at that moment, no trusted directory is admitted until it can.
Trust affects admission only; the sandbox and approval policy are the same
for every workspace.

- `GET /v1/workspaces` lists loaded workspaces, startup first, then by path:
  `{"startup": path, "roots": [path...], "workspaces": [{"path", "open_sessions", "workflows"}...]}`.
- `POST /v1/workspaces {"path": "..."}` admits and loads a workspace,
  returning one `workspaces` entry: `201` when it was newly loaded, `200`
  when it was already loaded. With `"trust": true`, a path refused as
  `403 WORKSPACE_NOT_ADMITTED` is first recorded as trusted in the config
  file, as `otto trust` does, and then loaded; an already admitted path
  records nothing. A path that is not an existing directory is still `400`.
- `GET /v1/fs/dirs?path=...` lists the subdirectories of an absolute
  directory for the web UI's folder picker:
  `{"path", "parent", "roots": [path...], "dirs": [{"name", "path"}...]}`.
  Listing is limited to the home directory, the `workspace_roots`, and their
  descendants, checked after resolving symlinks; without `path` it lists the
  home directory. Only directory names are returned, names starting with `.`
  are left out, and `parent` is `null` at the top of a root. `400
  INVALID_PATH` for a relative path or a non-directory, `403
  PATH_NOT_ALLOWED` outside those directories.
- `POST /v1/sessions` takes an optional `"workspace"`; the default is the
  startup workspace. `GET /v1/sessions` and `GET/POST /v1/workflows` take an
  optional `?workspace=`; absent, `GET /v1/sessions` covers every loaded
  workspace, and the workflow list and start use the startup workspace.
  Run-scoped workflow routes (`GET /v1/workflows/{id}`, `.../resume`,
  `.../fork`, `.../cancel`, and the approval routes) search every loaded
  workspace for the run or request id. Every workspace value is admitted
  before any lookup, so an unadmitted path is `403` even for a read.
- `POST /v1/sandbox/reload` reloads every loaded workspace's sandbox and
  keeps the single-workspace `409` guard: it fails while any open session in
  any loaded workspace has a turn running, and also fails `409` if any
  workspace's reload itself errors. With one loaded workspace the response is
  unchanged, the reloaded sandbox object; with more than one, it adds a
  `workspaces` array with each workspace's own reload result. The sandbox
  object has the optional string array `inert_read_paths`, which lists
  `read_paths` entries that are not below any denied path, and the optional
  string array `skipped_deny_paths`, which lists built-in denylist entries that
  contain a writable root and are not applied. Each array is present only when
  it is non-empty.
- Each loaded workspace with a workflow database keeps its own file lock
  (`~/.otto/workflow-locks/{key}.lock`); a workspace whose lock is held by
  another process has workflows disabled for it (`GET/POST /v1/workflows`
  returns the same error a single-workspace server returns when workflows
  are disabled), while its sessions and turns still work.
- `DELETE /v1/workspaces?path=...` unloads a workspace and removes it from
  the persisted list: `204` on success, `404 WORKSPACE_NOT_FOUND` when it is
  neither loaded nor listed, `409 WORKSPACE_IS_STARTUP` for the startup
  workspace, and `409 WORKSPACE_IN_USE` while a session in it is open or a
  workflow run in it is active. A listed path whose directory no longer
  exists can still be removed. Nothing on disk under the workspace changes.
- A workspace newly loaded by any request that names it
  (`POST /v1/workspaces`, or a `workspace` on a session or workflow request)
  is added to `~/.otto/serve-workspaces.json` (`{"workspaces": [path...]}`,
  sorted, one file shared by every `otto serve` process). The startup
  workspace is not written. At startup, after the startup workspace, each
  listed path is admitted against the current `workspace_roots` and loaded;
  a path that is missing, not admitted, or fails to load is skipped with a
  `warning:` line on stderr and stays in the file. An unreadable or
  unparsable file is a warning and an empty list. If the file cannot be
  written, the workspace stays loaded and stderr gets
  `warning: cannot save workspace list: <error>`. `DELETE /v1/workspaces`
  removes an entry.

### Cross-workspace deliveries

A session can hand work to a **different** admitted repository instead of
editing its own. The entry point is the parent-only `start_delivery_task`
tool; ordinary sub-agents never receive it.

- Arguments: `target` (absolute path of an admitted primary Git checkout),
  `objective` (the goal for the isolated target task), optional `branch`
  (a new branch; defaults to `agent/<objective slug>`) and optional
  `source_refs` (bounded references already selected from this conversation,
  never filesystem paths).
- The source workspace stays read-only. Only a bounded, hashed handoff packet
  of the selected context reaches the target; the delivery runner never
  receives a source root or handle.
- The call asks you to authorize exactly one target together with its primary
  checkout, Git directory, HEAD/base, the new branch, the target worktree path
  and the handoff digest. Nothing is created before that decision, and any
  change to those facts requires a fresh authorization. The target must
  already be an admitted workspace (`otto trust` or a configured
  `workspace_roots` entry); an approval never persists trust.
- After approval Otto creates `TARGET/.otto/delivery-worktrees/<delivery-id>`
  and runs the task there as its own session and runner. A completion notice is
  appended to this conversation, and the result is readable with
  `GET /v1/sessions/{source_session}/deliveries/{approval_id}`, which returns
  only `approval_id`, `state` (`running`, `succeeded`, `failed`, `canceled`,
  `unknown`), `result` and `error`.
- A failure or a cancellation keeps the worktree for inspection. Recovery never
  repeats the worktree creation or the target turn: an interrupted delivery is
  reported as `unknown`, and retrying is an explicit new decision.
- GitHub credentials, network access, protected Git metadata writes, `push`
  and pull-request creation keep their own approval and permission boundaries;
  a delivery does not inherit them. Delivery is available in `otto serve`
  sessions (HTTP, ACP and the attached TUI); the local CLI frontends have no
  target registry and do not offer the tool.

### Turn queue

`POST /v1/sessions/{id}/turns` with `"queue": true` starts the turn at once
when the session has no running or queued turn and no compaction, and
otherwise appends it to the session's queue with status `queued`. Queued
turns start one at a time in arrival order, each after the previous turn
ends. A session queues at most 16 turns; the 17th request returns
`409 queue_full`. Without `"queue": true` the request returns
`409 turn_active` while a turn runs or is queued.

A queued turn has an id and can be read, followed and cancelled through the
turn routes. Its event stream sends nothing until it starts. Cancelling it
removes it from the queue and ends it `canceled`. A server-started wake turn
(`trigger` `task` or `recovery`) starts only when no turn is running or queued.
`POST .../compact` returns `409 turn_active` while a turn runs or is queued.
Queued turns are saved with the session before they are accepted (see
[Sessions](interactive-use.md#sessions)); `503 input_not_persisted` means the turn could not be
saved and was not queued. On shutdown, queued turns end `canceled` for the
clients connected then, but stay saved: when the session is next opened they
run in order, after the interrupted turn's automatic resume, with their
original turn ids, so `GET .../turns/{turn_id}` and an `input_id` retry find
them again. Cancelling a queued turn or deleting its session removes it for
good.

Each `POST .../turns` response carries the header `Otto-Turn-Id` with the
turn's id. In the event stream of a turn started by a client, the first frame
is `user_message` with the prompt `text`, and `image: true` when an image was
attached (the image data is not repeated). Delivered in-turn queued guidance also
emits `user_message` with its text, so clients retain a visible user transcript
entry after the queued banner clears. Wake turns have no initial `user_message`,
but may emit one if user guidance is delivered while they run.
The last frame of every turn is `turn_end` with `status` (`ok`, `error`, or
`canceled`) and, for `error`, the error text. Both frames are kept in the
turn's event buffer, so a reader that resumes with `?after=N` receives them
as well. Turn errors are redacted of configured secrets before they are
stored, returned, sent in `agent_error` or `turn_end`, or logged.

### Otto's own help and approval controls

The agent can consult `otto_help` for the user manual embedded in the installed
executable. An empty topic lists section titles; `capabilities` lists this
session's actual tool names and descriptions. Other topics match a section
title case-insensitively. A phrase that differs only by generic words such as
`commands`, `help`, or `topic` also opens the section when exactly one title
matches; for example, `Memory commands` opens `Memory`. Unknown or ambiguous
topics remain errors and include up to five related section titles when their
meaningful words overlap the index. User commands and HTTP/ACP APIs are not
automatically model-callable tools.

`approval_pending` queries the pending requests visible to this session. `approval_revoke` withdraws it by exact ID. It cannot approve, undo an
executed command, remove permanent grants, or affect another session.

While approval waits, you can ask why it is needed or ask Otto to withdraw it.
This conversation has only help, approval query/withdrawal and queue tools;
normal tasks keep waiting in the frontend's queue. In the TUI, press `Esc` to
dismiss the panel without deciding; `/approve` opens pending requests again.
Telegram supports this with Otto; other ACP agents keep the ordinary
queue.
Explicit Deny/No, `/deny` in chat and `/stop` continue to work.

### Session plan

For work of three or more steps, the main task model keeps a short plan with
the `update_plan` tool: up to 20 steps, each one line of at most 200
characters with the status `pending`, `in_progress`, or `completed`, and at
most one step `in_progress`. Each call replaces the whole plan; an empty list
clears it. The tool records progress only and runs nothing. Sub-agents do not
get it.

The plan is saved with the session as an `otto.plan` record, so it survives
Otto stopping, `--resume`, `/resume`, and `/fork` (a fork keeps the plan in
force at its cut). When a compaction summarizes the call that set the plan,
the summary carries the current plan as a `<plan>` checklist, so the model
keeps it.

Where it appears:

- **TUI:** one line above the status line, `Plan 2/5 · <step in progress>`,
  dropped first when the terminal is short. It works with `--attach`.
- **ACP:** a `plan` `session/update` after every change, with every entry at
  priority `medium`; `session/load` sends the current plan after the history.
- **HTTP:** a `plan_updated` SSE event (see [Events](servers-and-connectors.md#events)) and the session
  object's `plan` field.

### Active-task guidance and model follow-ups

In Telegram, speak naturally while Otto is working: “investigate
first, do not change files yet” can guide the active task; “after this, write a
report” can become a separate follow-up; a progress question can get a read-only
answer. This is enabled by default. A short, isolated model dialogue chooses
the action using the original task and recent progress. It has only routing and
help tools, cannot execute task tools or grant approval, and has a 30-second
limit. Ambiguous input can produce a clarification without changing the task.

Guidance is delivered unchanged at the next safe provider boundary, after the
current provider request and any already-requested tool batch finish. Acceptance
does not mean an in-flight tool was interrupted or its effects undone. Pending
guidance belongs to that exact run and is discarded on failure/cancellation;
it cannot leak into the next task. While approval waits, the existing
approval-only dialogue continues to apply.

The main task model can call `task_queue` with a nonblank `prompt` of at most
32,000 UTF-8 bytes to schedule a separate follow-up within your objective. It
uses FIFO order with at most 16 accepted follow-ups per logical task, including
follow-ups scheduled by other follow-ups. These are labelled as agent-originated
context and cannot create user authority. ACP/server keep them behind approval;
denial, cancellation, or failure clears remaining model follow-ups. Direct
terminal prompts that end awaiting approval clear remaining follow-ups with a
visible, stored notice; resubmit them after resolving approval. Accepted
follow-ups are also saved with the session (see [Sessions](interactive-use.md#sessions)): when
the Otto process stops during the task, they run, in order, after the turn
that automatically resumes it, and a step that already ran is not run again.
A follow-up that cannot be saved is not queued and `task_queue` returns an
error. The `otto serve` and ACP turn queues are saved too (see
[Turn queue](servers-and-connectors.md#turn-queue)).

Tool schemas and the applicable operation policy accompany each task request
outside the mutable transcript. Compaction and session reload preserve this
capability information; `otto_help` describes the installed version and actual
tools. Parent-to-child guidance continues to use `agent_send`.

For deterministic chat control, use `/steer <guidance>` or `/queue <prompt>`.
Both accept nonblank text of at most 32,000 UTF-8 bytes. `/steer` refuses idle or
approval-waiting tasks; `/queue` starts immediately when idle and otherwise
waits in the existing chat queue, including during approval. Queued prompts get
their own answer. `/stop` clears pending chat input and cancels active work.
An old agent queues natural messages using the previous behavior and reports
explicit steering as unsupported; upgrade and restart both Otto and its
connector to use natural routing.

### Approvals inside a turn

When a prepared tool call in a serve turn waits for approval, the turn does not end. Its stream emits `approval_requested` with
`approval_id`, `tool_call_id`, `command`, and `justification`, and the turn
stays `running` while it waits; other turns queue behind it.
`POST /v1/sessions/{id}/approvals/{approval_id}` with `{"decision":"allow"}`
or `{"decision":"deny"}` decides it, from any client:

- The first decision returns `200 {"decision": ...}`. A later decision for
  the same request returns `409 approval_decided`; an id that is not waiting
  returns `409 approval_failed`.
- `allow` authorizes the prepared call and the server executes that exact call
  inside the same turn, with the same turn id and stream. The client sends no prompt.
- `{"decision":"allow","always":true}` first excludes the command from the
  sandbox as `/approve <id> always` does, then allows it as above. The `200`
  body adds `"always":true` and `followup`. A command that cannot be excluded
  returns `400 approval_always_failed`, and the approval keeps waiting.
- `deny` prevents the prepared action and later calls in that batch from running;
  the model receives the decision before replanning.
- An execution request without a decision expires after 10 minutes.
- Each outcome emits `approval_decided` with `approval_id` and `decision`
  (`allow`, `deny`, or `timeout`). Cancelling the turn during the wait ends
  it `canceled`.

The neutral `/v1/approvals` API also exposes scoped queues, decisions with
feedback, withdrawal, and application outcomes. Approval does not imply that
the operation succeeded.

### Web UI

The browser frontend now lives in
[otto-plus](https://github.com/baiyuqing/otto-plus). Follow its README for
setup, building, and hosting; the [UI guide](https://github.com/baiyuqing/otto-plus/blob/main/docs/web-ui.md)
describes the migrated features. Otto retains the authenticated HTTP API.
It no longer embeds static files: `/` and `/assets/` return `404`, and
`serve --open` is removed. Start `otto serve --listen 127.0.0.1:8787`, then
open the separate frontend URL with the token from Otto's startup output.
The frontend's same-origin proxy forwards `/v1` with bearer authentication.

### HTTP API

API endpoints are under `/v1/`; `/`, `/assets/`, `/healthz`, and `/metrics`
are served at the root. Request and error bodies are JSON.

| Method and path | Behavior |
| --- | --- |
| `GET /v1/workspaces` | List loaded workspaces, startup first: `{"startup", "roots", "workspaces": [{"path", "open_sessions", "workflows"}...]}`. |
| `POST /v1/workspaces` | Admit and load a workspace (`{"path":"...","trust":false}`). `201` when newly loaded, `200` when already loaded. `400 INVALID_WORKSPACE` or `403 WORKSPACE_NOT_ADMITTED` otherwise; with `"trust": true` a not-admitted directory is recorded as trusted in the config file, then loaded. Returns one `workspaces` entry. |
| `GET /v1/fs/dirs?path=...` | List subdirectories for the folder picker: `{"path", "parent", "roots", "dirs": [{"name", "path"}...]}`. Limited to the home directory and `workspace_roots`; `400 INVALID_PATH`, `403 PATH_NOT_ALLOWED`. |
| `DELETE /v1/workspaces?path=...` | Unload a workspace and remove it from `~/.otto/serve-workspaces.json`. `204` on success; `404 WORKSPACE_NOT_FOUND`, `409 WORKSPACE_IS_STARTUP`, or `409 WORKSPACE_IN_USE` otherwise. |
| `GET /v1/workspaces/diff?workspace=<path>` | Read-only changes of a working directory (default the startup workspace) against `HEAD`, or the empty tree before the first commit: staged, unstaged, and untracked files under that directory, ignored files excluded. `{"workspace", "repository", "branch", "files": [{"path", "old_path", "status", "binary", "patch", "truncated"}...], "truncated"}`. `status` is `modified`, `added`, `deleted`, `renamed`, or `untracked`; paths are relative to the directory. `repository:false` when the directory is not in a git work tree. git runs through the workspace's sandbox with external diff and textconv drivers disabled. Limits: 256 KiB of patch per file, about 1 MiB in total, patches for the first 200 untracked files, 10 s for all git commands. `400`/`403` as `POST /v1/workspaces`; `501 diff_unavailable` without a usable sandbox; `500 git_failed`; `504 git_timeout`. |
| `POST /v1/sessions` | Create a session (`{}`, optionally `"workspace":"<path>"`, default the startup workspace) or open one by id (`{"resume":"<id>"}`, 32 lowercase hexadecimal characters, looked up in `workspace` if given, else every loaded workspace, including sessions older than the newest 20 listed). Creation may include a stable `"request_id"` (1 to 128 ASCII letters, digits, `.`, `_`, `:`, `-`); a retry after a lost response returns the original session with `200`, even after restart. It cannot accompany `resume`. `201` for a new session, `200` for an existing one. Returns the session object. |
| `GET /v1/status` | `text/event-stream` of `event: status` snapshots of every session open in this process, in every loaded workspace: `{"sessions":[{"id","workspace","turn","turn_id","queued","approvals","tasks"}...]}`, sorted by workspace, then id. `turn` is `running`, the last finished turn's `ok`, `error`, or `canceled`, or `null` before the first turn; `turn_id` is the running turn's id, else the newest turn's; `queued` counts queued turns; `approvals` counts Bash approvals waiting for a decision; `tasks` counts queued or running sub-agent tasks. The current snapshot is sent on connect and again whenever it changes; there is no replay. The stream ends when the server shuts down. |
| `GET /v1/sessions?workspace=<path>` | List sessions: on-disk sessions merged with sessions currently open in this process, each flagged `open`, with `last_user_text` (at most 80 characters) and `modified` (RFC 3339). Without `workspace`, every loaded workspace; with it, that workspace only. |
| `GET /v1/sessions/{id}` | Return one session's info, re-opening it first when the server closed it for being idle (see [Session lifetime](servers-and-connectors.md#session-lifetime)). `404` if the session does not exist, or was closed by `DELETE`. |
| `PATCH /v1/sessions/{id}` | Rename an open session with `{"name":"dev"}`. `409 turn_active` while a turn is running. |
| `DELETE /v1/sessions/{id}` | Cancel any active turn, close the session, `204`. This does not delete its history; the session is not re-opened by later requests. See [Session lifetime](servers-and-connectors.md#session-lifetime). |
| `DELETE /v1/sessions/{id}/permanent` | Permanently delete a session only when the JSON body is `{"confirm":"<id>"}` with the exact session id. Deletes its history, sub-agent transcripts, sidecars, and lease/queue state; this cannot be undone. Invalid or mismatched confirmation returns `400`, an unknown session returns `404`, busy or lease conflicts return `409`, and I/O failure returns `500`. |
| `POST /v1/sessions/{id}/archive` | Archive an idle session, as TUI `/archive` does: close it and move its file, with sub-agent transcripts, to the `archive/` directory beside it, returning `200 {"archived_path"}`. Timer, inbox, input, follow-up and turn sidecars are removed. The session leaves `GET /v1/sessions` and cannot be resumed by id. An unknown session returns `404`, busy or lease conflicts return `409`, and I/O failure returns `500` with the session left on disk and resumable. |
| `GET /v1/sessions/{id}/fork-points` | `{"fork_points":[{"entry_id","prompt"}]}`: the user prompts that start a turn, oldest first. Empty for a session with no file yet. |
| `POST /v1/sessions/{id}/fork` | Write a new session from this session's history before the turn that `{"before":"<entry_id>"}` starts, open it, and return it, `201`, with `prompt` (the text of that message) and `forked_from`. Without `before`, the copy keeps everything up to the last finished turn. The source session is not changed, and a running turn on it is left out. An optional stable `"request_id"` returns the original fork with `200` on retry, including after restart; it reopens the source from disk when needed. Reusing that id with another source or `before` returns `409 request_id_conflict`. `409 invalid_fork_point` when `before` is not a fork point or the session has no history yet. |
| `GET /v1/sessions/{id}/history?before_turn=<turn_id>` | Return the session's message history. With `before_turn`, return only the messages that existed when that turn started, so a client can replay a running turn's events from sequence `0` without showing its prompt and finished steps twice; `404` when the turn is not retained or has not started. |
| `POST /v1/sessions/{id}/approvals/message` | Restricted dialogue during an unapproved request: `{"text":"..."}`. Returns `null` when none waits, otherwise `{"text","queued"}`. A true `queued` asks the client to put the original message in its normal task queue. Never approves. |
| `POST /v1/sessions/{id}/approvals/{approval_id}` | Decide an elevated Bash command a running turn waits for: `{"decision":"allow"}`, `{"decision":"allow","always":true}` or `{"decision":"deny"}`. `always` also writes the command to `excluded_commands` and adds `"always":true` and a `followup` prompt to the response; `400` with `deny`, and `400 approval_always_failed` when the command cannot be excluded (the approval keeps waiting). `200 {"decision"}` for the first decision; `409 approval_decided` when it was already decided; `409 approval_failed` when the id is not waiting. See [Approvals inside a turn](servers-and-connectors.md#approvals-inside-a-turn). |
| `POST /v1/sessions/{id}/turns` | Start a turn: `{"text":"...","stream":true}`. An optional `input_id` (1 to 128 ASCII letters, digits, `.`, `_`, `:`, `-`) makes a retry safe: the same `input_id` in the same session returns the turn it already started while the server remembers it (its 256 most recent per session), and `409 duplicate_input` once only the transcript holds it, for example after a restart. It is unrelated to the `X-Request-Id` log-correlation header. An optional `image` carries base64 `data` and `mime_type` (`image/png`, `image/jpeg`, or `image/webp`); `text` may be empty when `image` is present. `"queue": true` queues the turn while another runs (see [Turn queue](servers-and-connectors.md#turn-queue)). The response header `Otto-Turn-Id` names the turn. `stream` defaults to `true` and returns a `text/event-stream` response starting at sequence `0`; `stream:false` waits for the turn to finish and returns its summary instead. An optional `origin` (`terminal`, `web`, or `chat:<platform>:<chat id>`) is the turn's origin for [`schedule`](safety-and-tools.md#schedule); it defaults to `web`, and an invalid value, or a chat platform that differs from the optional `channel` (`telegram`), is `400`. `409 turn_active` while a turn runs and `queue` is not set; `409 queue_full` with 16 turns queued; `503 input_not_persisted` when a queued turn could not be saved. |
| `GET /v1/sessions/{id}/turns/{turn_id}` | Return a turn summary. The session's queued turns and its most recent started turn are retained. |
| `GET /v1/sessions/{id}/turns/{turn_id}/events?after=N` | Re-read a retained turn's event stream from sequence `N+1`; also honors the `Last-Event-ID` header. A queued turn's stream sends nothing until it starts. |
| `POST /v1/sessions/{id}/turns/{turn_id}/cancel` | Cancel the turn, `202`. A queued turn is removed from the queue. |
| `GET /v1/sessions/{id}/skill-manager` | Authenticated, session-scoped management snapshot, including discovered disabled skills. Returns `{global_enabled,skills:[{skill:{name,description,contract,directory,path},enabled,loaded}]}`; `contract` is `null` or `{input,output}`. `global_enabled` reflects the latest persisted `skills.enabled` (default true), even for an empty inventory; row `enabled` is the per-skill preference, and `loaded` reflects the fixed current-session catalog. Discovery inventory remains available when globally disabled; no field implies hot reload. `404` for an unknown session; `500 skill_config_read_failed` if configuration cannot be read. |
| `GET /v1/sessions/{id}/skill-manager/{name}` | Read-only details: `{skill,body,contract_check}`. `skill` has the same metadata shape as the list's row `skill`; `contract_check` is `null` or a string. Reads instructions through the confined skill loader, never executes them. Encode the skill name as one URL path segment. `404` for an unknown session; `400 skill_read_failed` for an unknown skill or a confined read failure. |
| `POST /v1/sessions/{id}/skills/{name}/enable` and `/disable` | Persist a skill's configured state through the shared configuration writer. Returns `{name,enabled,restart_required:true}`; restart the serving Otto process to apply it. Does not remove skill files or change the current session catalog. These writes require the same server authentication as other management routes; a failed write must not be shown as a confirmed state change. |



| `GET /v1/sessions/{id}/wake-replies` | The assistant text of this session's finished wake turns, as `{notices:[{id,text}],last}`. Pass `last` as `after` to read only newer replies; the 20 newest are kept. `otto acp --attach` reads it to send wake replies to its ACP client. |
| `POST /v1/sessions/{id}/compact` | Run one context compaction now, optionally with `{"focus":"..."}`, and return the compaction result (`noop:true` when there was nothing to compact). `409 turn_active` while a turn runs or is queued, or another compaction runs; `409 compaction_failed` when the compaction fails and the previous context stays in effect. Closing the request cancels the compaction. |
| `POST /v1/sessions/{id}/inputs` | Admit ordinary `{"text":"..."}` without model routing or implicit queuing. Returns `"steered"` when bound to the active run's next safe boundary, `"idle"` when the caller may explicitly start a new turn, or `"approval_pending"` while an approval owns input. Nonblank text is limited to 32,000 UTF-8 bytes. An optional `input_id` admits a retry once (`409 duplicate_input` after delivery). `503 input_not_persisted` when the guidance could not be saved; it was not accepted. |
| `POST /v1/sessions/{id}/tasks/steer` | Explicit steering endpoint. It returns `{id,text,status:"queued",input_id}` immediately, so clients display the exact guidance while it waits for the next safe boundary. An optional `input_id` returns the same message for a retry while it is queued and `409 duplicate_input` after delivery; `503 input_not_persisted` when it could not be saved. |
| `GET /v1/sessions/{id}/tasks/steer` | Lists transient steering items and their `queued`, `delivered`, `withdrawn`, or `discarded` status for the active run. |
| `DELETE /v1/sessions/{id}/tasks/steer/{message_id}` | Withdraws exactly one still-`queued` steer. Delivered guidance is append-only session history and cannot be removed; a non-queued or unknown item returns `409 task_steer_withdraw_failed`. |
| `POST /v1/sessions/{id}/tasks/steer/interrupt` | Stops the running provider or tool call and delivers every `queued` steer inside the current turn, which then continues with the next model call. No body. Returns `{"queued_guidance": N}`, the number of messages delivered. The stopped tool call is recorded `cancelled`; the remaining tool calls of that response are settled `not_started`; a stopped model call appends no assistant message. The stream carries `step_interrupted` before the delivered `user_message` frames. `409 no_queued_guidance` when the active run has nothing queued or no turn runs; `409 approval_pending` while an approval waits. |
| `POST /v1/sessions/{id}/tasks/control` | Interpret `{"text":"...","channel":"telegram"}` (`channel` optional, `telegram`) using the restricted control dialogue. Returns null if no active dialogue applies, otherwise `{"text","queued"}`. `queued:true` means the caller must submit the original message as a queued turn; this route does not itself admit a follow-up. `409 task_control_failed` reports routing failure. |
| `GET /v1/sessions/{id}/context` | Return what the next provider request contains: model, context window, compaction threshold, estimated and last reported input tokens, and sections of items with estimated tokens and text. `409 context_unavailable` when the redaction boundary is closed. |
| `GET /v1/sessions/{id}/trace` | Return the session's turns, provider requests, tool calls, compactions and sub-agent tasks as nested spans with start and end times, status, provider timing and token usage. The JSON holds no message or result text; tool labels carry an argument preview of at most 60 characters and sub-agent labels the first 40 characters of the task description. `404` for an unknown session; `409 trace_unavailable` when the redaction boundary is closed, the session has no file (`--no-session`), or the session file cannot be read. |
| `GET /v1/sessions/{id}/trace/{span_id}` | Return the text of one span from the trace as titled sections: a turn's `input`; a provider request's `input`, `request` (token estimates) and `reply`; a tool's `arguments` and `result`; a compaction's `summary`; a sub-agent's `prompt` and `result`. Empty sections are omitted. The text is the session content as stored, which the agent redacts before writing it. `404` for an unknown session or span; `409 trace_unavailable` as above. |
| `POST /v1/sessions/{id}/model` | With `{"model":"<provider>/<model_id>"}`, rebuild the session on that model and return the session; it makes no model request and never writes the configuration file, so other sessions and the next start keep their model. `400` for a missing, empty, malformed or unknown selector; `409 turn_active` while a turn runs, or `409 model_switch_unavailable` when the session withholds dynamic content or policy forbids a dynamic model. `/v1/sessions/{id}/profile` and `/v1/config/profiles*` are removed and return `404`. |
| `POST /v1/sessions/{id}/thinking` | With `{"thinking":"<level>"}` (`unset`, `default`, `low`, `medium`, `high`, `xhigh` or `max`), rebuild the session with that thinking level and return the session; it makes no model request and never writes the configuration file. `400 invalid_thinking` for bad JSON, an empty or an unknown level; `409 turn_active` while a turn runs, or `409 model_switch_unavailable` when the session withholds dynamic content or policy forbids a dynamic model. |
| `GET /v1/config/providers` | Return `{"default_model","providers":[...]}`. Each provider has `name`, `type`, `builtin`, the optional `base_url`, `api_key_env`, `api_version` and `workspace_id_env`, and `models`, a list of `{model_id, selector, default, thinking?, context_window?, compaction_window?}`. Built-in providers are always listed. API keys appear only as environment variable names. |
| `GET /v1/config/providers/{name}` | Return one provider in the same shape; `404 provider_not_found`. |
| `GET /v1/config/models?provider=<name>` | Query the provider's model catalog: `{"provider","models":[ids]}`. `501 models_unsupported` for `chatgpt`, `400 invalid_provider` for an unsupported provider type, `400 api_key_unavailable` when the key variable is unset, `404 provider_not_found`, `502 models_failed` when the endpoint request fails. |
| `POST /v1/config/changes` | Prepare an approval-gated change to the user configuration. Body `{"kind":...}` with `set_default_model{model}`, `set_provider{provider,type,base_url?,api_key_env?,api_version?,workspace_id_env?}`, `set_provider_field{provider,field,value}`, `remove_provider{provider}`, `set_model{model,thinking?}`, `set_model_field{model,field:"thinking",value}` or `remove_model{model}`. Returns `{"id","expires_in_seconds","operation","target","field"?,"diff"}`; the preview holds no API-key values. `400 invalid_provider`, `invalid_field` or `invalid_change`; `409 model_in_use` when `default_model` or `approval.reviewer_model` selects the target. A prepared change is confirmed with `POST /v1/config/changes/{id}` (`404` unknown or used id, `409` configuration changed since the preview, `410` expired; a restart is required afterwards) or cancelled with `DELETE /v1/config/changes/{id}` (`204`). |
| `GET /v1/config/providers` | List providers with their model tables. API keys appear only as environment variable names, never as values. |
| `GET /v1/sessions/{id}/tasks` | List the session's sub-agent tasks in creation order. |
| `GET /v1/sessions/{id}/tasks/observer` | Read a bounded snapshot of the current parent's live and recorded children for the TUI observer. Returns `{tasks:[{row,entries,missing}],truncated}`; `truncated:true` indicates that only recent tasks are included. Entry tails are bounded and contain visible assistant text and tool activity, not reasoning. Uses ordinary session/workspace access checks, grants no task-control or approval authority, and requires no extra configuration. |
| `GET /v1/sessions/{id}/tasks/{task_id}` | Return one task plus its child session's history. |
| `POST /v1/sessions/{id}/tasks/{task_id}/cancel` | Cancel a running task and return it. `409 task_done` if it already finished. |
| `GET /v1/tasks?status=&workspace=&parent_session=&limit=&before=` | List recorded sub-agent tasks from `~/.otto/tasks.db`, from any session and any Otto process, newest first by `created_at`. `status` is `queued`, `running`, `succeeded`, `failed`, `canceled`, or `interrupted`; `workspace` is an exact path; optional `parent_session` selects one parent session; `limit` defaults to 100 and is capped at 500; `before` is the `next_before` cursor of the previous page (empty when there are no older rows). Each task carries `cancelable: true` only when this server has its parent session open and the task is queued or running. |
| `GET /v1/tasks/{parent_session}/{task_id}` | Return one recorded task under `task`, its child transcript as `history`, and `transcript_missing: true` (with an empty `history`) when the transcript file is absent or unreadable. `404` for an unknown task. |
| `GET /v1/sessions/{id}/timers` | List the session's outstanding timers, soonest first: `id`, `fire_at`, and `message`. |
| `POST /v1/sessions/{id}/timers/{timer_id}/cancel` | Cancel one outstanding timer and return it. `404` if no timer has that id. |
| `POST /v1/schedule/claim` | Body `{}`. Claims up to 10 due [`schedule`](safety-and-tools.md#schedule) items with origin `web` for 120 s and returns `{"items":[{"id":"s7","due_at":<unix ms>,"text":"..."}]}`; a claimed item is not returned again until the claim expires. `503 schedule_unavailable` when `~/.otto/schedule.db` could not be opened. |
| `POST /v1/schedule/{id}/delivered` | Body `{"due_at":<unix ms>}` from the claim. Records the delivery and returns `{}`, also when it was already recorded. `404` for an id that is not `s<n>`, `400` for a bad body, `503` as above. |
| `GET /v1/sessions/{id}/mcp` | List the session's MCP servers and their connection state, in configuration order. |
| `GET/POST /v1/workflows?workspace=<path>` | List that workspace's workflow runs newest first (default the startup workspace), or start one there with `{"name":"...","input":"..."}`. The list takes `limit` (default 50, at most 200) and `before` (the `next_before` cursor of the previous page, empty when there are no older runs) and returns `{"runs":[...],"next_before":...}`. |
| `GET /v1/workflows/{id}` | Return a run, its steps, and its approval requests. Searches every loaded workspace for the run id. |
| `GET /v1/workflows/{id}/events?after=N` | Replay durable workflow SSE events after sequence `N`; unlike turn events, replay survives process restart. |
| `POST /v1/workflows/{id}/resume` | Resume a run; `{"retry":"step-id"}` explicitly retries one `interrupted` or `failed` step and re-admits the steps that failure canceled. |
| `POST /v1/workflows/{id}/fork` | Create a new run from a committed step boundary with `{"after_step":"step-id"}`. |
| `POST /v1/workflows/{id}/cancel` | Cancel a run after its active attempts stop. |
| `POST /v1/workflows/requests/{id}/approve` | Approve a durable workflow gate. Searches every loaded workspace for the request id. |
| `POST /v1/workflows/requests/{id}/reject` | Reject a durable workflow gate: the run is canceled and its in-flight attempts stop. Searches every loaded workspace for the request id. |
| `POST /v1/sandbox/reload` | Re-read `[sandbox]` and apply it to every loaded workspace; `409` while any session in any loaded workspace has a turn in flight, or `409 sandbox_reload_failed` when any workspace's reload fails; `501` when the process has no reloadable sandbox. With one loaded workspace, returns the sandbox object now in effect, as before; with more than one, the startup workspace's sandbox fields stay at the top level and an added `workspaces` array carries each workspace's own `{"workspace", "sandbox", "error"}` (`sandbox` and `error` are mutually exclusive). The sandbox object carries the optional arrays `inert_read_paths` (`read_paths` entries not below any denied path) and `skipped_deny_paths` (built-in denylist entries that contain a writable root); each is present only when non-empty. |
| `POST /v1/sessions/{id}/sandbox/changes` | Write one `[sandbox]` change to the serve process's configuration file and reload every loaded workspace: `{"change":"allow_read_path"\|"network"\|"exclude_command","value":"..."}`. A read path is resolved to an absolute, symlink-free path first. `200` returns the sandbox object as `POST /v1/sandbox/reload` does, plus `change_summary`, `config_path` and `followup` (the prompt that makes the model retry a refused command; `null` for `network deny`). `400 invalid_sandbox_change`; `404` unknown session; `409 turn_active` while any session has a turn in flight; `409 sandbox_change_failed` (for example the path is already readable, or the reload failed and the change was rolled back); `409 sandbox_reload_failed` when another workspace fails to reload; `501` without a reloadable sandbox. |
| `GET /v1/info` | Process-level static info: workspace, `provider` (provider name), `provider_type`, `model` (model ID), sandbox summary, and `models`, the configured selectors (`<provider>/<model_id>`). |
| `GET /v1/usage?session_id=<id>` | Aggregate persisted provider token usage across all sessions, or one session when `session_id` is set. |
| `GET /v1/usage/daily?days=30&session_id=<id>` | Return zero-filled daily usage and range totals for 1 to 365 UTC days. `session_id` is optional. |
| `GET /v1/openapi.yaml` | The OpenAPI 3.1 document for this API. |
| `GET /healthz` | `{"status":"ok","sessions_open":N}`. |
| `GET /metrics` | Prometheus text-format metrics. |

Workflow routes report failure by kind: `400` for an invalid request or page
limit, `404` when the workspace has no such run, request, or definition, `409`
when the run cannot change state as asked, and `503` when the workflow store is
unavailable. `501` still means the workspace has no workflow runtime.

A turn keeps running after its client disconnects; `POST .../cancel` is the
only way to stop it.

For session creation and fork, choose a new `request_id` for each intended
command and keep it when retrying. The id is scoped to the workspace and
command type and is separate from the `X-Request-Id` logging header. A retried
creation returns the original result while that session still exists. An
idempotent creation writes its session file before acknowledging the request;
ordinary creation still writes the file on first use.

### Memory management

`/v1/sessions/{id}/memory/...` gives a web or desktop client the
[memory lifecycle](memory-and-extensions.md#record-states-and-times) of the
built-in store: the operations of `/memory` and `/remember`, with the same
scopes (the user scope and the session's workspace scope) and the same
checks. The bearer token identifies the operator, so a write applies
immediately as a human change, like `/remember`, and a review is recorded as a
human decision with the actor `http-client`. The model cannot call these
routes. Retiring a record here produces the same
[retraction notices](memory-and-extensions.md#retraction-notices) in later
requests of every session that saw it. The full schemas are in
`GET /v1/openapi.yaml`.

| Method and path (under `/v1/sessions/{id}/memory`) | Effect |
| --- | --- |
| `GET /records` | One page `{records,next_cursor}`. Query: `scope` (`current`, `user`, `workspace`, `all`), `state` (repeatable; `active`, `needs_review`, `superseded`, `retracted`, `all`; default active), `kind` and `label` (repeatable), `updated_after` and `verified_after` (RFC 3339 or an age such as `30d`), `unverified`, `review_due`, `limit` (1 to 100, default 50), `cursor`. |
| `POST /records` | Remember `{scope,kind?,key?,text,review_in?}`; `201` with the record. `scope` is `user` or `workspace`; `kind` defaults to `note`. |
| `GET /records/{record_id}` | The record in any state. |
| `DELETE /records/{record_id}` | Forget it; returns `{tombstone,previous_revision}`. |
| `POST /records/{record_id}/state` | `{state,superseded_by?,reason?,expected_revision?}`: retract, flag for review, reactivate, or supersede (`superseded_by` names an active record in the same scope and is required only then). |
| `POST /records/{record_id}/verify` | `{review_in?,expected_revision?}`: record a human verification; a `needs_review` record becomes active; `review_in` sets the next deadline, omitted clears it. |
| `GET /records/{record_id}/history` | `{revisions,skipped}`, oldest first. |
| `GET /records/{record_id}/diff?from=N[&to=M]` | `{from,to}`; `to` defaults to the current revision. |
| `POST /records/{record_id}/restore` | `{revision,expected_revision?}`: make an earlier revision current as a new revision. |
| `POST /records/{record_id}/propose` | `{evidence}`: the model proposal of [`/memory propose`](memory-and-extensions.md#memory); returns `{outcome,candidate?,detail?}` with `outcome` `proposed`, `no_change`, `rejected` or `unavailable`. A proposal is only a pending candidate. |
| `GET /search?q=TEXT` | Active records matching the query, `{records}`. |
| `GET /candidates` | Candidates awaiting a human decision, `{candidates,next_cursor}`; `limit` 1 to 50 (default 20), `cursor`. Each candidate has its `action` (`create`, `update`, `forget`, `retract`, `supersede`, `correct`, `needs_review`), `target_id`, `reason`, `origin`, optional `advice` and the full `proposed` record. |
| `POST /candidates/{candidate_id}/review` | `{decision}` (`accept` or `reject`); returns `{decision,candidate,record?,tombstone?}`. |

Records carry `state`, `state_reason`, `superseded_by`, `revision`, the five
times and two derived flags: `review_due` and `effective` (whether normal
recall may use the record now). Writes that name a record take an optional
`expected_revision`; when the record has moved on they fail with `409
memory_conflict` and change nothing, so two windows cannot overwrite each
other. Without it the current revision is used, as the slash commands do.

Memory errors: `404 not_found` for an unknown session, record, revision or
candidate; `409 memory_conflict` for a stale revision or a candidate that was
already decided; `503 memory_unavailable` when memory is disabled or not
usable in the session (and for `propose` when no proposal model is
available); `501 memory_unsupported` when a
[plugin memory source](memory-and-extensions.md#plugin-memory-source) is
selected, which manages its own records.

### Session lifetime

The server keeps only the sessions in play. A session with no request, turn
start, or turn end for `[server].session_idle_timeout` (default `"30m"`) is
closed, and when more than `[server].max_open_sessions` (default `64`) sessions
are open the least recently used ones are closed first; the sweep runs once a
minute. A setting of `"0"`, or `0` for the cap, turns that rule off. Both are on
by default and need no restart to change. Busy sessions are never closed,
whatever their age or the count: a running or queued turn, a compaction, a
waiting approval, an unfinished sub-agent task, or an undelivered notification
keeps a session open.

Closing a session this way is invisible to a client. The session keeps its file,
its history, and its saved queued turns, and the next request that names it —
`GET /v1/sessions/{id}/history`, `POST /v1/sessions/{id}/turns`, any other
session route — re-opens it from disk and answers as before. The one thing an
eviction drops is the in-memory event buffer of the session's most recent turn,
so `GET /v1/sessions/{id}/turns/{turn_id}/events` for that turn answers `404`
afterwards, exactly as it already does for an older turn. A session older than
the last 1024 evictions is treated as one that was never opened: it answers
`404` until a client opens it with `POST /v1/sessions` and `{"resume":"<id>"}`.
`GET /v1/sessions` still lists a closed session with `"open": false`, and
`/healthz` counts only the open ones.

`DELETE /v1/sessions/{id}` is not an eviction: it closes the session for good,
and no later request re-opens it. Use `POST /v1/sessions` with
`{"resume":"<id>"}` to open it again.

### Session object

```json
{
  "id": "...",
  "name": "dev",
  "workspace": "...",
  "provider": "...",
  "provider_type": "...",
  "model": "...",
  "context_window": 0,
  "usage": {
    "input_tokens": 0,
    "output_tokens": 0,
    "cached_input_tokens": 0
  },
  "context_input_tokens": 0,
  "sandbox": {
    "mode": "...",
    "network": "...",
    "bash_available": true,
    "summary": "..."
  },
  "turn": { "id": "...", "trigger": "user", "status": "..." },
  "forked_from": { "session_id": "...", "entry_id": "..." },
  "plan": [{ "text": "...", "status": "in_progress" }]
}
```

`plan` is the [session plan](servers-and-connectors.md#session-plan), omitted when there is none.

`forked_from` is present only on a fork: the source session and the user
message the fork stops before (`entry_id` is absent for a fork of the whole
history).

`usage` totals the whole session, including the tokens a sub-agent task
reported when it finished: the notification that carries them is part of the
session history, so the totals survive resume, `--attach` and forks.

`turn` is `null` when no turn has run yet on this session. `trigger` is
`user` for a turn started by `POST .../turns`, `task` for one the server
started to deliver a finished sub-agent task or another notification, and
`recovery` for one that resumes a turn interrupted when the previous Otto
process stopped. The session object
never includes the session's file path; the session ID is the handle used by
every endpoint.

### Turn summary

```json
{
  "id": "...",
  "trigger": "user",
  "status": "ok",
  "error": "",
  "text": "...",
  "usage": { "input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0 },
  "usage_present": true,
  "started_at": "...",
  "finished_at": "..."
}
```

`status` is one of `queued`, `running`, `ok`, `error`, or `canceled`. `error` is omitted
unless `status` is `error`; `finished_at` is omitted while the turn runs.

### Events

Each SSE frame carries `id: <sequence>`, `event: <name>`, and a JSON `data:`
payload. Event names are the `agent.Event` type names: `agent_started`,
`text_delta`, `reasoning_delta`, `tool_call_started`, `tool_call_finished`, `provider_usage`,
`provider_retry`, `compaction_planned`, `compaction_started`, `compaction_completed`,
`compaction_warning`, `memory_warning`, `agent_finished`, and `agent_error`,
plus `notification` when a sub-agent task finishes, `plan_updated` after a
tool call changes the [session plan](servers-and-connectors.md#session-plan), carrying the whole plan as
`steps: [{text, status}]` (empty when it was cleared), and `step_interrupted`
when `POST .../tasks/steer/interrupt` stopped the running step, carrying
`queued_guidance: N`, the number of `user_message` frames that follow before
the turn continues. `reasoning_delta` carries
`text` like `text_delta`. `provider_retry` carries
`retry: {attempt, max_attempts, delay_ms, reason, interrupted_output}`.
`interrupted_output` defaults to false for older events; true marks the preceding
partial response as interrupted, so consumers must separate it from the next
attempt and discard unfinished buffers. `attempt` is the
1-based attempt about to start and `reason` is an HTTP status such as
`HTTP 503`, `connection interrupted`, `request timeout`, `stream read timeout`,
or `model output idle timeout`;
it never contains the response body.

A `notification` carries `task_id`, `text`, `usage` and `usage_present` for the
finished task, exactly like `provider_usage`; `cached_input_tokens_present`
says whether every provider report behind that `usage` carried the cached
breakdown, and is absent when the notification carries no usage.

### Errors

An error body has the shape `{"error":{"code":"...","message":"..."}}`.
Status codes:

| Status | Meaning |
| --- | --- |
| `400` | Empty or missing turn text, or an invalid JSON body. |
| `401` | Missing or invalid bearer token (TCP listener only). |
| `403` | `forbidden`: the client may not perform this operation. The owner, the only caller today, never receives it. Workspace routes also answer `403` for a directory outside the admitted workspaces. |
| `404` | Session, turn, task, memory record or memory candidate not found. |
| `409` | `turn_active`: a turn is running or queued, or a compaction is active, on this session. `queue_full`: 16 turns are already queued. `approval_decided` and `approval_failed` name the approval conflicts. `compaction_failed`, `skills_unavailable`, `sandbox_reload_failed`, `task_done`, and `memory_conflict` name the other conflicts. |
| `501` | `memory_unsupported`: the selected plugin memory source manages its own records. |
| `503` | `memory_unavailable`: memory is disabled or not usable in the session. |
| `500` | Internal error. The response body is a fixed `internal error` message; details go to the server log only. |

### Observability

Each normal provider response, compaction summary, and sub-agent provider
response appends a content-free row to `~/.otto/usage.db`. The row contains
time, workspace/session/task identifiers, provider/model (the model column stores the selector), usage
presence, and token counts; it never contains prompts, response text, tool
arguments, or tool output. Collection, SQLite storage, and the HTTP/UI query
path are separate boundaries.

`GET /v1/usage` returns all recorded totals; pass `session_id` to restrict the
query. `cache_hit_rate` is the weighted ratio
`cached_input_tokens / input_tokens`. Input includes ordinary input, cache reads
and cache writes. Claude reports these separately; Otto normalizes their sum
before context accounting and display. A missing cache-read breakdown makes
`cached_input_tokens` and `cache_hit_rate` unknown (`null`), not zero.

`cache_write_input_tokens` counts newly cached input; `cache_write_1h_input_tokens`
is its one-hour subset. They are optional in provider/turn `usage` values and
nullable in aggregate/daily results. Each aggregate is known only when every
reported usage contributing to it supplies that counter. Old records remain
unknown; an explicit zero stays known. Database upgrades add nullable columns
without rewriting session history or inventing historic cache counts.
Pi sessions retain writes in `cacheWrite`/`cacheWrite1h`, with exact optional
presence in Otto details. Monetary amounts are not calculated from these totals;
read and write tokens have different prices, and failed/canceled calls may
provide no usage.

`GET /v1/usage/daily` returns the same metrics grouped by UTC date and fills
days without provider calls with zeroes. The Web UI uses the existing
MIT-licensed Mermaid dependency for its chart; no separate analytics or chart
backend is involved.

`GET /metrics` exposes `otto_http_requests_total{route,method,status}`,
`otto_http_request_duration_seconds{route}`,
`otto_provider_api_requests_total{provider,model,status}`,
`otto_provider_api_request_duration_seconds{provider,model}`,
`otto_sessions_open`, `otto_sessions_evicted_total`,
`otto_session_context_window_tokens{session_id,provider,model}`,
`otto_session_context_input_tokens{session_id,provider,model}`,
`otto_session_context_input_tokens_pending{session_id,provider,model}`,
`otto_turns_total{status}`, `otto_turns_active`, `otto_turn_duration_seconds`,
`otto_tool_calls_total{tool,status}`, `otto_tool_call_duration_seconds{tool}`,
`otto_provider_tokens_total{kind}`, `otto_event_stream_clients`,
`otto_tasks_started_total`, `otto_tasks_finished_total{status}`,
`otto_tasks_running`, `otto_workflow_runs{status}`,
`otto_workflow_steps{status}`, `otto_schedule_runs_total{outcome}`,
`otto_schedule_deliveries_total{channel,outcome}`, `otto_build_info{version}`,
`process_start_time_seconds`, `process_resident_memory_bytes`,
`process_virtual_memory_bytes`, `process_cpu_seconds_total`, `process_threads`,
and `process_open_fds`. Process resource samples describe the Otto server process
only, not its child processes, and are omitted individually when the operating
system cannot provide them. Otto logs a rate-limited
`process_metrics_sample_incomplete` warning naming omitted samples.

Otto logs one line per HTTP request, turn, provider request, and tool call.
See [Logs, metrics, and request correlation](servers-and-connectors.md#logs-metrics-and-request-correlation)
for the events, their fields, and a Prometheus scrape configuration.

### Sub-agent task records

Every sub-agent task started by the `agent` tool is recorded in
`~/.otto/tasks.db` (SQLite, file mode `0600`), one row per task, keyed by
parent session and task id. Otto writes the row when the task is queued and
again when it starts, after each provider step and tool call, and when it
finishes. Each Otto process writes only its own tasks; `/agents`, the Web
UI's **Agents** view, and `GET /v1/tasks` read every process's rows.

A row holds the workspace, the parent session id and file (for a
`--no-session` parent, the id is `memory:<pid>:<process start time>` and the
file is empty), the agent, name, description, model, and context mode, the
status and its timestamps, step and tool-call counts and the last tool,
input, output, and cached tokens, the child transcript path, the owning
process id and start time, and the first 64 KiB each of the prompt, the
result, and the error. These texts are stored in plaintext, like the session
files that already hold them in full. Nothing is sent off the machine, and
rows are never pruned.

A queued or running row whose owning process has exited (or whose process id
now belongs to a different process) is shown as `interrupted`; the stored row
is not changed.

If `tasks.db` cannot be opened, or was written by a newer Otto with a schema
this build does not know, Otto prints one warning at startup and runs without
recording. A write error later in the process stops recording for that
process without output. Neither affects the task.

`agent_send` messages and `agent_report` calls are not rows in `tasks.db`. If
the child receives an `agent_send` message, it is written into that child's
transcript as parent-message context when the child reaches the next normal
notification checkpoint; until then it waits in the child's pending-message
file (see [Sessions](interactive-use.md#sessions)).

### Agent server shutdown

`otto serve` shuts down on `SIGINT` or `SIGTERM`: it stops accepting new
requests, cancels every active turn and compaction, closes every session,
removes the socket file (socket mode), and exits `0`. When an open session
holds a lease, `SIGTERM` first moves every open session as described in
[Continuing a session on another host](interactive-use.md#continuing-a-session-on-another-host),
and a second `SIGTERM` exits at once; without a lease a second `SIGTERM` has
no effect.

### Examples

```bash
curl -s --unix-socket ~/.otto/otto.sock -X POST http://otto/v1/sessions -d '{}'
curl -N --unix-socket ~/.otto/otto.sock -X POST http://otto/v1/sessions/<id>/turns -d '{"text":"list files"}'
curl -s --unix-socket ~/.otto/otto.sock -X POST http://otto/v1/sessions/<id>/turns/<turn_id>/cancel

# TCP listener; TOKEN is the value printed at startup
curl -s -H "Authorization: Bearer $TOKEN" -X POST http://127.0.0.1:8787/v1/sessions -d '{}'
curl -s -H "Authorization: Bearer $TOKEN" -X POST http://127.0.0.1:8787/v1/sessions/<id>/compact -d '{}'
```

### Not supported

- CORS on the TCP listener; a browser page on another origin cannot call the
  API directly. Client certificates and certificate reload without a restart.
- Authentication on the Unix socket beyond file permissions (owner-only
  directory and socket modes); there is no peer-uid check. More than one
  token, or a token with less than the host user's authority.
- Streaming compaction progress; `POST .../compact` returns only the result.
- Event replay across turns; only the most recent turn per session is readable.
  Workflow events are separate and durable.
- Idle session eviction or a limit on how many sessions can stay open.
- SSE heartbeats.

## ACP agent server

`otto acp` runs Otto as an [Agent Client Protocol](https://agentclientprotocol.com)
(ACP) v1 agent: newline-delimited JSON-RPC 2.0 on stdin and stdout, started by
an ACP client as a child process. stdout carries only JSON-RPC frames;
warnings and diagnostics go to stderr.

```bash
otto acp [--config PATH] [--cwd PATH] [--model PROVIDER/MODEL_ID] ...
```

`acp` accepts `--config`, `--cwd`, `--model`, `--base-url`, `--thinking`,
the deprecated `--provider`, `--sandbox`, `--shell-timeout`, and
`--max-output-bytes`. It rejects `--ui`, `--prompt`, `--resume`,
`--continue`, `--no-session`, `--archive`, and the `serve`-only flags;
`--socket` is accepted only with `--attach`.

Local `otto acp` serves one workspace: `--cwd`, else the process working
directory. A request whose `cwd` resolves to another directory is rejected.
The workspace's sandbox and memory are set up at startup as for `otto serve`;
MCP servers start when a session is created or loaded. A process may hold
several sessions. A second prompt in a session that is already running one
is rejected.

### Methods

| Method | Behavior |
| --- | --- |
| `initialize` | Reports protocol version 1, `loadSession`, and `session/list`. `_meta.otto.channelContext` advertises structured per-turn channel metadata. `agentCapabilities._meta.otto.scheduleDelivery: true` advertises delivery of [`schedule`](safety-and-tools.md#schedule) items; a client that lists platforms (`telegram`) in `clientCapabilities._meta.otto.scheduleDelivery` receives them (see below). Prompts accept text only; images, audio, and embedded context are not accepted. No authentication methods: credentials come from environment variables and `otto login`. |
| `session/new` | Creates a session. No session file is written until the first prompt. |
| `session/load` | Opens a session of this workspace by its 32-character id, sends the stored conversation as `session/update` notifications, then responds. The response also carries `sessionId`. |
| `session/list` | The newest 20 sessions of the workspace, titled by session name or the last user message (truncated to 80 characters). |
| `session/prompt` | Runs one turn. Text blocks are joined with newlines; a `resource_link` block becomes a line `<name>: <uri>`. Optional `_meta.otto.channel` (`telegram`) selects fixed guidance for this turn without altering the user's text or permissions. Optional `_meta.otto.chat` (`<platform>:<chat id>`) is the turn's chat origin for [`schedule`](safety-and-tools.md#schedule); an invalid value, or a platform that differs from `_meta.otto.channel`, is ignored with a warning on stderr. Optional `_meta.otto.inputId` is the prompt's [`input_id`](servers-and-connectors.md#agent-server): a retry with the same value in the same session runs once, and a malformed value is rejected; `_meta.otto.requestId` only correlates logs. Returns `end_turn`, or `cancelled` after `session/cancel`. |
| `_otto/approvals/message` | Restricted pending-approval dialogue with `sessionId` and `text`; advertised by `_meta.otto.approvalDialogue`. Returns `null` or `{"text","queued"}`. Available in local and attach mode. |
| `session/cancel` | Cancels the session's running prompt and any pending permission request. |
| `_otto/memory/pending` | Extension. Params `{sessionId, cursor?, limit?}` (`limit` 1 to 50, default 20); returns `{candidates, nextCursor}` with candidates as `{id, action, kind, key, text, reason, origin, scope}` plus an optional string `advice` (the one-line advice, omitted when there is none); `nextCursor` is `""` on the last page. |
| `_otto/memory/review` | Extension. Params `{sessionId, candidateId, decision}` with `decision` `accept` or `reject`; decides one candidate as a human review and returns `{decision, candidateId, record, forgotten}`. Errors: `-32010` memory is not available in the session, `-32011` the candidate was already decided or changed, `-32002` unknown session or candidate, `-32602` bad parameters or cursor. `otto acp` and `otto acp --attach` advertise both with `agentCapabilities._meta.otto.memoryReview`; a relay reviews only sessions it opened and forwards to `GET /v1/sessions/{id}/memory/candidates` and `POST /v1/sessions/{id}/memory/candidates/{candidate_id}/review`. |

`mcpServers` in `session/new` and `session/load` must be empty: MCP servers
come from Otto's own configuration (see [MCP servers](memory-and-extensions.md#mcp-servers)). Otto
does not call the client's `fs/*` or `terminal/*` methods; tools run inside
Otto as in the other frontends.

### Scheduled items are sent with `_otto/schedule/deliver`

When the client lists platforms in `clientCapabilities._meta.otto.scheduleDelivery`,
the client may also declare `clientCapabilities._meta.otto.scheduleDeliveryChats`
as an array of `<platform>:<chat-id>` destinations. Each destination must
belong to an advertised platform. `otto-connect` supplies its admitted chats
by default; an empty array disables chat delivery. Otto advertises this
support as `agentCapabilities._meta.otto.scheduleDeliveryChats: true`.
Claims are limited to these destinations before acquiring a delivery lease.
Without the destination field, or with an empty array, Otto delivers nothing
to that connection. Disconnect removes the offer; a crashed connection's
offer expires after 90 s, refreshed every 15 s independently of slow sends.

Otto sends each eligible due item as the request
`_otto/schedule/deliver` with params `{"chat": "telegram:123", "text": "..."}`
and waits up to 60 s. A `result` with any value (`otto-connect` answers
`{}`) marks the item delivered; an error or no answer is a failed attempt
that Otto retries (see [`schedule`](safety-and-tools.md#schedule)). `otto acp` also runs the due
prompt items of those chats that were created in its workspace; with
`--attach`, `otto serve` runs them.

### `/compact` and `/context` are slash commands

After `session/new` and `session/load` has replied, Otto sends a
`session/update` of type `available_commands_update` listing `compact`,
`context`, `model`, `skill`, `steer`, `queue`, and `auto`.
A `session/prompt` containing `/context`, `/compact [focus]`, `/model`, or
`/model <provider>/<model_id>` is run by Otto itself and is not sent to the task model. Command names must match
whole words; `/compactx`, `/models` or a sentence that contains `/context` is a normal
prompt. `otto acp --attach` does the same, running the command on
`otto serve` through its existing HTTP APIs. Each command answers with one
`agent_message_chunk` and the stop reason `end_turn`; `session/cancel` during
`/compact` answers `cancelled`. Existing
context/compaction/skill/model commands are rejected while the session runs
a prompt. `/steer <guidance>` bypasses the occupied prompt slot and accepts
guidance for that run. `/queue <prompt>` acknowledges acceptance immediately,
then streams the independent answer as later session updates; local ACP keeps
at most 16 waiting prompts and attached ACP uses the server queue. Local
queued prompts are saved with the session (see [Sessions](interactive-use.md#sessions)): when
the process stops before they run, `session/load` in a new process runs them
after the interrupted turn's resume, and their answers arrive as session
updates. Cancellation clears local queued prompts, including their saved
copies, and cancels this connection's attached follow-ups.

ACP advertises `_meta.otto.taskControl:true`. `_otto/tasks/steer` and
`_otto/tasks/control` use the HTTP bodies above plus `sessionId`; replies are
JSON-only, separate from the running prompt's updates.

- `/compact [focus]` summarizes earlier messages as in the other frontends
  and replies `Compacted the session context: <n> tokens before, about <m>
  tokens after (estimated).`, or `Nothing was compacted: the session has no
  earlier messages that can be summarized safely.`
- `/context` replies with plain text, one item per line: model, context
  window (`not configured` when unset), automatic compaction threshold (`off`
  when disabled), estimated tokens of the next request, the input tokens the
  provider reported last (omitted when unknown), and one line with the token
  count of each non-empty part (system prompt, tools, MCP tools, compaction
  summary, memory, messages). It never includes the text of any part. When
  the session withholds dynamic content, the reply is "The context report is
  not available in this session."
- `/model` replies `Model <selector> (type <type>[, thinking <effort>])`,
  then `Configured models: <selectors>` and
  `Switch with /model <provider>/<model_id>.`; with no configured models the
  last two lines are `No models are configured; add one on the host to switch.`
- `/model <provider>/<model_id>` rebuilds this session's runner on that model
  and replies `Switched to model <selector> (type <type>).` A bare model ID
  answers ``model `X` has no provider; use /model <provider>/<model_id>``. It makes no model
  request and never writes the configuration file, so other sessions and the
  next start keep their model; an unknown selector answers with an error
  instead of a model turn. This is the same session-level switch as the TUI's
  `/model`, which additionally saves `default_model`. See [provider and model configuration](getting-started.md#providers-and-models).

During a prompt Otto sends assistant text as `agent_message_chunk`,
reasoning as `agent_thought_chunk`, and each tool call as a `tool_call`
followed by a `tool_call_update` with status `completed` or `failed`.
`bash` calls have kind `execute` and the command as title. When a tool call
changes the [session plan](servers-and-connectors.md#session-plan), Otto sends a `plan` update with the
whole plan.

### Elevated `bash` uses `session/request_permission`

When the model asks for an unsandboxed `bash` command (see
[`bash` sandbox policy](safety-and-tools.md#bash-sandbox-policy)), the turn finishes as in the
other frontends. Otto then sends `session/request_permission` with the
command and two options, **Allow once** and **Deny**. Allow once grants that
command and runs the retry turn inside the same `session/prompt`. Deny, a
cancelled request, or `session/cancel` ends the prompt; the approval stays
pending as when a TUI user does not enter `/approve`. There is no "always"
option over ACP.

### Background results start a wake turn

When a session is open and no prompt runs, a pending notification (a fired
`remind` timer, a finished sub-agent, or another inbox item) starts a wake
turn, as in the REPL. Otto checks when the session is opened with
`session/new` or `session/load`, when a sub-agent task changes, and when a
prompt ends. The wake turn sends no `session/update` while it runs, and an
elevated `bash` request in it is denied. A `session/prompt` sent during a wake
turn waits for it to end and then runs; `session/cancel` cancels both.

If the wake turn writes assistant text, Otto sends it after the turn as one
`agent_message_chunk` outside any prompt, with
`_meta: {"otto": {"wakeReply": true, "replyId": N}}`. `replyId` starts at 1
for each session and increases by one. A wake turn that writes no text sends
nothing.

### ACP agent server shutdown

End of file on stdin, `SIGINT`, or `SIGTERM` cancels every running prompt,
answers each with `cancelled`, closes every session, and exits `0`. A
`SIGTERM` while a session holds a lease first moves it, as for `otto serve`
(see [Continuing a session on another host](interactive-use.md#continuing-a-session-on-another-host)).
A client that kills the process with `SIGKILL` loses the assistant message
being streamed; everything Otto had already written to the session file
remains.

### `--attach` forwards requests to `otto serve`

```bash
otto acp --attach [--socket PATH]
```

`otto acp --attach` holds no session: it forwards each ACP method to a
running `otto serve` over its Unix socket, so the session it uses can be open
in the web UI, in another `otto acp --attach` process, and in `otto serve`'s
other clients at the same time. It runs no model or tool and reads no
provider credentials. The socket defaults to `[server].socket`, then
`~/.otto/otto.sock`; TCP is not supported. The `initialize` response, the
memory-review capabilities are those of attach mode. Unlike local ACP,
`session/new` and `session/load` select an admitted workspace using each
request's absolute `cwd`. `session/list` filters by its `cwd`, or uses the
connection's startup workspace when omitted. Admission stays in `otto serve`; ACP does not implicitly
trust directories (see [Workspaces](servers-and-connectors.md#workspaces)).

At startup it requests `GET /healthz`. If `otto serve` does not answer, it
prints `otto serve is not reachable at <path>: <error>` to stderr and exits
with status `1`.

| Method | Request to `otto serve` |
| --- | --- |
| `session/new` | `POST /v1/sessions` with the workspace. |
| `session/load` | `POST /v1/sessions` with `resume`, then `GET .../history`, sent as `session/update` notifications before the response. Any session id of the workspace can be loaded, including one older than the newest 20. |
| `session/list` | `GET /v1/sessions?workspace=...`. |
| `session/prompt` | `POST .../turns` with `"queue": true`. A prompt sent while the session runs another client's turn waits in `otto serve`'s queue (see [Turn queue](servers-and-connectors.md#turn-queue)). |
| `session/cancel` | Cancels the prompt's turn, queued or running. |

- Only turns started by this process are forwarded; turns from other
  clients of the same session are not sent as `session/update`. The
  exception is a wake turn that `otto serve` runs for the session: its text
  is sent as one tagged update, as in
  [`otto acp`](servers-and-connectors.md#background-results-start-a-wake-turn). The relay reads
  `GET /v1/sessions/{id}/wake-replies` once a second, starting after the
  newest reply that existed when the session was opened, so earlier replies
  are not sent again. Loading the session again replaces
  its notification subscription, including after another client closes and
  reopens that session.
- A turn that ends with an error answers the prompt with a JSON-RPC error
  carrying the redacted message.
- An elevated `bash` command sends `session/request_permission` while the
  turn waits (see [Approvals inside a turn](servers-and-connectors.md#approvals-inside-a-turn)).
  **Allow once** allows it; any other answer denies it. If another client
  decides first, the request is withdrawn with `$/cancel_request` and the
  turn continues with that decision.
- If the connection to `otto serve` fails, or a turn's event stream ends
  without its `turn_end` frame, every running prompt is answered with the
  JSON-RPC error `connection to otto serve lost` and the process exits with
  status `1`.

### Chat access goes through otto-connect

To use Otto from Telegram, run `otto-connect`, which starts `otto acp`, or
`otto acp --attach` to share the sessions of `otto serve`.
See [Chat connector](servers-and-connectors.md#chat-connector).

### Not supported over ACP

- `session/resume`, `session/close`, `session/delete`, session modes and
  config options, and client-supplied MCP servers.
- Image, audio, and embedded-resource prompt content.
- Slash commands other than `/compact`, `/context`, `/model`, `/steer`, `/queue`, and the
  supported `/skill` operations, such as `/approve` or
  `/sandbox`: text from the client reaches the model as a user message.

## Chat connector

`otto-connect` connects Telegram chats to Otto through ACP.
It is a separate Go program in `chat-connector/`: it starts one `otto acp` child,
by default a local agent or `otto acp --attach` to share the sessions of
`otto serve`, and maps each chat to one Otto session. Provider selection and authentication
always belong to that Otto runtime. For example, a Claude key must be present
in the environment of `otto acp` or `otto serve`, never in `connect.toml` or the
connector process when it connects to an existing server. Restart the Otto
runtime after changing its model configuration or credentials; restarting only the
connector does not update a running server.

Otto is a general-purpose personal agent for research, writing, planning,
coding, and practical tasks, using the tools and permissions available in
the session. The connector sends the current channel separately from the
user's text, including after a session is resumed or shared. Otto adds fixed
Telegram guidance to that turn's system context, so it can adapt
replies without assuming you can see its local terminal. This context does
not change tool availability or permissions and does not carry over to a
later turn from another frontend. With an older ACP agent that does not
advertise this capability, the connector retains the previous text-prefix
behavior for channel hints.

Each prompt carries `chat:<platform>:<chat id>:<message id>` as
`_meta.otto.inputId`, so a message the platform redelivers, or one the
connector sends again after a restart, runs once in its session.

### Building and running

Building requires Go at the version in `chat-connector/go.mod`.

```bash
make connect-build                 # writes target/otto-connect
export OTTO_CONNECT_TELEGRAM_TOKEN=<bot token from @BotFather>   # for [telegram]
target/otto-connect [--config PATH]
```

`otto-connect` runs in the foreground and logs to stderr. `SIGINT` or
`SIGTERM` sends `session/cancel` for every running prompt, stops polling
Telegram, closes the agent's stdin, and kills the agent if it has not exited
10 s later. A config error or a token that Telegram rejects (HTTP 401 or 404
from `getMe`) exits with status 1 and `otto-connect: <message>`.

At most one `otto-connect` runs per user. At startup it takes an exclusive
lock on `~/.otto/connect/state.json.lock` and holds it until it exits; a
second `otto-connect` exits with status 1 and
`otto-connect: another otto-connect is running (pid <pid>); stop it before starting a new one`.

### Chat connector configuration

`~/.config/otto/connect.toml`, or the file given with `--config`:

```toml
[agent]
command = ["otto", "acp"]          # the default; resolved through PATH
workspace = "/Users/me/work"       # required, absolute

[telegram]
token_env = "OTTO_CONNECT_TELEGRAM_TOKEN"
chats = ["123456789"]
senders = ["123456789"]

[metrics]
listen = "127.0.0.1:9465"          # the default; "" turns the endpoint off
```

- The `[telegram]` section is required.
- The agent process starts with `workspace` as its working directory, and
  every session uses it as `cwd`. Otto flags go into `command`, for example
  `["otto", "acp", "--model", "claude/MODEL_ID"]`.
- The Telegram bot token is read from the environment variable that
  `token_env` names; an empty or unset variable fails startup. A `token` key
  in the file, like any unknown key, fails the config load. The agent process
  is started without this variable, so tools run by Otto cannot read it.
- `otto acp` inherits the rest of the environment and reads its provider key
  as usual (the provider's `api_key_env` variable or `OTTO_API_KEY`), or uses
  the `chatgpt` provider after `otto login`. For Claude, export the selected
  API-key variable (and optional `workspace_id_env`) in the environment that
  starts `otto acp`; `otto-connect` never reads, stores, or forwards either
  value.
- State is kept in `~/.otto/connect/state.json` (mode `0600`): the session id
  of each chat and the Telegram update offset.
- `[metrics].listen` is the loopback address of the connector's `/metrics`
  and `/healthz` endpoints. Only `127.0.0.1`, `::1`, and `localhost` are
  accepted. If the port cannot be bound, the connector logs
  `metrics_listen_failed` and runs without the endpoint.
- Log lines contain chat ids, sender ids, and errors. Message text is not
  logged. See [Logs, metrics, and request correlation](servers-and-connectors.md#logs-metrics-and-request-correlation).

### Sharing sessions with `otto serve`

With `command = ["otto", "acp", "--attach"]`, the agent process is the
relay described in [`--attach` forwards requests to `otto serve`](servers-and-connectors.md#--attach-forwards-requests-to-otto-serve):
sessions, models, and tools run in the `otto serve` on the socket, and the
web UI of that serve shows the same sessions.

```toml
[agent]
command = ["otto", "acp", "--attach"]   # add "--socket", "/path" for a non-default socket
workspace = "/Users/me/work"            # the same workspace the serve clients use
```

- Start `otto serve` before `otto-connect`. The relay reads no provider
  credentials; the serve process needs them.
- A chat's message and a web UI message on the same session wait in serve's
  turn queue and run one after the other. The chat gets the reply of its own
  turns only; turns started in the web UI are not sent to the chat. A wake
  turn that serve runs for the session, such as a fired `remind` timer, is
  sent to the chat while the connector has the session open.
- Use `/sessions` and `/use` (see [Commands](servers-and-connectors.md#commands)) to bind a chat to a
  session that was started in another client.

### The serve and connect supervisor script

`scripts/otto-serve-connect.sh` supervises `otto serve` and the connector as a
pair. Serve listens on its Unix socket, and the connector's `connect.toml` sets
`[agent] command = ["otto", "acp", "--attach"]`, so each chat session is an
`otto acp --attach` process that relays to serve over that socket:

```bash
scripts/otto-serve-connect.sh start     # otto serve plus otto-connect
scripts/otto-serve-connect.sh status
scripts/otto-serve-connect.sh restart   # both; `restart serve` or `restart connect` for one
scripts/otto-serve-connect.sh logs connect
scripts/otto-serve-connect.sh stop
```

Build the two programs first (`make build`, `make connect-build`), or install
them and set `OTTO_BIN` and `OTTO_CONNECT_BIN`.

It reads the same `otto.env` as [the launcher](getting-started.md#launcher-script), so the
provider and model selector, base URL, workspace, sandbox mode, thinking effort
and shell limits are shared with the terminal frontends:

| Setting | Meaning |
| --- | --- |
| `OTTO_BIN`, `OTTO_CONNECT_BIN` | the two binaries; empty uses `<checkout>/otto` and `<checkout>/target/otto-connect`, then `PATH` |
| `OTTO_SERVE_CONNECT_SOCKET` | absolute path of the Unix socket serve binds (`--socket`); empty uses `~/.otto/otto.sock`. A different path must also appear in `connect.toml` as `command = ["otto", "acp", "--attach", "--socket", "<path>"]` |
| `OTTO_CONNECT_CONFIG` | `connect.toml` to run; empty uses `~/.config/otto/connect.toml` |
| `OTTO_CWD` | workspace serve starts in; empty uses `[agent].workspace` from `connect.toml` |
| `OTTO_LISTEN` | optional HTTP listener for serve (`--listen`), for `otto --attach` over TCP and the web UI |
| `OTTO_SERVE_EXTRA_ARGS` | extra `otto serve` options, appended verbatim |
| `OTTO_SERVE_CONNECT_HOME` | state and log directory; empty uses `~/.local/state/otto-serve-connect` |
| `OTTO_UI`, `OTTO_EXTRA_ARGS` | not used: `otto serve` accepts neither `--ui` nor `--prompt` |

The script always passes `--socket` to serve, because with `--listen` alone
serve opens only the TCP listener. No token is used between the two programs,
and the script removes `OTTO_ACP_TOKEN` from the environment of both.

Startup validates the absolute workspace and the connector `[agent]` table:
`command` must be the attach command (with the matching `--socket` when the
socket is not the default), and `address` and `token_env` must be absent. A
different connector workspace warns because serve can admit several. Keep
checks enabled; `OTTO_SERVE_CONNECT_SKIP_CHECKS=1` delegates this validation to
the operator.

`start` backgrounds the supervisor; `run` stays foreground for a supervisor or
terminal. Connect starts after serve answers `GET /healthz` on the socket (checked with `curl`, or `python3` when curl is absent), or after 20 s. Exited
processes restart with 1–60 s exponential delay, reset after 60 s uptime; five
fast failures stop the stack. Inspect logs/config/credentials before
restarting. SIGTERM/SIGINT stop connect first, cancelling prompts, then serve.
SIGUSR1 restarts serve; SIGUSR2 restarts connect. After a serve restart,
connect reloads the chat session on the next message; old running turns were
cancelled.

`OTTO_SERVE_CONNECT_HOME` holds PID/state and `supervisor.log`, `serve.log`,
`connect.log` (directory `0700`, files `0600`). `start` retains one previous
log generation; `logs` follows. Protect them: serve logs can contain its HTTP
token URL and connector logs contain chat/sender IDs. The connector's platform
token comes from the launching environment or ignored `otto.env`, never a
committed supervisor file. For launchd or another supervisor use
`otto-serve-connect.sh run` with private environment/credential injection.
Restart the relevant processes after upgrades or changes.

### Admission

A message is handled only when its chat id is in `chats` and its sender id
is in `senders`. An empty list admits nothing, and startup logs a warning
that the platform will ignore all messages. A rejected message gets no reply
and is logged as `chat_rejected` with its `chat_id` and `sender_id`; send the
bot a message and read that log line to find the ids to add. In a Telegram
private chat the chat id equals the user id; Telegram group ids are negative
numbers.

In a Telegram group, a message must also mention the bot (`@BotName`), reply
to one of the bot's messages, or be a command addressed to it
(`/stop@BotName`). With BotFather's privacy mode on (the default), Telegram
delivers only such messages to the bot.


### Messages and replies

- On Telegram, `otto-connect` converts the Markdown of Otto's replies to
  Telegram formatting and sends the text with message entities, not with a
  parse mode. Backslash escapes and HTML entity references are resolved,
  except inside inline code. Bold, italic, strikethrough, inline code, links, block quotes, and
  fenced or indented code blocks (with the language of the fence) keep their
  formatting. Headings are bold lines. Images become a link on their alt
  text. A link or image whose destination is not an `http://`, `https://`, or
  `tg://` URL, such as a relative file path, keeps only its text. Bullet and numbered lists are written as "•" and "N." lines, nested
  two spaces per level. Tables are sent as monospace blocks with padded
  columns; the padding counts characters, so East Asian wide characters do
  not align. Raw HTML such as `<id>` is shown as written. Messages from the
  connector itself (command output, errors, permission requests) stay plain
  text. If Telegram rejects a formatted part with status 400, that part is
  resent as plain text.
- Each chat has one Otto session. The first message creates it. At startup
  `otto-connect` loads the session of each chat of a configured platform,
  once per session, so timers armed before a restart fire again; a session
  that fails to load is logged as a warning and the chat keeps it. After a
  restart of the agent, the next message loads it.
  The replayed history of a loaded session is not sent to the chat.
- If the session cannot be loaded (for example, without `--attach`, it is
  open in the TUI or in `otto serve`), the chat gets "The previous session
  could not be loaded; started a new one." and the message runs in a new
  session.
- One task prompt runs per chat at a time. Further natural text is interpreted
  as guidance, a separate follow-up, or a read-only question (see
  [Active-task guidance and model follow-ups](servers-and-connectors.md#active-task-guidance-and-model-follow-ups)).
  Waiting input uses a bounded queue of at most 10. A message beyond that gets "Queue is full (10 messages); the
  message was not queued." Chats on different sessions run concurrently.
  Persisted chat bindings outside the current chats/senders allowlists are not
  restored, and background replies recheck admission before sending.
  Chats bound to the same session (with `/use`) take turns: one prompt runs
  on a session at a time, and when three or more chats wait for the same
  session, the order in which they run is not guaranteed.
- The text of a wake turn (a fired `remind` timer, a finished sub-agent, or
  another notification that Otto handles while no prompt runs; see
  [Background results start a wake turn](servers-and-connectors.md#background-results-start-a-wake-turn))
  is sent to every chat bound to that session, formatted like other replies
  and not as a reply to a message. If that chat has a prompt running, the
  wake text is sent after that prompt's reply.
- An item created with [`schedule`](safety-and-tools.md#schedule) in a chat is delivered to that
  chat as Markdown, not as a reply to a message, also after `/new` or `/use`.
  Otto sends the text over ACP; `otto-connect` sends it only to a chat id
  listed in `chats` of a configured platform and keeps no copy, so Otto
  retries a failed send. `otto-connect` connects to Otto at startup, also when
  no chat has a session yet, and connects again after the Otto process exits.
- The reply is sent when the turn ends, as a reply to the message that
  started it. It contains the assistant text only: reasoning and tool calls
  are not sent, and text before and after a tool call is separated by a
  blank line. Telegram replies are plain text, split at line boundaries into
  parts of at most 4096 characters, and the chat shows "typing" while the
  turn runs.
- Only text is sent to Otto. A photo, file, or other attachment gets
  "Attachments are not supported and were ignored."; a Telegram caption is
  sent as the prompt.
- A message received by the platform more than 30 minutes before
  `otto-connect` reads it is not run; the chat gets a notice with the
  message's time.
- Each Telegram message is acknowledged once it is queued or rejected. A
  crash therefore does not run a message twice; a message acknowledged but
  not yet run when `otto-connect` stops is not run.

### Commands

A message whose whole text is one of these commands (for `/use`, the
command and its argument) is handled by `otto-connect` and not sent to Otto,
except `/compact`, `/context`, and `/skill`, which are sent to
Otto as typed for agent-side command handling, and `/model <provider>/<model_id>`, which the
connector forwards to Otto as a prompt. Bare `/model` is answered by the
connector itself.
Any other text, including other words starting with `/`, is a prompt.

| Command | Effect |
| --- | --- |
| `/new` | The chat's next message starts a new session. The old session stays in Otto's session store. |
| `/stop` | Cancels the running turn (reply "Stopped.") and clears the chat's queue. With nothing running: "Nothing is running." |
| `/steer <guidance>` | Delivers guidance directly to the active task at its next safe boundary. Refuses idle tasks and pending approval; does not interrupt an in-flight tool. |
| `/queue <prompt>` | Queues an independent prompt without model routing. Starts immediately when idle, otherwise waits behind the task and any approval; acknowledges actual admission. |
| `/compact [focus]` | Sent to Otto as a prompt; `otto acp` runs it as `/compact` (see [`/compact` and `/context` are slash commands](servers-and-connectors.md#compact-and-context-are-slash-commands)) and replies with the token counts before and after. Like any prompt it waits in the chat's queue behind a running turn. |
| `/context` | Sent to Otto as a prompt; `otto acp` replies with the token counts of the next request, part by part. It waits in the chat's queue like any prompt. |
| `/model` | Answered by `otto-connect`: it posts the models from the local `~/.config/otto/config.toml` (`[providers.<name>.models]`), one line each as `provider/model_id`, plus one button per model. Choosing a button switches the conversation exactly as `/model <provider>/<model_id>` does. On a platform without buttons the same list is posted as text. It makes no model request. The list covers the user configuration only (a workspace's `.otto/config.toml` is not read), and it marks the configured `default_model`, not the session's current one. |
| `/model <provider>/<model_id>` | Sent to Otto as a prompt; `otto acp` rebuilds this session on the named model, replies "Switched to model …", and leaves the configuration file and every other session unchanged. It works whether the connector runs `otto acp` or `otto acp --attach`, uses no model request, and is therefore the way to move off a model whose quota is exhausted. An unknown selector answers with an error. |
| `/allow` | Answers the pending permission request with Allow once. |
| `/deny` | Answers the pending permission request with Deny. |
| `/sessions` | Lists the 10 newest sessions of the workspace, one per line: `*` for the chat's session or a space, the first 8 characters of the id, the last change as `YYYY-MM-DD HH:MM` (`-` when unknown), and the title (`(untitled)` when empty). With none: "No sessions." |
| `/use <id>` | Binds the chat to a session and loads it. `<id>` is a full session id, or a prefix of at least 4 characters of a session that `session/list` returns (the newest 20). Reply: "Using session `<id>`: `<title>`". |
| `/memory` (or `/memory review`) | Lists the chat session's pending memory candidates, one per line: the first 8 characters of the id, the action, `kind/key`, the text (200 characters at most), and the origin. With none: "No pending memory candidates." |
| `/memory accept <id>` | Accepts one pending candidate, which writes the record. `<id>` is a full candidate id or a unique prefix of at least 4 characters. |
| `/memory reject <id>` | Rejects one pending candidate; no record is written. |

At startup `otto-connect` sets the Telegram bot's command menu to these
commands, including `/compact`, `/context`, `/model`, `/skill`, `/steer`, and `/queue`, and to `/models` and `/config` (see
[Provider and model management](servers-and-connectors.md#provider-and-model-management)): `setMyCommands`
for the default scope, then `deleteMyCommands` for the `all_private_chats` and
`all_group_chats` scopes, whose lists Telegram would show instead. Lists set
for a single chat, for chat administrators, or for a specific language are not
changed. It also uses `setChatMenuButton` to select the commands menu for the
bot default and every configured private chat; group IDs are skipped because
this API applies to private chats. This replaces any custom Web App menu button
for those private chats. A failed call is logged and the connector continues.

`/use` replies with one of these when it does not switch:

| Case | Reply |
| --- | --- |
| No argument | "Usage: /use <session id>" |
| Prefix shorter than 4 characters | "A session id prefix needs at least 4 characters." |
| Prefix matches several listed sessions | "\"`<prefix>`\" matches `<n>` sessions; use more characters." |
| Prefix matches no listed session | "No listed session starts with \"`<prefix>`\"." |
| A message of the chat is running or queued | "A message is running or queued; /use is refused. Send /stop first." |
| The agent refuses the load | "Error: could not load session `<id>`: `<error>`" |

A message that arrives while `/use` is loading gets "A session switch is in
progress; the message was not queued." A failed `session/list` call in
`/sessions` or `/use` gets "Error: session/list failed: `<error>`".

### Provider and model management

When the connector starts `otto acp --attach`, the same admitted chat senders
can manage models ([format](getting-started.md#providers-and-models)) through
the running `otto serve`:

```text
/config
/config models
/config show <provider>
/models <provider>
/config add <provider>/<model_id> [--thinking <level>]
/config set <provider>/<model_id> thinking <level>
/config use <provider>/<model_id>
/config remove <provider>/<model_id>
/config confirm <id>
/config cancel <id>
```

`/config` and `/config models` list the default model and each
`provider/model_id`. `show <provider>` adds the endpoint and API-key
**variable name**, never its value. `/models <provider>` queries the provider's
model IDs through `GET /v1/config/models`; it reports an error for `chatgpt`.
`add` creates a model entry, `set` changes its `thinking`, `use` sets
`default_model`, and `remove` deletes the entry.

Management requires `otto acp --attach` and a running server. Direct ACP rejects
these commands without sending them to the model; `/model <provider>/<model_id>`
still switches the current conversation in either deployment without saving
config.

Each mutation calls `POST /v1/config/changes` and returns a redacted preview
and an id. The same chat and sender must `/config confirm <id>` within ten
minutes, or `/config cancel <id>`. No raw TOML, arbitrary writes or credential
values are accepted. Chat cannot create or edit providers: creating a provider
or changing its `base_url`, `api_key_env`, `api_version` or `workspace_id_env`
is host-only, so chat cannot change where server environment credentials are
sent.

Restart `otto serve` after saving; existing sessions never silently switch.
On failed previews search connector/server logs for `request_id`. Logs contain
IDs/selectors/outcomes, not text, config bytes or credential values.

### Memory review

`remember` and `forget` queue candidates through the shared approval service.
AUTO may review and apply a change covered by your original task. A change
requiring your decision, or any request in MANUAL, stays pending. In a
chat, **Approve / Deny** buttons and `/memory accept` / `/memory reject` make
that decision: the connector
sends them to `otto acp` as the ACP extension requests `_otto/memory/pending`
and `_otto/memory/review`, which Otto records as a human review. The model has
no tool for this, and text in the chat such as "approve" is just a prompt.
The commands need the chat to have a session (otherwise "This chat has no
session yet; send a message first."), work while a turn is running, and
reply "This agent does not support memory review." when the agent does not
advertise it; `otto acp` and `otto acp --attach` both do. An id that is not a unique
pending prefix gets "No single pending candidate starts with \"`<id>`\"."; other
arguments get "Usage: /memory | /memory accept <id> | /memory reject <id>".
After each successful turn, Telegram shows pending memory candidates
as cards with **Approve / Deny** buttons, including create, update, and forget
proposals. `/memory` refreshes the cards; old buttons become inactive. Buttons
are bound to the original chat session and candidate, and use the same chat and
sender allowlists as text commands. A successful decision or a conflict removes
the buttons. Failed card delivery falls back to text commands. After restarting
the connector, use `/memory` to get fresh cards.

`/memory` shows the first page (20) and says when more are pending; an id
is looked up across pages. When a turn called `remember` or `forget`, the
reply ends with "Memory changes are proposals. Send /memory to review them."
The tool results of `remember` and `forget` also tell the model that only the
user decides a candidate and how, so the model can point the user to
`/memory`; it still cannot decide one. Editing a candidate during review is
not available from the chat; use `/memory review` in the REPL or TUI.

Local REPL/TUI announces pending reviews; the TUI also refreshes its unified
approval queue. `/memory review` opens memory-only browsing: arrows select,
`a` accepts, `r` rejects, Esc closes unchanged. This content-free notice is
process-local; attached/web/chat clients use their own refresh behavior.

### Permission requests

When Otto asks to run an unsandboxed `bash` command (see
[Elevated Bash permission requests](#elevated-bash-uses-sessionrequest_permission)),
the chat gets the command with **Approve** and **Deny** buttons (Telegram inline
keyboard). The first button click or `/allow` or
`/deny` from an admitted sender in that chat answers it once. Buttons are bound
to that request; old cards cannot answer a newer request. After a decision,
cancellation, or expiry, the connector replaces the buttons with the final status.
If sending the card fails, it falls back to text commands. `/stop`
cancels it. With no answer after 10 minutes the request is denied and the
chat gets "Permission request timed out; denied." `/allow` or `/deny` with
no pending request gets "No pending request."

With `--attach`, the same request is also shown in the web UI of
`otto serve`. When another client decides it first, serve's own
10-minute timeout denies it, or the turn ends before an answer, the relay
withdraws the request (`$/cancel_request`) and the chat gets "permission
request answered elsewhere". A later `/allow` or `/deny` gets "No pending
request."

### Agent process exits

If `otto acp` exits, each running turn ends and its chat gets one message
with the exit status and the last 20 lines of the agent's stderr. The next
message starts the agent again. After consecutive exits within 60 s of a
start, the restart waits 1 s, then 2 s, 4 s, and so on up to 60 s.

With `--attach`, the relay exits with status 1 when `otto serve` stops or
the socket connection breaks, and a relay started while serve is not
running exits with status 1 and the stderr line "otto serve is not reachable
at `<path>`: `<error>`". Both reach the chat as an agent exit. Restart
`otto serve`; the next message starts the relay again.

### Not supported by otto-connect

- Streaming partial replies; the reply is sent when the turn ends.
- Images, files, and other attachments.
- Running as a login service (launchd or systemd).

## Logs, metrics, and request correlation

Native agent runtimes automatically write content-free execution diagnostics to
`~/.otto/logs/diagnostic-<pid>-<random-id>.jsonl`. TUI, REPL, headless, ACP,
serve and the embedding startup path share this facility. Attach-mode execution
is recorded on the server host. `--no-session` disables conversation persistence,
not these content-free diagnostics. Each runtime owns an independent file family;
multiple processes and PID reuse cannot share an active log or rotate each
other's files.

Files rotate at 5 MiB with three backups (`.jsonl.1` through `.jsonl.3`). New
files are mode `0600` and the logs directory is mode `0700`. On startup, Otto
removes inactive families whose latest file activity is over seven days old;
a held ownership lock protects active runtimes. Retention cleanup is best effort.

JSONL records contain `time`, `pid`, `instance`, build `version`, `session_id`,
`task_id`, `operation_id`, `span_id`, provider/model identity and operation kind.
Turn records use the persisted turn id as their operation id. Provider/tool
operation ids correlate with session records; child scopes retain the parent
session id and their own task id. No prompts, stream text, reasoning, tool
arguments/results, URLs, headers or raw error messages are written. Metadata
identifiers are bounded and passed through the existing secret redactor.

Operations record start, finish, retries and abandonment when a future is dropped
without settlement. While active, a background thread records a heartbeat every
15 seconds. Provider phases distinguish request preparation, sending, received
HTTP headers/status, received body bytes, decoded model output and retry waits.
`first_network_ms`, `first_model_ms`, `last_network_ms`, `last_model_ms` and
`last_activity_ms` are offsets from `started_at`; `elapsed_ms` and provider
`idle_ms` use a monotonic clock. Tool/turn heartbeats report elapsed time, not
inferred network idleness. These are observed stages, not proof of a remote
server's internal state or a tool subprocess's liveness.

`finish` preserves the typed outcome, attempt count, safe error class and any
provider-reported token usage. Missing usage is unknown, not zero; an explicit
all-zero usage remains present. Input tokens include cached input tokens.
`finish.cached_input_tokens_present` distinguishes a reported cache count from
an unknown cache breakdown; false must not be treated as a zero cache hit. Sum
only final provider/local-compaction/remote-compaction/one-shot usage records,
not heartbeats or turns.
Compaction and one-shot records describe their provider calls, not subsequent
checkpoint/content validation. Cancelled attempts may not return usage. Tool
exit codes and detailed failures remain in session tool results; this file does
not copy their contents. Diagnostic records never enter model context.

If file logging cannot initialize, startup emits a warning and continues.
Later write failures do not fail agent operations; `dropped_records` on the next
successful write reports lost records. Forced process termination can leave an
unfinished operation or a partial final line. These logs supplement, rather than
replace, append-only session/operation history.

To locate one session across processes, for example:

```sh
rg 'SESSION_ID' ~/.otto/logs/diagnostic-*.jsonl*
```

Existing frontend logs remain separate: `otto serve` and `otto-connect` write
structured lines to stderr and expose Prometheus metrics on `/metrics`.
To retain those stderr lines as well, redirect them or use a supervisor:

```sh
otto serve --socket ~/.otto/otto.sock --listen 127.0.0.1:9464 2>>~/.otto/serve.log
otto-connect 2>>~/.otto/connect.log
```

The terminal UI suppresses stderr log lines to protect its screen; its execution
diagnostics still go to the private JSONL files.
Plain `otto acp` writes to stderr; when `otto-connect` starts it, the
connector logs each of those lines as `agent stderr: <line>`.

### Log format

Every line has the form

```text
time=2026-10-04T10:15:02.114+08:00 level=INFO msg=turn_finished session_id=... turn_id=... duration_ms=8123
```

`msg` is a stable event name; values that contain spaces or quotes are
quoted. Levels are `INFO`, `WARN`, and `ERROR`; there is no level setting.
Message text, model output, prompts, tool arguments and results, OAuth/API
token values, secrets, and remote-compaction `encrypted_content` are never
logged. Token counts and cache-breakdown presence may be logged. Chat ids and
sender ids are logged; protect the log files accordingly.

Provider runtime events shared by non-interactive frontends:

| Event | Fields |
| --- | --- |
| `provider_operation_finished` | `operation_id`, `kind`, `provider`, `model`, `attempts`, `status`, `disposition`, `effect_certainty`, `stop_reason`, `duration_ms` |
| `remote_compaction_finished` | `checkpoint_id`, `provider`, `reason`, `automatic`, `tail_messages`, `tokens_before`, `usage_present`, `cached_input_tokens_present` |

`otto serve` events:

| Event | Fields |
| --- | --- |
| `http_request` | `method`, `route`, `status`, `duration_ms`, `request_id`, `peer` (TCP and TLS listeners only) |
| `session_evicted` | `session_id`, `idle_ms` |
| `turn_started` | `session_id`, `turn_id`, `trigger`, `request_id`, `inbox_kind` on wake turns |
| `provider_request_finished` | `session_id`, `turn_id`, `request_id`, `provider`, `model`, `status`, `attempts`, `duration_ms` |
| `tool_call_finished` | `session_id`, `turn_id`, `request_id`, `tool`, `status`, `duration_ms` |
| `turn_finished` | `session_id`, `turn_id`, `trigger`, `request_id`, `status`, `duration_ms`, `provider_requests`, `provider_ms`, `tool_calls`, `tool_ms`, `input_tokens`, `output_tokens` |
| `turn_error` | `session_id`, `turn_id`, `trigger`, `request_id`, `error` |
| `schedule_run_finished` | `schedule_id`, `due_at`, `outcome`, `duration_ms` |
| `schedule_delivery_finished` | `channel`, `outcome`, `error` on failure |
| `schedule_store_error` | `operation`, `error` |
| `workflow_step_finished` | `run_id`, `workflow`, `step_id`, `status`, `duration_ms` |
| `workflow_run_finished` | `run_id`, `workflow`, `status`, `duration_ms` |
| `mcp_server_connected` | `server`, `transport`, `tools` |
| `mcp_server_failed` | `server`, `transport`, `error` |

`request_id` is present only on turns whose request carried one. The
schedule, workflow, and MCP events are also written by `otto acp` and
`otto workflow`, which run the same code.

- `schedule_run_finished`: `outcome` is `reply`, `failed_before_tools`, or
  `failed`. It is written after the run is recorded; if recording fails, a
  `schedule_store_error` line (`WARN`) is written instead.
- `schedule_delivery_finished`: `outcome` is `ok` or `error` (`WARN`, with the
  same error text the schedule store records). `channel` is `telegram`,
  `terminal`, or `web`.
- `workflow_step_finished`: `status` is `succeeded`, `failed`, `canceled`, or
  `interrupted` (the run's deadline passed). `workflow_run_finished` is
  written once, when the run ends as `succeeded`, `failed`, or `canceled`; its
  `duration_ms` counts from the run's creation, approval waits included.
- `mcp_server_connected`: `tools` is the number of tools kept after filtering.
  `mcp_server_failed` is `WARN` and carries the error `/mcp` shows. A server
  that needs `otto mcp login` or is disabled writes neither event.

`otto-connect` events:

| Event | Fields |
| --- | --- |
| `connector_started` | `platforms`, `agent_mode`, `metrics_listen` |
| `connector_stopping` | `reason` |
| `chat_received` | `request_id`, `platform`, `chat_id`, `sender_id`, `message_id` |
| `chat_rejected` | `platform`, `chat_id`, `sender_id`, `reason` |
| `chat_queued`, `chat_queue_full` | `request_id`, `platform`, `chat_id`, `queue_length` |
| `session_created`, `session_loaded` | `request_id`, `platform`, `chat_id`, `session_id` |
| `session_load_failed` | `request_id`, `platform`, `chat_id`, `session_id`, `error` |
| `turn_started` | `request_id`, `platform`, `chat_id`, `session_id` |
| `turn_finished` | `request_id`, `platform`, `chat_id`, `session_id`, `stop_reason`, `duration_ms`, `reply_bytes` |
| `turn_failed` | `request_id`, `platform`, `chat_id`, `session_id`, `duration_ms`, `error` |
| `chat_reply_sent`, `chat_reply_failed` | `request_id`, `platform`, `chat_id`, `bytes`, `error` on failure |
| `agent_started` | `pid`, `command` |
| `agent_exited` | `status` |
| `agent_restart_scheduled` | `delay_ms` |
| `metrics_listen_failed` | `address`, `error` |

`chat_queue_full`, `session_load_failed`, `turn_failed`, `chat_reply_failed`,
`agent_exited`, and `metrics_listen_failed` are `WARN`; the others are `INFO`.
`connector_stopping` has `reason=signal` or `reason=error`. `chat_reply_sent`
is written for agent replies only; `chat_reply_failed` is written for every
failed send, with an empty `request_id` when the text was a connector notice
(for example `Stopped.`).

### Following one chat message

The connector gives each admitted chat message a `request_id` (32 hex
characters) and sends it with the prompt. Otto attaches it to the turn the
prompt starts when the prompt reaches `otto serve` through `otto acp
--attach`. To follow a message:

```sh
grep 'request_id=<id>' ~/.otto/connect.log   # chat_received ... chat_reply_sent, with session_id
grep 'request_id=<id>' ~/.otto/serve.log     # turn_started, each provider request and tool call, turn_finished
```

`turn_finished` in the serve log splits the turn's time into `provider_ms`
and `tool_ms`; each `provider_request_finished` and `tool_call_finished` line
gives one call's duration. A `/config` command's error reply quotes its
`request_id`. With plain `otto acp` (no `otto serve`), only the connector's
events carry the ID.

### Prometheus

Prometheus cannot read a Unix socket, so start `otto serve` with a loopback
TCP listener next to the socket (see [Listener](servers-and-connectors.md#listener)); on a loopback
listener `/metrics` and `/healthz` need no token. On a non-loopback TLS
listener `/metrics` needs the token: add `scheme: https`, a `tls_config` that
trusts the certificate, and `authorization: {type: Bearer, credentials_file:
...}` holding the value of `OTTO_SERVER_TOKEN` to the scrape job.
`otto-connect` serves its metrics on `[metrics].listen`, by default
`127.0.0.1:9465`.

```yaml
scrape_configs:
  - job_name: otto
    static_configs:
      - targets: ["127.0.0.1:9464"]
  - job_name: otto-connect
    static_configs:
      - targets: ["127.0.0.1:9465"]
```

The serve metric families are listed under [HTTP API](servers-and-connectors.md#http-api). The
connector exports:

| Metric | Type | Labels |
| --- | --- | --- |
| `otto_connect_build_info` | gauge | `version` |
| `otto_connect_messages_total` | counter | `platform`, `outcome` (`accepted`, `rejected`, `queue_full`) |
| `otto_connect_turns_total` | counter | `platform`, `stop_reason` |
| `otto_connect_turn_duration_seconds` | histogram | `platform` |
| `otto_connect_replies_total` | counter | `platform`, `outcome` (`ok`, `error`) |
| `otto_connect_agent_restarts_total` | counter | none |
| `otto_connect_agent_up` | gauge | none |

`accepted` counts every message that passed the chat and sender lists and was
not refused by a full queue, including commands and messages too old to run.
`otto_connect_replies_total` counts sends of agent replies only.
`otto_connect_agent_restarts_total` increases each time the connector replaces
an agent process or connection that ended. `otto_connect_agent_up` is 1 while
a started agent process or connection has not ended. The `version` label is
the module version (`(devel)` for a local build), followed by `+` and the first 7 characters of the Git
revision when the binary was built from a checkout.

No metric has a chat, sender, session, or request label.

---

Previous: [Safety and tools](safety-and-tools.md) · [Documentation index](../README.md) · Next: [Automation](automation.md)

### Optional input source claims

HTTP turn creation (`POST /v1/sessions/{id}/turns`) and active input admission
(`POST /v1/sessions/{id}/inputs`, including the explicit task-steer endpoint)
accept optional `client_source`: `unknown`, `tui`, `repl`, `one_shot`, `http`,
`acp`, `connector`, `runtime`, or `agent`. These are descriptive claims only.
The server always supplies confirmed HTTP source/transport and actual delivery;
a client cannot supply trusted `tui`, `local`, or security authority. Omitting the
field preserves existing API behavior; external browser clients need no changes.
Unknown enum values are rejected. Turn creation also accepts `queued_input: true`
for explicit follow-up intent (ACP `/queue`); unlike `queue: true`, which only
permits waiting behind an active turn, this preserves queued delivery even when
admitted while idle. It grants no source or security authority.
User-message events additionally expose
optional `input_id` and `input_provenance` for the accepted input.

ACP clients can declare `_meta.otto.inputSource` in `clientCapabilities` during
initialization. Otto Connect declares `connector`. This becomes only an
unverified `client_source`; ACP source and transport remain runtime-confirmed.
For a full explanation of persistence, legacy unknown provenance and local versus
attached terminals, see [Per-message input origin](interactive-use.md#per-message-input-origin).

## Input identity, origin, and control dialogue receipts

HTTP and ACP reuse the shared stable input IDs, canonical delivered user messages,
and `InputProvenance` (`source`, `transport`, `delivery`, optional `client_source`).
Delivery values are `direct`, `queued`, and `steering`; they do not imply source or
successful effects. Confirmed HTTP/ACP ingress is never replaced by a client's
TUI/connector claim. Originless legacy records remain unknown, and generated
runtime/control answers remain context rather than human messages.

The native local TUI additionally saves a recoverable intent before clearing its
editor ([local recovery](interactive-use.md#local-input-ids-and-recovery)). This
local editor guarantee does **not** cover external HTTP/ACP client editors.
Existing remote durable turn queues and guidance recovery keep their existing
semantics; they are not replaced with the local journal or new endpoints. Remote
clients own their display and draft retention. A delivered ID is evidence that
its user message entered context, not an exactly-once external-effect guarantee.

Existing HTTP `POST /v1/sessions/{id}/approvals/message` and
`POST /v1/sessions/{id}/tasks/control` accept optional `input_id` and
`client_source`, just like runtime-owned identity/provenance elsewhere. Their
non-null control receipts include `input_id`; omitted IDs are allocated by the
runtime. Existing ACP `_otto/approvals/message` and `_otto/tasks/control` accept
optional `inputId` and return the same receipt ID. ACP source comes from the
connection's observed transport; application claims retain their unverified
meaning. These additions are backward compatible and create no new endpoints.
Approval messages keep original human text/identity; generated dialogue answers
are separate context and cannot grant an approval or authorize effects. A queued
control reply remains an admission instruction to the caller, not delivered
message evidence. An absent/non-applicable control reply is still `null`.
