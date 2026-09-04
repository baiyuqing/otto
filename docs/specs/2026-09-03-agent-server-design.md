# Agent server design

Status: approved 2026-09-03, implementation in progress.

## Goal

Otto currently has two in-process frontends, `internal/tui` and
`internal/repl`. Neither is reachable from outside the process, so an
external program (a web UI, a script) has no way to drive Otto. This PR adds
a third frontend, `otto serve`, that exposes the agent over HTTP+JSON+SSE on
a Unix domain socket, so it can act as an agent server for other local
processes.

The server reuses `app.Controller` and `runtimeBuilder` unchanged; it adds
no new orchestration logic to the agent loop. It manages multiple sessions:
one process, one workspace, N sessions, each with its own `*app.Controller`.
Turns in different sessions run concurrently. A second prompt against a
session that already has an active turn returns HTTP 409.

Non-goals for this iteration: TCP listening and authentication, a
`POST .../compact` endpoint, event replay across turns, session idle
eviction or an open-session cap, SSE heartbeats, peer-UID verification on
the socket, profile-switch and archive endpoints, resuming a session older
than the 20 most recent, and wiring `memory.Binding.Observe`. These are
listed again in [Follow-ups, not in this PR](#follow-ups-not-in-this-pr).

## Transport and socket

### CLI

```
otto serve [--config --cwd --profile --provider --base-url --model --thinking --sandbox --shell-timeout --max-output-bytes] [--socket PATH]
```

`run()` recognizes `args[0] == "serve"` at the same subcommand-dispatch site
used for `memory`/`login`/`logout` (`cmd/otto/main.go:216`), strips it, and
continues through the existing `parseFlags` with `options.serve = true`.
`serve` combined with `--ui`, `--approve`, `--resume`, `--continue`,
`--archive`, or `--no-session` exits with code 2. `--no-session` is rejected
because its in-memory sessions have no persisted file: `session.List` and
`Open` cannot find them, so they cannot be created or resumed by ID through
the server API.

Startup reuses the full existing chain unchanged: environment capture →
`loadConfig` → `configEnvironment` → workspace resolution → session root →
`runtimeBuilder` → sandbox → memory service. `options.serve` skips
`selectFrontend` (`cmd/otto/main.go:344-350`), which would otherwise error
when `OTTO_UI=tui` or `[ui].mode = "tui"` is set and stdin is not a
terminal. The serve branch is taken before `initialSession` is constructed
(`cmd/otto/main.go:550`): `if options.serve { return runServe(...) }`. Serve
mode does not pre-create a session; sessions are created or opened per HTTP
request through the session factories described in [Sessions](#sessions).

`serve` refuses to start with `errSessionOperationUnavailable` when
`!dynamicContent`, the same check already applied to `--resume`,
`--continue`, and `--archive` (`cmd/otto/main.go:288`). This is consistent
with `buildNewReplacement`, which returns the same error in that state
(`cmd/otto/runtime_builder.go:388`).

### Socket path and listener

Priority: `--socket` flag > `[server].socket` (TOML) > default
`~/.otto/otto.sock`. No environment variable.

`internal/server/listen.go`:

- If the socket's parent directory does not exist, it is created with
  `MkdirAll 0700`, the same permissions `internal/session/store.go:101-106`
  uses for the session directory. If it exists, its permissions are checked
  with the same ownership/mode test `internal/memory/sqlite/path_unix.go:79-89`
  uses: owner UID must equal the current UID and the mode must carry no
  group or world bits.
- If a socket already exists at the path, `net.Dial` probes it: a successful
  connection means a server is already listening there; otherwise the stale
  socket is removed. Existing non-socket files are rejected and preserved.
- After `net.Listen` succeeds, the socket is `chmod`ed to `0600`.
- `http.Server{ReadHeaderTimeout: 10 * time.Second}`; no `WriteTimeout`,
  because SSE responses stay open for the duration of a turn.
- The socket file is removed on shutdown.

First iteration does not listen on TCP and does not verify the connecting
peer's UID; see [Trust and safety](#trust-and-safety) for why the directory
and file permissions above are considered sufficient for this iteration.

### Signals and shutdown

SIGTERM is subscribed only inside the serve branch:
`signal.Notify(ch, syscall.SIGTERM)` calling `cancelProcess()` on receipt.
The shared `subscribeOSInterrupts` (`cmd/otto/main.go:148`, `os.Interrupt`
only) is left untouched, because the shared signal loop
(`cmd/otto/main.go:435-441`) routes every signal to `currentREPL.Interrupt()`
first and only falls back to `cancelProcess()` when no REPL claims it;
reusing that loop for SIGTERM in serve mode would risk a `kill` only
canceling the active turn instead of terminating the process.

Shutdown sequence: signal → `cancelProcess()` → `http.Server.Shutdown` (5
second grace period, then `Close`) → cancel every in-flight turn → `Close()`
every open `*app.Controller` → `runServe` returns → the existing
`closeMemoryService`/`closeSandbox` deferred calls run → the socket file is
deleted → process exits 0. Controllers must close before the memory service,
because the memory service's `Close` waits for operations still in flight.

`printUsage` (`cmd/otto/main.go:829`) gains a line for
`otto serve [options] [--socket PATH]` and a description of `--socket`.

## Sessions

### Controller assembly reuse

`cmd/otto/main.go:583-648` already assembles one `*app.Controller` inline.
That assembly is extracted into:

```go
func (b runtimeBuilder) newController(initial session.Session, runner app.Runner, info app.RuntimeInfo, dynamicContent bool) (*app.Controller, error)
```

in `cmd/otto/runtime_builder.go`. Inside it, the `build` closure passed to
`app.New` returns the supplied `runner` directly (no
`initialRunnerPending` flag), and the `create` closure returns
`errSessionOperationUnavailable`. Both are dead paths today:
`app.New` calls `build` exactly once (`internal/app/controller.go:353`), and
`WithNewSessionBuilder` is always set on every construction site, so
`build`'s second branch and the `create` closure inside
`buildReplacement` are unreachable (`internal/app/controller.go:506-518`).
`WithRuntimeInfo(info)` uses the `RuntimeInfo` passed to `newController`,
not the process-level `resolvedRuntime`, because a resumed session's
profile and model come from that session's file header, not from process
flags. The remaining options
(`WithDynamicContent`/`WithProfileSwitcher`/`WithDefaultProfileSetter`/
`WithNewSessionBuilder`/`WithMemory`/`WithSessionBrowser`/
`WithSessionArchiver`) are copied unchanged, since every field they close
over already lives on `runtimeBuilder`. The single-session startup path is
rewritten to call
`builder.newController(initialSession, initialRunner, builder.runtimeInfo(resolvedRuntime), dynamicContent)`;
existing tests are expected to pass unmodified.

### Session factories

Implemented in `cmd/otto`, passed into the server package as plain
functions. All three use `processCtx`, never an HTTP request context, and
wrap errors through `builder.redactError` before returning them.

- `Create(ctx)`: `builder.buildNewReplacement(ctx, builder.runtimeInfo(resolvedRuntime))`,
  then `builder.newController(rep.Session, rep.Runner, rep.RuntimeInfo, true)`.
- `Open(ctx, id)`: `session.List(ctx, sessionRoot, workspacePath, "", 20)`
  resolves `id` to a path, then `builder.openReplacement(ctx, path)`, then
  `newController`. `// ponytail: only the 20 most recent sessions can be
  resumed by id; session.PathForID would need exporting to reach further
  back.`
- `List(ctx)`: `session.List(..., 20)`; `errors.Is(err, os.ErrNotExist)`
  returns an empty result, since a fresh `$HOME` has no `~/.otto/sessions`
  directory yet.

`Create` and `Open` never use the request context because
`buildRunner(ctx)` → `buildProvider(ctx)` → `creds.TokenSource(ctx, path)`
retains that context for the lifetime of the token source
(`internal/auth/tokensource.go:35-40`); a request context would be canceled
when the HTTP request that created the session finishes, breaking token
refresh for everything built after that.

### Session registry and open/resume race

The server keeps `map[id]*openSession{ctrl *app.Controller; mu sync.Mutex; turn *turn}`
behind one registry lock. `POST /v1/sessions {"resume": "<id>"}` checks the
registry first: a hit returns the existing session object with HTTP 200
without calling `Open` again. A miss inserts an "opening" placeholder under
the lock before calling `Open`, so two concurrent resumes of the same ID
never call `Open` twice. This matters because `PrepareListed` opens the
session file `O_RDWR` without an flock
(`internal/session/prepared.go:214`): opening the same ID twice would
corrupt the parent-checkpoint chain in that file.

### Session endpoints

| Method path | Behavior |
|---|---|
| `POST /v1/sessions` `{}` or `{"resume":"<id>"}` | 201 new session, or 200 an already-open session; returns the session object |
| `GET /v1/sessions` | disk `session.List` merged with in-memory open sessions by id, each row carrying `open:true/false` |
| `GET /v1/sessions/{id}` | session object; the session must already be open, otherwise 404 |
| `DELETE /v1/sessions/{id}` | cancel any active turn, then `ctrl.Close()`, then 204 |
| `GET /v1/sessions/{id}/history` | `[]model.Message` (already has JSON tags) |

`DELETE` cancels before calling `Close`: `Close` blocks until any in-flight
`Prompt` returns (`internal/app/controller.go:1170-1183`), so canceling
first is required, or `DELETE` would block until the model finished on its
own.

### Session object

```json
{
  "id": "…",
  "workspace": "…",
  "provider": "…",
  "profile": "…",
  "model": "…",
  "context_window": 0,
  "usage": {"input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0},
  "context_input_tokens": 0,
  "sandbox": {"mode": "…", "network": "…", "bash_available": true, "summary": "…"},
  "turn": {"id": "…", "trigger": "…", "status": "…"}
}
```

`turn` is `null` when no turn has run yet or the last turn already finished
without a client still tracking it. Every field comes from `ctrl.Info()`
(`internal/app/controller.go:215`). The object carries no `path`: before the
first prompt the session file does not exist yet, `Store.Path()` reads it
non-atomically, and the `id` already works as the handle for every
endpoint. `GET /v1/sessions` list rows instead use `session.SessionInfo`
(`internal/session/types.go:78`), which does carry `path`, for callers that
want to resume by file path with `otto --resume PATH` outside the server.

```bash
curl -s --unix-socket ./otto.sock -X POST http://otto/v1/sessions -d '{}'
curl -s --unix-socket ./otto.sock http://otto/v1/sessions
```

## Turns and event streaming

### Turn state

```go
type turn struct {
    id        string
    cancel    context.CancelFunc
    mu        sync.Mutex
    events    []wireEvent      // seq = index
    changed   chan struct{}    // closed and replaced on every append (broadcast)
    done      bool
    status    string           // ok | error | canceled
    err       string
    text      strings.Builder  // accumulated text_delta
    usage     model.Usage      // accumulated provider_usage
    toolStart time.Time        // one tool runs at a time per turn (internal/agent/agent.go:146)
    startedAt, finishedAt time.Time
}
```

`emit` runs on the agent goroutine: it converts the `agent.Event` to its
wire form first (avoiding aliasing the `Compaction`/`Plan` pointers), then
locks, appends, and broadcasts. It performs no I/O, so a slow HTTP client
never applies backpressure to the agent. Each session keeps only the most
recent turn's event buffer.
`// ponytail: the per-turn event buffer has no byte cap; add one if it
matters.`

### Starting a turn

`POST /v1/sessions/{id}/turns`:

1. `strings.TrimSpace(text) == ""` → 400. `agent.ErrEmptyUserText` raised
   inside the agent goroutine would otherwise only surface as the new
   turn's error status, never reaching the HTTP layer, so the handler
   checks eagerly before starting a turn.
2. Under `openSession.mu`, a still-running previous turn → 409 `turn_active`.
   This is the only 409 source in the API; `app.ErrPromptActive` is never
   returned here, because the check happens before `Prompt` is even called.
3. Otherwise, a new turn is created and `go ctrl.Prompt(turnCtx, text, emit)`
   is started.

`turnCtx` derives from the context passed to `server.New` (the process's
long-lived context), not from the HTTP request context. A client
disconnecting mid-stream never cancels the turn.

Terminal status: `err == nil` → `ok`;
`errors.Is(err, context.Canceled)` → `canceled` (the same test
`internal/repl/repl.go:198` uses, still valid after redaction); anything
else → `error`. The agent already reports error text through an
`agent_error` event, so the server does not synthesize an additional
terminal event.

`DELETE /v1/sessions/{id}` and process shutdown both call `cancel()` before
`ctrl.Close()`, for the same reason given in [Sessions](#session-endpoints).

### Turn endpoints

| Method path | Behavior |
|---|---|
| `POST /v1/sessions/{id}/turns` `{"text","stream":true}` | `stream` defaults to true: 200 `text/event-stream` starting at seq 0; `stream:false` waits for the turn to finish and returns the turn summary |
| `GET /v1/sessions/{id}/turns/{turn_id}` | turn summary |
| `GET /v1/sessions/{id}/turns/{turn_id}/events?after=N` | SSE starting at seq N+1; also honors `Last-Event-ID` |
| `POST /v1/sessions/{id}/turns/{turn_id}/cancel` | calls `cancel()`, returns 202 |

Turn summary:

```json
{"id": "…", "trigger": "user", "status": "ok", "error": "", "text": "…", "usage": {"input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0}, "started_at": "…", "finished_at": "…"}
```

SSE frame format: `id: <seq>`, `event: <agent.EventType literal>`,
`data: <json>` — one wire event per frame, matching each `agent.EventType`
one to one (shapes in [Wire events](#wire-events)).

```bash
curl -N --unix-socket ./otto.sock -X POST http://otto/v1/sessions/<id>/turns -d '{"text":"list files"}'
curl -s --unix-socket ./otto.sock -X POST http://otto/v1/sessions/<id>/turns/<turn_id>/cancel
```

## Wire events

Every `agent.Event` (`internal/agent/events.go`) is converted to one wire
event with the same type name and this `data` shape:

| `agent.EventType` | `data` |
|---|---|
| `agent_started` | `{"turn_id": "<id>"}` — the only event with a field the `agent.Event` struct itself does not carry; it lets a client reading the SSE body alone learn the turn ID without a separate response header |
| `agent_finished` | `{}` |
| `text_delta` | `{"text": "<string>"}` |
| `tool_call_started` | `{"tool_name": "<string>", "tool_call_id": "<string>", "tool_args": <value>}` |
| `tool_call_finished` | `{"tool_name": "<string>", "tool_call_id": "<string>", "tool_result": {"content": "<string>", "is_error": <bool>}}` |
| `provider_usage` | `{"usage": {"input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0}}` |
| `compaction_started` | `{"compaction": {"checkpoint_id","reason","tokens_before","estimated_tokens_after","automatic","usage","usage_present","noop"}}` |
| `compaction_planned` | `{"plan": {"reason","automatic","tokens_before","estimated_tokens_after","summarized_messages","retained_messages","mode"}}` |
| `compaction_completed` | same shape as `compaction_started`, with the final checkpoint fields |
| `compaction_warning` | `{"error": "<string>"}` |
| `memory_warning` | `{"error": "<string>"}` |
| `agent_error` | `{"error": "<string>"}` |
| `notification` | `{"task_id": "<string>", "text": "<string>", "usage": {"input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0}}` — emitted when a sub-agent task finishes |

`tool_args` is `agent.Event.ToolArgs` (a string): when it parses as valid
JSON it is embedded as a `json.RawMessage` value, otherwise it is embedded
as a JSON string. `Err` fields become the string `err.Error()`; the agent
has already redacted these before they reach `emit`.

## Sub-agent tasks

See `docs/specs/2026-09-03-subagents-design.md` for the task registry
(`agent.Tasks`) and the `agent` tool that populates it. This section covers
only the server's use of that registry: wake turns, the task routes, and the
task metrics.

### Wake turns

Each `openSession` starts a goroutine (`startWakeLoop`) as soon as the
session opens, if the runner exposes a task registry (`app.TaskLister`); it
returns immediately otherwise.

That goroutine is the sole place that starts a task-triggered turn
(`startTurn(os, "", triggerTask)`). It reacts to two signals:

- `os.ctrl.Tasks().Updates()` — the registry changed (a task added, a task
  updated, or a notification pushed).
- `os.turnFinished` — a turn on this session just finished. `startTurn`
  sends this after every turn, regardless of trigger.

Both signals are handled by the same goroutine instead of each
independently calling `startTurn`. A task can finish between the parent's
last provider request and the turn actually returning, with no further
`Updates()` signal to catch it, so the end-of-turn check is needed. But if
that check ran inline on the finishing turn's own goroutine, it and a
concurrent `Updates()` reaction could both read `Tasks().Pending() > 0` from
the same notification and both start a turn — one call landing after the
other's wake turn has already drained the notification, producing two wake
turns for one notification. Routing both signals through one goroutine's
`select` loop makes every "`Pending() > 0`, then `startTurn`" decision
atomic with respect to the other trigger, so exactly one wake turn starts
per pending notification.

On each signal, if `Tasks().Pending() > 0` and no turn is active, the
goroutine starts a wake turn with `trigger: "task"` and empty text.
`errTurnActive` (a turn is already running) is expected and ignored: the
running turn drains its own inbox before its next provider request, so no
separate wake turn is needed while one is active. The goroutine exits when
`Tasks().Updates()` closes (the controller closed) or the server's context
is done.

A wake turn is an ordinary turn otherwise: it is recorded on the session,
its events stream over SSE, and both `GET /v1/sessions/{id}/turns/{turn_id}`
and the session's `turn` field carry `trigger: "task"` so a client can tell
it apart from a user turn (`trigger: "user"`).

### Task routes

| Method path | Behavior |
|---|---|
| `GET /v1/sessions/{id}/tasks` | `{"tasks": [Task, …]}`, in registry creation order; empty when the session has no task registry |
| `GET /v1/sessions/{id}/tasks/{task_id}` | a `Task` plus `"history": [model.Message, …]` in the same wire form as `GET .../history`; 404 `not_found` for an unknown id |
| `POST /v1/sessions/{id}/tasks/{task_id}/cancel` | cancels the task; 200 with the `Task` re-read after cancel; 404 `not_found` for an unknown id; 409 `task_done` if the task already finished |

`Task` (`internal/server/tasks.go`, `taskWire`):

```json
{
  "id": "t1",
  "agent": "reviewer",
  "description": "…",
  "model": "…",
  "status": "running",
  "created_at": "…",
  "started_at": "…",
  "finished_at": null,
  "steps": 2,
  "tool_calls": 3,
  "last_tool": "grep",
  "last_text": "…",
  "usage": {"input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0},
  "result": "",
  "error": ""
}
```

`model`, `started_at`, `finished_at`, `last_tool`, `last_text`, `result`, and
`error` are omitted when empty/zero. Fields map 1:1 from `agent.Task`
(`internal/agent/tasks.go`); the server never adds a field `agent.Task`
doesn't have. All three routes reach the registry only through
`app.Controller.Tasks()` (`app.TaskLister`), never through
`internal/subagent` or the runner directly.

```bash
curl -s --unix-socket ./otto.sock http://otto/v1/sessions/<id>/tasks
curl -s --unix-socket ./otto.sock http://otto/v1/sessions/<id>/tasks/t1
curl -s --unix-socket ./otto.sock -X POST http://otto/v1/sessions/<id>/tasks/t1/cancel
```

### Task metrics

- `otto_tasks_started_total` — counter, incremented once per task the wake
  loop first observes in `Tasks().List()`.
- `otto_tasks_finished_total{status}` — counter, `status` one of
  `succeeded`/`failed`/`canceled`, incremented once per task the first time
  its status is observed as final.
- `otto_tasks_running` — gauge, the number of tasks currently observed with
  status `running`, summed across sessions.

The server only sees tasks through `Tasks().List()`, not a push per
status change, so each session's wake-loop goroutine keeps a
`map[task ID]agent.TaskStatus` of what it last observed and diffs it
against `List()` on every `Updates()` signal (`metrics.diffTasks`). Because
`Updates()` is a capacity-1 channel that coalesces signals, a fast task can
go from unseen straight to a final status between two observations; that
case counts as both a start and a finish.

## Errors

Error body: `{"error":{"code","message"}}`.

| Situation | Status |
|---|---|
| Empty turn text | 400 |
| Session or turn does not exist | 404 |
| A turn is already active for the session | 409, `code: "turn_active"` |
| Anything else | 500, fixed body `internal error`; the real error goes only to the log line, already passed through `redactError` |

## Observability

### Metrics

`internal/server/metrics.go`, hand-written against the stdlib, no new
dependency. Labels never carry a path, a query parameter, or request/turn
text.

- `otto_http_requests_total{route,method,status}`
- `otto_http_request_duration_seconds{route}`
- `otto_sessions_open`
- `otto_turns_total{status}`
- `otto_turns_active`
- `otto_turn_duration_seconds`
- `otto_tool_calls_total{tool,status}`
- `otto_tool_call_duration_seconds{tool}` — measured from `toolStart` to the
  matching `tool_call_finished`, one active interval per turn since tool
  calls run one at a time
- `otto_provider_tokens_total{kind}`
- `otto_event_stream_clients`
- `otto_tasks_started_total`, `otto_tasks_finished_total{status}`,
  `otto_tasks_running` — see [Task metrics](#task-metrics)

Durations use fixed histogram buckets. `GET /metrics` renders all of these
in Prometheus text exposition format.

### Logs

`log/slog`, `TextHandler` to stderr, no new dependency.

- One line per HTTP request: method, route, status, duration_ms, request_id.
- One line per turn start and one per turn end: session_id, turn_id,
  status, duration_ms, tokens.
- Never logged: prompt text, tool arguments, tool output.

### Trace file concurrency fix

`internal/trace/roundtripper.go:111-114` currently issues two separate
`Write` calls per record. Under multi-session concurrency, multiple
`*RoundTripper` instances (one per runner) can share the same underlying
`*os.File`; each tripper's own mutex only serializes its own two writes, so
two trippers' writes can interleave mid-record. The fix merges the record
into one `rt.w.Write(append(line, '\n'))` call. `seq` stays independent per
tripper; duplicate sequence numbers across trippers are accepted, since the
trace file is a debugging aid, not the wire protocol.

## Trust and safety

- The session object never includes `path`; see
  [Session object](#session-object) for why. The session `id` is the only
  handle the API exposes.
- Error responses other than the documented 400/404/409 always return the
  fixed body `internal error`; the underlying error — already redacted by
  `builder.redactError` — appears only in the log line, never in the HTTP
  response.
- `X-Request-ID`: generated when absent; a client-supplied value is
  truncated to 64 bytes and reduced to printable ASCII before it is echoed
  back or written to a log line.
- Socket permissions ([Socket path and listener](#socket-path-and-listener))
  restrict the socket to the owning user: parent directory `0700`, socket
  file `0600`. This iteration relies on that instead of adding peer-UID
  verification or a TCP listener with authentication; both are deferred
  (see [Follow-ups](#follow-ups-not-in-this-pr)) because file-mode
  restriction already excludes every other local account on the same
  machine.
- `// ponytail: every session's bash subprocesses share the seatbelt
  driver's one private HOME/TMPDIR (driver_darwin.go:425-426), the same as
  today's single-process behavior; isolate per session if that becomes a
  problem.`

## Configuration

`internal/config/server.go` + `server_test.go`:

```go
type Server struct {
    Socket string `toml:"socket"`
}
```

`File` gains `Server Server `toml:"server"`` (`config.Load` is strict, so
the field must be present in the struct even though the TOML section is
optional). `ResolveServer(file, override, env) (ServerRuntime, error)`
applies override > file > default (`~/.otto/otto.sock`), with `~/`
expansion using the same `homeFromEnv`-based switch
`internal/config/skills.go:44-56` and `internal/config/memory.go:49-58`
already use.

## Package layout

| Location | Responsibility |
|---|---|
| `internal/server` (new) | HTTP server, routing, session registry, turn lifecycle, event conversion, metrics, socket listener |
| `internal/server/server.go` | `Server`, routes, session table, handlers |
| `internal/server/turn.go` | turn buffering, broadcast, cancellation |
| `internal/server/tasks.go` | task wire conversion (`taskWire`), the three task route handlers |
| `internal/server/events.go` | `agent.Event` → wire JSON |
| `internal/server/metrics.go` | counters/histograms and Prometheus text rendering |
| `internal/server/listen.go` | socket creation and permission checks |
| `internal/server/openapi.yaml` | OpenAPI 3.1 document, served via `go:embed` at `GET /v1/openapi.yaml` |
| `internal/config/server.go` (new) | `[server]` TOML struct and resolution |
| `cmd/otto/main.go` | `serve` dispatch, `--socket` flag, `options.serve`, conflicting-flag checks, skip `selectFrontend`, `runServe`, serve-only SIGTERM subscription, usage text |
| `cmd/otto/runtime_builder.go` | `newController` extraction |

`internal/server` imports only `internal/app`, `internal/agent`,
`internal/model`, `internal/session`, and `internal/tool` (for `Result`); it
never imports `cmd/otto`. Its ID generator uses `crypto/rand`, 16 bytes,
hex-encoded — a five-line duplicate of `cmd/otto`'s `randomID`, kept
separate because `cmd/otto` is not reachable from `internal/server`.

```go
type Options struct {
    Create func(ctx context.Context) (*app.Controller, error)
    Open   func(ctx context.Context, id string) (*app.Controller, error)
    List   func(ctx context.Context) (session.ListResult, error)
    Info   Info          // process-level static info: workspace, provider, profile, model, sandbox summary, profiles
    Logger *slog.Logger  // nil -> TextHandler to stderr
}
func New(ctx context.Context, opts Options) *Server  // ctx is the parent of every turn and every factory call
func (s *Server) Handler() http.Handler
func (s *Server) Close() error                        // cancel every turn, close every Controller
func Listen(path string) (net.Listener, error)        // listen.go
func Serve(ctx context.Context, l net.Listener, s *Server) error // http.Server lifecycle
```

Routing uses the Go 1.22+ `http.ServeMux` method-and-path-value form (for
example `"POST /v1/sessions/{id}/turns"`, read with `r.PathValue`); the
metrics `route` label uses `r.Pattern`. Request bodies are capped with
`http.MaxBytesReader` at 1 MiB.

## Development plan

Each phase: failing test first, minimal code, focused `go test`, `make
check`, one commit.

| Phase | Deliverable | Focused test |
|---|---|---|
| 1 | spec doc (this file) | — |
| 2 | `internal/config/server.go` | `[server].socket` parsing, `~/` expansion, override > file > default |
| 3 | `internal/trace/roundtripper.go` single-write fix | two trippers sharing one writer under `-race`, records never interleave |
| 4 | `internal/server/metrics.go` | counter/gauge/histogram text output and labels |
| 5 | `internal/server/events.go` | JSON shape per `agent.EventType`; `Err` to string; `ToolArgs` as `RawMessage` |
| 6 | `internal/server/turn.go` | concurrent readers from an arbitrary seq, cancel, done state, text/usage accumulation, `-race` |
| 7 | `internal/server/server.go` | real `*app.Controller` built from `app.New(session.NewMemory(hdr), ...)` plus a scripted fake `app.Runner`; `httptest.NewServer` covers every endpoint: 201/200/204/404/409/400, SSE streaming with `after` and `Last-Event-ID`, `stream:false`, cancel, `DELETE` canceling an active turn, resume of an already-open session returning 200 without a second `Open`, two concurrent resumes calling `Open` once, two sessions running turns concurrently, metrics/logs free of prompt text, fixed 500 body |
| 8 | `internal/server/listen.go` | `t.Chdir(t.TempDir())` with a relative socket path (avoids the 104-byte `sun_path` limit); `0600` permission, parent-directory permission check, stale-socket cleanup, non-socket preservation, "already running" |
| 9 | `openapi.yaml` + `go:embed` | every `ServeMux` pattern appears under `paths:` in the embedded document |
| 10 | `cmd/otto` wiring | `newController` extraction (existing tests unchanged) → `serve` dispatch/`--socket`/conflicting-flag checks/skip `selectFrontend` → SIGTERM → `List` on `ENOENT` returns empty → end-to-end test: `run(ctx, ["serve","--config",cfg,"--cwd",ws,"--socket","otto.sock"])` in a goroutine, an `http.Transport{DialContext: unix}` client creates a session, posts a turn, reads SSE text from a fake provider, lists sessions, then `ctx` is canceled and the test asserts exit code 0 and socket-file removal |
| 11 | docs | README "Configuration and precedence" + new "Agent server" section, `docs/user-manual.md` command table and config example plus a new "Agent server" chapter, `CLAUDE.md` architecture bullet 9 (and fixing bullet 6's link to the now-deleted `docs/superpowers/` path to point at `docs/specs/`) |
| 12 | `make check` | fmt, vet, staticcheck, test, test-race, `git diff --check`; small commits; open the PR |

Verification:

```bash
go test ./internal/server -race -count=1
go test ./internal/config -run Server
go test ./internal/trace -race
go test ./cmd/otto -run Serve -count=1
make check
```

Manual smoke test:

```bash
make build && ./otto serve --socket ./otto.sock
# in another terminal:
curl -s --unix-socket ./otto.sock http://otto/healthz
curl -s --unix-socket ./otto.sock -X POST http://otto/v1/sessions -d '{}'
curl -N --unix-socket ./otto.sock -X POST http://otto/v1/sessions/<id>/turns -d '{"text":"list files"}'
curl -s --unix-socket ./otto.sock http://otto/metrics | grep otto_
# then: kill -TERM <pid>; confirm exit code 0 and that the socket file is gone.
```

## Follow-ups, not in this PR

- TCP listening and authentication.
- `POST /v1/sessions/{id}/compact`.
- Event replay across turns (only the most recent turn per session is
  buffered).
- Session idle eviction and an open-session cap.
- SSE heartbeats.
- Peer-UID verification on the socket.
- Profile-switch and session-archive endpoints.
- Resuming a session older than the 20 most recent by ID.
- Wiring `memory.Binding.Observe`.

## Decisions taken

- Multi-session model: one process, one workspace, N sessions each with
  its own `*app.Controller`, turns in different sessions run concurrently
  (user decision, 2026-09-03).
- Unix socket only, no TCP, in this iteration: the socket's directory and
  file permissions already restrict access to the owning user, and adding
  network exposure would require authentication this iteration does not
  build.
- HTTP+JSON+SSE instead of JSON-RPC or gRPC: matches the reference APIs
  this design follows (Docker Engine API's HTTP-over-Unix-socket shape,
  OpenAI Responses' `stream` field and named SSE events, ACP's
  session/new-prompt-cancel semantics) without adding an RPC framework
  dependency.
- Hand-written Prometheus exposition and `log/slog` instead of a metrics or
  logging library: the repository has no metrics/slog/expvar dependency
  today, and the metric and log surface here is small enough not to justify
  one.
- A turn runs under the server's root context, not the HTTP request
  context; a client disconnect never cancels a turn, so a turn already
  billed to the provider always finishes and is recorded.
- Only the most recent turn per session is buffered, not full history,
  to bound memory use without adding a byte-level cap in this iteration.
- The session object carries no `path` field: before the first prompt the
  session file does not exist and `Store.Path()` is not safe to read
  concurrently, and the session `id` already serves as the handle for
  every endpoint.
- SIGTERM is subscribed only in serve mode, not through the shared
  `subscribeOSInterrupts` path, because the shared signal loop hands every
  signal to `currentREPL.Interrupt()` first, which would only cancel a
  turn instead of terminating a server process.
- `--no-session` is rejected in combination with `serve`: its in-memory
  sessions have no file for `session.List`/`Open` to find, so they cannot
  be created or resumed through the server API.
- Resuming a session by ID is limited to the 20 most recent, matching the
  existing cap in `session.List` (`internal/session/list.go:18`).
