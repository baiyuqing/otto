# Sub-agents design

Status: draft, 2026-09-03, pending approval. This version replaces the
synchronous draft of the same date: the parent model no longer blocks inside
the `agent` tool call; sub-agents run as background tasks and report back
through notifications.

## Goal

Let the model delegate a self-contained task to a child agent loop that runs
concurrently with the parent turn, in its own context, with the same workspace,
sandbox, and tools. The parent keeps working, sees the child's status and
transcript, receives the child's final report as a message in its own context,
and can send messages to the child while it runs.

Scope (L3, decided): the `agent` tool, named agent definitions (`AGENT.md`,
discovered like skills), and parallel execution with a configurable limit.

## Terms

- **task**: one run of a sub-agent. Identified as `t1`, `t2`, … within the
  current session; the counter resets when the session is replaced (`/new`,
  `/resume`, `/model`).
- **parent**: the agent loop of the session (`agent.Agent` built by
  `runtime_builder.buildRunner`).
- **child**: an `agent.Agent` created per task, run in its own goroutine with an
  in-memory session (`session.NewMemory`). Depth is fixed at 1: a child never
  receives any `agent*` tool.
- **notification**: a text item queued for delivery into an agent's next
  provider request. Terminal task states and child reports produce
  notifications for the parent; `agent_send` produces notifications for a
  child.
- **wake turn**: a parent turn started by a frontend with empty prompt text,
  because notifications are pending and no turn is active.

## One delegation, end to end

1. The parent model calls `agent {prompt: "find where sessions are written"}`.
   The tool registers task `t1`, starts the child goroutine (or queues it when
   `max_parallel` children are running), and returns
   `task t1 started` within the same tool call. The parent turn continues:
   the model may call other tools, start more tasks, answer the user, or call
   `agent_wait`.
2. The child runs its own turn loop against the same provider until it
   finishes without tool calls. Every child event updates the task record
   (step count, last tool, last text, usage) in the task registry, which
   signals frontends.
3. On completion the child's final assistant text becomes the task result, and
   a notification is pushed into the parent's inbox.
4. If the parent turn is still running, `Agent.Run` drains the inbox before its
   next provider request, so the result reaches the model in the same turn. If
   the parent is idle, the frontend observes the registry signal and starts a
   wake turn; `Agent.Run` drains the inbox at the start of that turn.
5. Drained notifications are appended to the parent session as context
   messages and emitted as `notification` events, so the transcript, the
   session file, and the model all see the same text.

## Task lifecycle

```
queued ──► running ──► succeeded
                   ├──► failed      (provider or tool error, child loop error)
                   └──► canceled    (context canceled: /task cancel, API cancel,
                                     session replacement, process exit)
```

A task record holds: `ID`, `Agent` (definition name or empty), `Description`,
`Prompt`, `Context` (`fresh`/`inherit`), `Status`, `StartedAt`, `FinishedAt`,
`Steps` (provider round trips), `ToolCalls`, `LastTool` (name plus argument
preview), `LastText` (last assistant text, capped at 500 bytes), `Usage`
(`model.Usage` totals), `Result` (final report, uncapped), `Error`.

Only terminal states and explicit reports produce notifications. Queued →
running does not; `agent_status` and the frontends show it.

## Tools available to the parent

All parameter objects use `additionalProperties: false` and are decoded with
`tool.DecodeStrictJSON`.

### `agent`

Parameters:

| name | type | required | meaning |
|---|---|---|---|
| `prompt` | string | yes | The complete task. The child sees nothing else unless `context` is `inherit`. |
| `description` | string | no | Short label (≤ 80 chars) shown in status output and the TUI panel. |
| `agent` | string | no | Definition name. `enum` lists the catalog when non-empty. Unknown name → error result. |
| `context` | `"fresh"` \| `"inherit"` | no | Default `fresh`, or the definition's `context` field. See "Context: fresh or inherit". |
| `wait` | bool | no | `true` = start and block until the task ends; equivalent to `agent` followed by `agent_wait`. |

Result on success: `task t3 (explorer) started` or `task t3 (explorer) queued
(2 running, limit 2)`. With `wait: true` the result is the task's completion
text (see `agent_wait`).

Description text given to the model: "Start a sub-agent on a self-contained
task and return immediately with its task id. The sub-agent runs in parallel
with you, has its own context (fresh unless context is "inherit"), the same
workspace and file tools, and cannot see this conversation. Its final report
arrives later as a [task-notification] message. Use agent_wait when you need
the result before continuing, agent_status to check progress. Put everything
the sub-agent needs into prompt: goal, relevant paths, what to report back."

### `agent_wait`

Parameters: `task_id` (optional; omitted = every task that is queued or
running), `timeout_seconds` (optional integer, default 600, max 3600).

Blocks the parent turn until the selected tasks reach a terminal state or the
timeout elapses, then returns the completion text of each task (the same text
the notification would carry). A completion notification returned by
`agent_wait` is removed from the inbox so it is not delivered twice.

Turn cancellation (Esc, `POST …/cancel`) ends the wait with an error result
("wait canceled; task t3 still running"); the task itself keeps running.
Timeout returns an error result naming the tasks still running.

### `agent_status`

Parameters: `task_id` (optional).

Without `task_id`: one line per task in the session:

```
t1  explorer   running    12s   4 tools  grep "session"        find where sessions are written
t2  (default)  succeeded  42s   7 tools  12,310 tokens         review the diff
t3  reviewer   queued                                          run the tests
```

With `task_id`: the same line, then the last 10 steps of the child (tool name
and argument preview per step, assistant text capped at 500 bytes), then the
result or error when finished.

### `agent_send` (Phase C)

Parameters: `task_id`, `message`.

Pushes `[message from parent]\n<message>` into the child's inbox. The child
drains it before its next provider request. Error result when the task is not
queued or running.

### `agent_cancel` (Phase C)

Parameters: `task_id`. Cancels the child's context. The task ends as
`canceled` and produces the usual notification.

## Tools available to the child

The child receives the parent tool set minus `agent`, `agent_wait`,
`agent_status`, `agent_send`, `agent_cancel`, `remember`, `forget`, and
`memory_search` (children get no memory binding; see "Decisions taken"). A
definition's `tools` field narrows the set further.

### `agent_report` (child only, Phase C)

Parameters: `message`.

Pushes `[task-notification] task t1 (explorer) reports:\n<message>` into the
parent's inbox without ending the task. Intended for progress notes and
questions the child cannot answer alone. The child does not block for a reply;
if the parent answers with `agent_send`, the reply reaches the child at its
next provider request.

## Notifications

### Inbox

`agent.Inbox` (new, `internal/agent/inbox.go`): a mutex-protected FIFO of
`agent.Notification{TaskID, Kind, Text, Usage *model.Usage}` with `Push`,
`Drain() []Notification`, `Remove(taskID, kind)`, `Len()`, and
`Signal() <-chan struct{}` (capacity 1, non-blocking send on every push).
`agent.Options.Inbox *Inbox` is set for the parent (the registry's
notification inbox) and for each child (its own inbox).

Notification text formats (the model sees exactly these):

```
[task-notification] task t1 (explorer) succeeded · 42s · 7 tool calls · 12,310 tokens
<final report, capped at max_output_bytes; full text via agent_status/TaskHistory>

[task-notification] task t1 (explorer) failed · 12s · 3 tool calls
<error text>

[task-notification] task t1 (explorer) canceled · 12s · 3 tool calls

[task-notification] task t1 (explorer) reports:
<message>
```

The definition name is shown in parentheses; unnamed tasks show `(default)`.

### Delivery inside `Agent.Run`

`Agent.Run(ctx, text, emit)` drains `Options.Inbox` at two points:

1. At turn start. When `text` is non-empty the user message is appended
   first, then the drained notifications. When `text` is empty and the inbox is
   non-empty, no user message is appended (wake turn). When `text` is empty and
   the inbox is empty, `Run` fails with `ErrEmptyUserText` as today.
2. Before every subsequent provider request in the tool loop (after tool
   results are appended).

Each drained notification is appended to the session as one message:

```go
model.Message{
    ID: NewID(), Role: model.RoleContext, ContextType: "task_notification",
    Display: true, CreatedAt: Now(), Usage: n.Usage,
    Blocks: []model.Block{{Type: model.BlockText, Text: n.Text}},
}
```

`openaicompat` already sends `RoleContext` messages with the `user` role
(`protocol.go:98`). The agent emits `EventNotification{Text, TaskID, Usage}`
for each appended notification. Memory recall is skipped on wake turns (there
is no user text to query with).

Delivered notifications never carry redaction-sensitive material unfiltered:
the child's transcript and result pass through the same `agent.Redactor` as
the parent's, and the notification text is passed through
`Redactor.RedactString` before it is appended.

### Wake turns

The frontends own the decision to start a turn, as they do for user prompts:

- Condition: `Tasks().Pending() > 0` (inbox length) and no turn is active and
  no session replacement is pending and, in the TUI, no modal is open.
- Action: `Backend.Prompt(ctx, "", emit)`. `app.Controller.Prompt` passes the
  empty text through; `Agent.Run` applies the rule above.
- Trigger points: every registry signal (`Tasks().Updates()`), and the end of
  every turn (a task may finish after the parent's last provider request but
  before the turn returns; the inbox is then non-empty with no further
  signal).

A wake turn is an ordinary turn from the frontend's point of view: the running
spinner shows, Esc cancels it, `ErrPromptActive` applies. Notifications already
appended to the session are not lost when a wake turn is canceled; the model
sees them in the next turn.

If the parent turn is running when a task finishes, no wake turn is needed:
the running turn drains the inbox before its next provider request. If the
parent's turn ends without another provider request (the model already
produced its final answer), the end-of-turn check starts a wake turn.

### Persistence

Notifications are persisted as Pi v3 `custom_message` entries
(`customType: "task_notification"`, `display: true`). Decoding already exists
(`internal/session/context.go:343`); `Store.Append` currently rejects
`RoleContext` (`store.go:601`, "unsupported message role") and gains an encode
branch: `RoleContext` with a `ContextType` other than the reserved
`compaction` and `branch_summary` is written as `custom_message`.
`session.Memory` accepts any role already.

On `/resume` the notifications appear in the transcript as system entries
(`EntriesFromHistory` maps displayed `RoleContext` messages to `EntrySystem`),
and the model sees them in later requests. Task records and child transcripts
are not persisted (see Follow-ups).

## Child construction

`internal/subagent.Runner` builds and runs children. It is constructed once per
parent runner build in `buildRunner` with:

```go
type Runner struct {
    Provider       provider.Provider
    Tools          []tool.Tool          // parent set minus the excluded tools
    Redactor       *agent.Redactor
    Options        agent.Options        // Model, Thinking, Compaction copied from the parent; Memory nil
    PromptFor      func(defs []model.ToolDefinition) string // parent static prompt + workspace/skills tail for the child's tool set
    Header         session.Header       // template; ID replaced per child
    Catalog        Catalog
    Tasks          *agent.Tasks
    ParentSession  func() []model.Message // request view of the parent session, for inherit
    MaxParallel    int
    MaxOutputBytes int
    NewID          func() string
    Now            func() time.Time
}
```

Per task: resolve the definition → filter tools → `tool.NewRegistry` → system
prompt = `PromptFor(childDefinitions)` + `"\n\n## Sub-agent role\n"` +
(definition body, or the generic instruction) → `Redactor.RedactString` →
`session.NewMemory(header)` (with the inherited messages appended when
`context: inherit`) → `agent.New(provider, registry, memory, options, redactor)`
with `options.Inbox = task.inbox`, `options.Model` overridden by the
definition's `model` when set → goroutine: `child.Run(taskCtx, prompt, emit)`.

Generic instruction: "You are running as a sub-agent of Otto. Complete only
the delegated task below with the available tools, then reply with a
self-contained final report. That final message is returned to the caller as
your result; nothing else you write is."

The child emit callback does not forward events to the parent's frontend. It
updates the task record: `Steps` on `provider_usage`, `ToolCalls`/`LastTool`
on `tool_call_started`, `LastText` on `text_delta` (accumulated per assistant
message), `Usage` on `provider_usage`. Every update signals the registry.

Result extraction: the text blocks of the last assistant message, joined.
Empty → the task still succeeds with the text "(sub-agent returned no final
text)". `child.Run` error → `failed` with the error text; `context.Canceled` →
`canceled`. `child.Close()` runs after the goroutine ends.

### Context: fresh or inherit

- `fresh` (default): the child's session contains only its system prompt and
  the delegated prompt.
- `inherit`: before the prompt, the child's memory session receives a copy of
  the message list the parent would send in its next provider request (the
  post-compaction view, including any `[Compaction summary]` context message),
  minus the trailing assistant message that carries the not-yet-answered
  `agent` tool call(s). The copy is taken at task start under the parent's turn
  (the `agent` tool executes inside the parent turn, so the parent session
  does not change concurrently).

Cost of `inherit`: every child step resends the whole snapshot. A 60,000-token
parent context and 8 child steps cost 480,000 input tokens (prefix caching
lowers the price, not the count). The tool description says this; definitions
that need the conversation should set `context: inherit` explicitly.

### Concurrency limit

`Runner.MaxParallel` (default 4) is a counting semaphore across all tasks of
the session. `agent` returns `queued` when the semaphore is full; the task
starts when a slot frees, in FIFO order. Queued tasks can be canceled.

Components shared by concurrent children, checked before enabling parallelism:
the provider client (`net/http.Client` is goroutine-safe; the `openaicompat`
client keeps no per-call state), `agent.Redactor` (read-only after
construction), `tool.Registry` (read-only), the Seatbelt `bash` executor
(`internal/sandbox/executor.go:19` holds a mutex; each execution is a separate
process). Any of these found to be non-reentrant is serialized with a mutex in
the runner, not redesigned.

## Task registry and frontend access

`agent.Tasks` (new, `internal/agent/tasks.go`) is the per-session registry:

```go
func (t *Tasks) List() []Task
func (t *Tasks) Get(id string) (Task, bool)
func (t *Tasks) History(id string) ([]model.Message, bool)   // child session messages, live while running
func (t *Tasks) Cancel(id string) error
func (t *Tasks) Pending() int                                 // notifications waiting for the parent
func (t *Tasks) Updates() <-chan struct{}                     // capacity 1, signaled on every change
func (t *Tasks) Notifications() *Inbox                        // the parent's inbox
func (t *Tasks) Close()                                       // cancel all, close Updates
```

`agent.Options.Tasks *Tasks` and `Agent.Tasks()` expose it; `Agent.Close`
calls `Tasks.Close()`. `app.Controller.Tasks() *agent.Tasks` returns the
active runner's registry through a type assertion on `Runner`
(`interface{ Tasks() *agent.Tasks }`), nil when absent. Runner replacement
closes the old runner (`closeRunner`), which cancels its tasks; frontends
re-read `Tasks()` after every replacement and after `Updates()` closes.

`History(id)` reads the child's `session.Memory.Messages()`, which is
mutex-protected (`memory.go:14`), so a frontend can show the child's
provider interaction while the child runs.

## Frontends

### TUI

- **Task panel** in the live region between the transcript and the editor,
  shown while any task is queued or running (at most 6 lines: header plus 5
  tasks, then `+N more`; `calculateLayout` gains a `taskLines` argument):

  ```
  tasks  2 running · 1 queued
   ⠋ t1 explorer   12s  4 tools  grep "session"        find where sessions are written
   ⠋ t2 (default)   3s  1 tool   read internal/…       review the diff
   · t3 reviewer   queued                              run the tests
  ```

  Refreshed on `taskUpdateMsg` (a `tea.Cmd` that waits on `Updates()` and
  re-arms itself) and on the existing spinner tick while tasks run.
- **Notifications** render as `EntrySystem` entries, both live (from
  `EventNotification`) and from history. The header line is followed by the
  first 20 lines of the report and `… (N more lines; /task t1)`.
- **Wake**: `maybeWake()` runs on every `taskUpdateMsg` and at the end of
  `finishTurn`; it calls a variant of `startPrompt` with empty text that adds no
  `EntryUser`. While a wake turn runs, typed text stays in the editor; submit
  is ignored as during any turn.
- **Commands**: `/tasks` (all tasks of the session, including finished, with
  durations and tokens), `/task <id>` (the child's transcript rendered as a
  system entry: one line per tool call, assistant text in full), `/task cancel
  <id>`. `/new`, `/resume`, `/model` cancel running tasks and print
  `canceled N running tasks` as a system entry.
- `toolArgumentPreview` gains cases for `agent` (description, else definition
  name plus the first 60 chars of the prompt), `agent_wait` (task id or "all"),
  `agent_send` (task id).

### REPL

- Events: `[tool] agent (…)` as today; `[notification] task t1 (explorer)
  succeeded · 42s …` followed by the report body.
- Wake: the `Run` loop already reads stdin from a goroutine; its `select` gains
  a case on `backend.Tasks().Updates()` (re-read each iteration; nil channel
  when absent). On signal with pending notifications and no active turn it
  runs the wake turn, prints its output, and reprints the prompt.
- Commands: `/tasks`, `/task <id>`, `/task cancel <id>` with the same output
  as the TUI in plain text.

### Server

- Each `openSession` starts a goroutine on `ctrl.Tasks().Updates()` that starts
  a wake turn (`startTurn(os, "", triggerTask)`, factored out of
  `handleStartTurn`) when notifications are pending and no turn is active.
  Turn summaries and `GET …/turns/{turn_id}` gain `trigger: "user" | "task"`.
  Clients discover wake turns through `GET /v1/sessions/{id}` (the `turn`
  field) and stream them like any turn.
- Routes: `GET /v1/sessions/{id}/tasks` (list), `GET
  /v1/sessions/{id}/tasks/{task_id}` (record plus `history` in the same wire
  form as session history), `POST /v1/sessions/{id}/tasks/{task_id}/cancel`.
- Wire events: new type `notification` with `task_id`, `text`, `usage`.
- Metrics: `otto_tasks_started_total`, `otto_tasks_finished_total{status}`,
  `otto_tasks_running`.
- `openapi.yaml` and `docs/specs/2026-09-03-agent-server-design.md` updated
  in the same PR.

## Agent definitions

```
~/.otto/agents/reviewer/AGENT.md
---
name: reviewer
description: Review a diff for correctness and missing tests. Use after code changes.
tools: read, grep, find, ls, bash
model: gpt-4o-mini
context: fresh
---
You review code. Report findings as file:line bullets ordered by severity.
```

- `name`: same rules as skills (`^[a-z0-9]+(-[a-z0-9]+)*$`, ≤ 64 chars) and
  equal to the directory name.
- `description`: required, ≤ 1024 runes; shown to the model in the `## Agents`
  section and used to pick a definition.
- `tools`: optional comma-separated allowlist, each `^[a-z0-9_]+$`; unknown
  names produce a startup warning and are ignored; an empty value produces the
  warning "tools must be a comma-separated list" (the frontmatter parser drops
  YAML block lists silently).
- `model`: optional provider model id; overrides the parent model for this
  definition. This is the cost knob: definitions for exploration should name a
  cheaper model.
- `context`: optional, `fresh` (default) or `inherit`; the tool parameter
  overrides it.
- Body: Markdown after the frontmatter, appended to the child system prompt
  under `## Sub-agent role`. Empty body → the generic instruction.

Definitions are instruction text of the same trust class as `SKILL.md`,
`AGENTS.md`, and `CLAUDE.md`. They pass through the redactor and cannot widen
the tool set, escape the workspace, or change the sandbox policy.

## Discovery

`subagent.Discover(roots) (Catalog, []string)` mirrors `skill.Discover`: one
directory level per root, `AGENT.md` required, later roots win on name
conflict, missing roots are silent, invalid files produce warnings and are
skipped. Discovery runs on every runner build (startup, `/new`, `/resume`,
`/model`) and the catalog is fixed for the session, for prompt-cache stability.

The parent system prompt gains a `## Agents` section rendered by
`subagent.PromptSection(catalog)`:

```
## Agents

Named sub-agent definitions for the `agent` tool (`agent` parameter):

<available_agents>
<agent name="reviewer">Review a diff for correctness and missing tests. Use after code changes.</agent>
</available_agents>
```

Same 8 KiB cap and drop warning as `skill.PromptSection`. Empty catalog → no
section; the `agent` tool still works without a definition.

## Configuration

```toml
[agents]
enabled = true                              # default; false removes all agent* tools
paths = ["~/.otto/agents", ".otto/agents"]  # default; later wins on name conflict
max_parallel = 4                            # default; concurrent children per session, 1..16
```

`config.ResolveAgents(file, env, workspacePath) AgentsRuntime{Enabled, Roots,
MaxParallel}`; the path-expansion loop is shared with `ResolveSkills`. TOML
only; no flags or environment variables (same as `[skills]`). Existing agent
roots are added to the Seatbelt read paths at process start, like skill roots.

## Trust and safety

- Children run in the same workspace with the same `tool.Workspace`
  confinement and the same sandbox settings; `bash` in a child is under
  Seatbelt exactly as in the parent, and only explicit sandbox `off` is
  unsandboxed.
- Children never receive `agent*`, `remember`, `forget`, or `memory_search`.
- The redactor covers the child system prompt, child messages, task results,
  notification text, and task history returned to frontends.
- `agent_send` messages and definition bodies are model- or user-authored text
  and get no privileges beyond being present in a prompt.
- Cancellation is always available: Esc/API cancel for the parent turn,
  `/task cancel` and the cancel endpoint for a child, `CancelAll` on session
  replacement and on `Controller.Close`. `Close` does not wait for child
  goroutines; they end on context cancellation.

## Cost and usage accounting

- Child usage is accumulated in `Task.Usage` and shown by `agent_status`,
  `/tasks`, the task panel, and the task endpoints.
- The completion notification carries the child's total `Usage`; the TUI adds
  it to the live footer counter through `EventNotification`, and the persisted
  `custom_message` does not keep usage, so after `/resume` the session total
  excludes child usage. `Info().Usage` (session aggregate) never includes child
  usage. This is a known gap, listed under Follow-ups.
- Cost controls: `max_parallel`, per-definition `model`, `context: fresh` by
  default, `agent_wait` timeouts.

## Persistence and resume

- Parent session: `agent*` tool calls and results, and notifications as
  `custom_message` entries. All append-only.
- Child transcripts: in memory for the life of the Controller session;
  readable through `TaskHistory`, `/task <id>`, and the task endpoint. Lost on
  `/new`, `/resume`, `/model`, and exit.
- A task that is still running at exit is canceled; no notification is
  written for it.

## Package layout

| package | change |
|---|---|
| `internal/agent` | `inbox.go` (`Inbox`, `Notification`), `tasks.go` (`Task`, `TaskStatus`, `Tasks`), `Options.Inbox`, `Options.Tasks`, `Agent.Tasks()`, inbox drain and wake-turn rule in `Run`, `EventNotification`. No import of `internal/subagent`. |
| `internal/subagent` (new) | `definition.go`, `prompt.go`, `runner.go`, `inherit.go`, `tools.go` (`agent`, `agent_wait`, `agent_status`, `agent_send`, `agent_cancel`, `agent_report`). Imports `agent`, `tool`, `session`, `provider`, `model`, `skill` (frontmatter). Never imported by `agent`, `app`, or the frontends. |
| `internal/tool` | `Registry.Lookup`, `Registry.Tools`; export `DecodeStrictJSON`, `CappedTextResult`. |
| `internal/skill` | export `ParseFrontmatter`; still imports nothing from Otto. |
| `internal/session` | `Store.Append` encodes `RoleContext` messages as `custom_message`. |
| `internal/config` | `agents.go`, `File.Agents`, shared root expansion with skills. |
| `internal/app` | `Controller.Tasks()`; replacement and `Close` already close the old runner. |
| `cmd/otto` | `buildRunner` wiring, `boundaryToolDefinitions`, `systemPromptFor` guidance line for delegation, Seatbelt read paths for agent roots. |
| `internal/tui`, `internal/repl`, `internal/server` | as described under Frontends. |

AGENTS.md gains the `internal/subagent` package entry and the rule: "children
are built only through `agent.New`; `internal/agent` knows tasks only through
`agent.Tasks`/`agent.Inbox`; children never receive `agent*`, `remember`,
`forget`, or `memory_search`; child transcripts are not persisted."

## Development plan

Each phase is one PR on `feat/subagents`-derived branches; `make check` and
`make test-tui` are green at the end of each. TDD throughout.

### Phase A1: core loop, registry, three tools, REPL

1. `internal/tool`: `Lookup`/`Tools`, exported helpers.
2. `internal/agent`: `Inbox`, `Tasks`, `Options.Inbox/Tasks`, drain at turn
   start and before each provider request, wake-turn rule for empty text,
   `EventNotification`, notification message shape.
3. `internal/session`: `RoleContext` encode as `custom_message`; round-trip
   test against the existing decoder.
4. `internal/subagent`: `Runner` (goroutine per task, semaphore, task record
   updates from child events, result extraction, cancellation), tools `agent`
   (without `agent`/`context` parameters), `agent_wait`, `agent_status`.
5. `cmd/otto`: wiring in `buildRunner`, `boundaryToolDefinitions`, one
   guidance line in `systemPromptFor`. `app.Controller.Tasks()`.
6. REPL: notification rendering, wake from the `Run` loop select, `/tasks`,
   `/task <id>`, `/task cancel <id>`.
7. TUI and server compile and render `notification` events as system text; no
   panel, no wake yet (documented as Phase A2).
8. Docs: README "Sub-agents" section (Phase A1 scope only), AGENTS.md,
   CLAUDE.md.

### Phase A2: TUI and server

1. TUI: task panel, `taskUpdateMsg` loop, `maybeWake`, `/tasks`, `/task`,
   `/task cancel`, argument previews, cancel-on-replacement notice.
2. Server: wake goroutine, `trigger` field, task routes, `notification` wire
   event, metrics, `openapi.yaml`, agent-server spec update.

### Phase B: definitions, configuration, inherit

1. `skill.ParseFrontmatter`; `subagent/definition.go` with the validation
   rules above; `prompt.go`.
2. `config/agents.go`; `[agents]` in README precedence docs; Seatbelt read
   paths.
3. `agent` tool: `agent` and `context` parameters, `model` override,
   definition body in the system prompt; `inherit.go` snapshot rule.
4. `## Agents` prompt section in `buildRunner`.

### Phase C: two-way messages

`agent_send`, `agent_cancel`, `agent_report`; child inbox drain is already in
place from A1 (the child is an `agent.Agent` with `Options.Inbox`).

### Phase D (optional): child transcript persistence

Write each child session as JSONL under the parent's session directory with
Pi v3 `parentSession` set; `/task <id>` and the task endpoint read it after
resume. Not scheduled.

## Testing

All offline, next to their packages.

- `internal/agent`: drain order (user message, then notifications); drain
  before the second provider request when a notification arrives during a tool
  call; empty text with empty inbox → `ErrEmptyUserText`; empty text with a
  pending notification → no user message appended, provider called once;
  `EventNotification` emitted with the persisted text; memory recall skipped
  on wake turns; `Tasks.Close` closes `Updates()`.
- `internal/subagent`: a package-local fake provider that routes on the last
  user message and supports concurrent calls (the `scriptedProvider` in
  `agent_test.go` pops sequentially and is unsuitable); `agent` returns
  immediately and the task reaches `succeeded` with the last assistant text;
  child registry excludes the listed tools; `agent_wait` returns the result and
  removes the notification; timeout and cancel results; `max_parallel` = 2 with
  3 tasks never has more than 2 children in flight (atomic counter, barrier,
  5 s test timeout); cancel → `canceled` notification; child error → `failed`;
  `inherit` snapshot drops the trailing `agent` call; `-race` clean.
- `internal/session`: `RoleContext` append/decode round trip; reserved types
  rejected.
- `internal/tui`: panel lines; wake starts only when idle and pending;
  end-of-turn wake; notification entry rendering; `/tasks` output; replacement
  re-reads `Tasks()`.
- `internal/repl`: wake from the select; command output.
- `internal/server`: task routes; wake turn with `trigger: task`; cancel
  endpoint; `notification` wire event; metrics names.
- `cmd/otto`: `agent*` tools present in definitions and in the `Usable tools:`
  line; absent with `enabled = false`; child tool set excludes the listed
  tools; PTY smoke test still passes.

## Follow-ups

- Child transcript persistence (Phase D).
- Child usage in `Info().Usage` and after resume (needs usage on
  `custom_message` or a task record entry type).
- Memory binding for children (`memory_search` and per-turn recall) once the
  SQLite binding is confirmed goroutine-safe.
- A blocking `agent_ask` for children that must wait for a parent reply.
- Definition-level `max_steps`/`max_tokens` budgets.
- Depth > 1 (children starting children).

## Decisions taken

- Asynchronous by default: the `agent` tool returns a task id; `wait: true`
  and `agent_wait` give synchronous behavior when the model needs it.
- Notifications are delivered as `RoleContext` messages with
  `ContextType: "task_notification"` and persisted as Pi `custom_message`
  entries, reusing the existing decoder and the TUI's system-entry rendering.
- Wake turns are started by frontends (TUI, REPL, server), not by the
  Controller, so the existing single-active-turn rule and each frontend's
  input handling stay unchanged.
- The task registry lives in `internal/agent` as plain data (`Tasks`,
  `Inbox`) so that `internal/agent` and `internal/app` never import
  `internal/subagent`.
- Child events are not forwarded to the parent's event stream; the registry
  and `History(id)` give frontends full visibility without mixing two event
  streams in one transcript.
- Context is fresh by default; `inherit` is opt-in per call or per
  definition, with the cost stated in the tool description.
- `max_parallel` is a semaphore with a `queued` state, not a per-message
  group.
- Children get no memory binding in this design.
- `tools` is comma-separated because the frontmatter parser has no list
  support.
- `skill.ParseFrontmatter` is exported instead of adding an
  `internal/frontmatter` package.
