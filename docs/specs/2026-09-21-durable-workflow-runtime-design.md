# Durable workflow runtime design

Status: approved and implemented through slices A-C on 2026-09-21. Branch
`baiyuqing/durable-workflow-design`, worktree
`.worktree/durable-workflow-design`. Slice D remains explicitly out of scope.

## Implementation notes

- Every CLI workflow command takes the workspace lock. While `otto serve`
  owns it, inspect and control runs through its HTTP/Web UI instead of a second
  CLI process.
- Resume requires the current profile/provider/model identity to match the
  stored run. Agent instructions, model overrides, and tool allowlists come
  from the stored snapshot; the current sandbox remains authoritative.
- Predecessor input uses the fixed 64 KiB workflow prompt bound. Overflow
  fails the step instead of truncating it.
- The event endpoint returns the durable suffix as SSE and closes; the Web UI
  polls. Sequence replay survives process restart, but there is no long-held
  workflow event connection.

## Goal

Add a small durable workflow runtime for explicit, repeatable multi-agent work.
The first implementation supports:

- agent steps arranged as a directed acyclic graph (DAG), which covers
  sequential and concurrent execution;
- persisted run, step, attempt, event, and approval state;
- safe process restart from committed step boundaries;
- explicit human approval steps;
- durable event replay and status through the existing application and server
  boundaries.

This is not a replacement for Otto's model-driven `agent` tool. Ad-hoc
sub-agents remain useful when the model should decide what to delegate.
Workflows are for paths whose order, gates, and recovery behavior must be
declared before execution.

## Non-goals

- No additional providers, languages, operating systems, remote workers, or
  hosted control plane.
- No general graph framework, loops, arbitrary conditions, group chat,
  nested workflows, or workflow-as-agent wrapper in the first implementation.
- No automatic retry. Retrying an interrupted step may repeat an external
  effect, so it requires an explicit human action.
- No attempt to resume inside a provider request or tool call.
- No YAML. Definitions use TOML, which Otto already parses and versions.
- No change to Pi v3 parent-session semantics. Workflow durability is stored
  separately; a context-compaction checkpoint is not a workflow checkpoint.
- No prompt, tool arguments, tool output, or response text in logs, metrics,
  or OpenTelemetry by default.

## Current boundaries to preserve

- `otto-core::agent::Agent` remains the provider/tool turn loop.
- `app::Controller` remains the shared frontend use-case boundary and sole
  owner of chat turn admission and session replacement. Workflow lifecycle is
  separate, so replacing a chat session cannot stop a workflow.
- `subagent` remains the owner of child-agent construction and child tool
  restrictions. Workflow execution reuses that construction path rather than
  creating a second kind of child agent.
- Parent sessions stay append-only Pi v3 JSONL. Workflow state never appears as
  reserved or invented Pi entries.
- Existing `agent`, `agent_wait`, and `agent_status` behavior stays compatible;
  ad-hoc task records and child transcripts remain process-local in this
  change.
- Workflow approval never grants an unsandboxed Bash command. The existing
  exact-command, session-bound Bash approval remains a separate security
  boundary.

## Definition format

Definitions are discovered from `~/.otto/workflows` and
`<workspace>/.otto/workflows`, with the workspace definition winning on a name
collision. One `<name>.toml` file defines one workflow:

```toml
version = 1
description = "Research a change, review it, then ask before delivery."

[[steps]]
id = "research"
agent = "researcher"
prompt = "Inspect the requested change and report evidence."

[[steps]]
id = "review"
agent = "reviewer"
prompt = "Review the evidence and identify blocking problems."
needs = ["research"]

[[steps]]
id = "approve"
kind = "approval"
prompt = "Approve delivery of the reviewed change?"
needs = ["review"]

[[steps]]
id = "deliver"
agent = "executor"
prompt = "Deliver the approved change."
needs = ["approve"]
```

Rules:

- `version` must be `1`; unknown fields are rejected.
- The filename is the workflow name. It follows the existing skill-name rule:
  1 to 64 lowercase letters, digits, and single hyphens.
- A definition has 1 to 32 steps. Step IDs follow the same rule and are unique.
- `kind` is `agent` by default; `approval` is the only other first-version
  value.
- An agent step requires `agent` and `prompt`. The agent name must resolve from
  the existing `AGENT.md` catalog. A workflow cannot widen that agent's tools.
- An approval step rejects `agent` and requires `prompt`.
- `needs` defaults to empty. Every dependency must exist, cannot name the step
  itself, and the complete graph must be acyclic.
- The file is limited to 1 MiB before parsing. Description is limited to 1024
  characters; every step prompt and run input is limited to 64 KiB of UTF-8.
- All validation completes before the run is written. Invalid input leaves no
  partial run.

The root agent steps receive the run input. Other agent steps receive the run
input plus successful direct-predecessor results under fixed, clearly delimited
headings. There is no string interpolation or template language. The combined
predecessor block is capped by the existing agent output cap; overflow is a
step error rather than silent truncation.

The parsed definition, its SHA-256 hash, and the parsed definitions of every
referenced agent are stored with the run. Editing a workflow or `AGENT.md`
affects new runs only. A resumed run keeps its stored prompts, model choices,
and tool allowlists; current sandbox policy still applies, and a required tool
that no longer exists fails closed.

## Runtime model

### Identities

- A run has a globally unique opaque `run_id`.
- A step is identified by `(run_id, step_id)`.
- Every execution of an agent step has an integer `attempt`, starting at 1.
- A human request has a globally unique opaque `request_id`.

IDs use Otto's existing native convention: 16 bytes from `/dev/urandom`, lower
hex encoded. Display names never serve as database identity.

### Run state

```text
running ──► waiting ──► running
    │           │
    ├───────────┼──────► succeeded
    ├───────────┼──────► failed
    ├───────────┼──────► canceled
    └───────────┴──────► paused
```

- `waiting` means one or more approval steps are awaiting a response.
- A run stays `running` while any agent step is ready or active, even if an
  independent branch is awaiting approval. It becomes `waiting` only when no
  agent work can advance without a human response.
- `paused` means recovery found an interrupted attempt, or the user explicitly
  paused the run in a later extension.
- Terminal states never change.

### Step state

```text
pending ──► ready ──► running ──► succeeded
                         ├───────► failed
                         ├───────► canceled
                         └───────► interrupted

pending ──► waiting ──► succeeded   (approval)
                  └───► canceled    (rejected or run canceled)
```

A step becomes `ready` only after every dependency is durably `succeeded`.
All ready agent steps may run concurrently, bounded by the existing
`[agents].max_parallel` setting. The workflow controller owns the semaphore;
there is no second concurrency option. This provides sequential and concurrent
patterns without a general graph scheduler.

The first version is fail-fast. A failed step makes the run `failed`, requests
cancellation of running siblings, and marks unscheduled dependents canceled.
The registry does not claim cancellation succeeded until each active attempt
has actually stopped.

### Commit and crash semantics

Every state transition and its event are written in one SQLite transaction.
The scheduler commits `running` before spawning an agent attempt and commits a
terminal result before making dependents ready.

On recovery:

- `succeeded`, `failed`, and `canceled` steps stay terminal;
- `pending` and `ready` steps have not started and may be scheduled safely;
- `waiting` approval steps re-expose the same pending request;
- every `running` step becomes `interrupted`, and the run becomes `paused`;
- no interrupted attempt is retried automatically.

This is at-most-once automatic execution, not exactly-once external effects. A
tool may have completed an external effect before the process died and before
the terminal transaction committed. The runtime cannot prove otherwise.
`resume --retry <step>` creates a new numbered attempt only after the user
accepts that risk. Future tools may supply idempotency keys, but the workflow
runtime does not invent them.

### Agent attempts

Each attempt uses the existing provider, sandbox, redactor, static prompt,
agent definition, and restricted child tool set. A small shared child-execution
helper is extracted from `subagent::runner`; it is the only new common seam and
has two real consumers: ad-hoc sub-agents and workflow attempts.

Workflow attempts use a file-backed Pi v3 session instead of `MemorySession`.
Attempt transcripts live under `~/.otto/workflow-sessions`, scoped by
workspace, run, step, and attempt. They are sensitive, use directory mode
`0700` and file mode `0600`, and are never rewritten. A retry gets a new file;
the interrupted transcript remains available for diagnosis.

The scheduler records only the final assistant text as the step result.
Success without final assistant text is a step failure. Downstream steps never
receive an interrupted or partial transcript.

### Human approval

An approval step does not start an agent. When its dependencies succeed, the
scheduler stores one pending request and moves the run to `waiting`.

- Approve: the request and step become succeeded in one transaction; the fixed
  result `approved` is available to dependents.
- Reject: the request is rejected, the step and run become canceled, and no
  dependent is scheduled.
- Repeating the same response is an idempotent success. A different response
  for an answered request is a conflict.
- Restart reuses the same request ID and prompt.

This gate is intentionally boolean. Free-form human input and agent-initiated
requests can be added after the durable gate is proven.

## Persistence

One local SQLite database, `~/.otto/workflows.db`, is opened at the composition
root with the same permissions, busy timeout, WAL mode, and fixed-text error
boundary as the usage store. It contains these responsibilities:

- `workflow_runs`: immutable definition snapshot and run-level state;
- `workflow_steps`: materialized current step state;
- `workflow_attempts`: immutable attempt identity and transcript path plus its
  terminal result/error;
- `workflow_requests`: durable approval request and response;
- `workflow_events`: append-only, content-minimized state transitions.

The schema is `STRICT`, uses foreign keys and status `CHECK` constraints, and
is initialized transactionally. Store methods expose domain operations such as
`create_run`, `claim_ready`, `finish_attempt`, `request_approval`, and
`respond`; callers cannot issue arbitrary updates.

Event rows contain IDs, statuses, timestamps, durations, provider/model names,
and error categories. They do not contain run input, prompts, predecessor
results, tool arguments, tool results, or assistant text. Those values remain
in the protected run/attempt records and transcripts.

The store is the source of truth. In-memory maps hold only active cancellation
tokens and notification channels and can be rebuilt or discarded without
changing durable state.

Only one process may schedule or mutate workflows for one workspace at a time.
The workflow controller holds a nonblocking exclusive advisory lock on
`~/.otto/workflow-locks/<workspace-key>.lock`; a competing controller reports
`workflow runtime is already active for this workspace`. The kernel relies on
the OS releasing that lock at process exit, not on a time-based lease.
Separate workspaces can run concurrently. This is sufficient for Otto's
single-host scope and avoids a distributed lease protocol.

## Application and frontend boundaries

`crates/otto::workflow` owns definitions, persistence, scheduling, and recovery.
It is native-only: durable state, process lifecycle, and child session files do
not belong in wasm-safe `otto-core`.

The application layer owns one workspace-scoped `WorkflowController`, beside
the chat-session controllers. It exposes a narrow facade for
list/get/start/resume, approve/reject, cancel, event observation, and attempt
history. Frontends never receive the store or scheduler. A run stores its
workspace and the same provider/profile/model runtime metadata that a session
does; resume goes through the existing runtime resolution path. The stored
definition remains fixed.

The first command/API surface is deliberately small:

```text
otto workflow run <name> [--input TEXT]
otto workflow status <run-id>
otto workflow resume <run-id> [--retry <step-id>]
otto workflow approve <request-id>
otto workflow reject <request-id>
otto workflow cancel <run-id>
```

Server routes mirror those use cases under `/v1/workflows`; they do not require
an open chat session. The existing Web UI initially needs only a Workflows tab,
run list, step timeline, approval buttons, retry, and cancel. It reuses
SSE/event polling; workflow event sequence numbers come from the durable event
table, so `after=N` replay works across process restarts. No separate DevUI
process is introduced. TUI commands and graph rendering are follow-ups.

Opening the store rehydrates nonterminal runs for the current workspace but
starts no work. `resume` is explicit. Process shutdown cancels active attempt
tokens, waits for them to stop, and persists them as `interrupted` with the run
`paused`; explicit workflow cancellation is the only path to terminal
`canceled`. Chat session replacement has no workflow effect.

## Observability

Phase one extends existing Prometheus metrics and structured events:

- runs started/finished by status;
- active runs and steps;
- step and approval wait duration;
- recovery and manual retry counts.

Labels remain bounded (`workflow`, `step`, provider/model, status); `run_id`,
prompts, results, and arbitrary error text are not metric labels.

OpenTelemetry is a second delivery slice after durable IDs and state transitions
are stable. It adds `workflow.run` and `workflow.step` spans, linked provider
and tool spans, W3C context propagation to MCP, and sensitive content disabled
by default. The event log remains the recovery source; telemetry loss never
changes execution.

## Time travel

Time travel is not in the first implementation. The append-only event and
attempt records leave a direct extension path: a later command selects a
committed event boundary and creates a new run whose succeeded-step results
reference immutable prior attempts. It never rewinds or mutates the original
run. No time-travel API is added until restart and approval recovery pass their
fault-injection tests.

## Implementation slices

### A. Durable kernel

- Definition parser and DAG validation.
- SQLite store, state machines, transactional events, and recovery.
- Focused tests inject failure before and after every transition transaction.
- No agent execution or frontend changes yet.

### B. Agent execution and CLI

- Extract the shared child construction/execution helper.
- Run ready DAG steps with the existing concurrency cap and file-backed attempt
  transcripts.
- Add run/status/resume/cancel CLI commands.
- Prove sequential, fan-out/fan-in, failure, cancellation, and restart behavior.

### C. Approval and server UI

- Durable approval steps and approve/reject commands.
- Application facade, HTTP/SSE routes, OpenAPI updates, and the minimal Web run
  inspector.
- Restart exposes paused/waiting runs without starting them.

### D. Operations

- OpenTelemetry tracing and MCP propagation.
- Time-travel fork from a committed boundary.
- Add handoff only when a concrete workflow needs conditional routing; add
  group collaboration only when shared-conversation semantics are specified.

Every behavioral slice follows RED, focused GREEN, then `make check-fast` and
the relevant wider `make check` gates. The root `AGENTS.md` task map,
development guide, README, user manual, and OpenAPI document change only when
the corresponding behavior is implemented and tested.

## Acceptance

The first three slices are complete only when automated tests prove:

1. Invalid or cyclic definitions write nothing.
2. Root steps run concurrently within the configured bound; dependents start
   only after every required result commits.
3. A failure cancels siblings and never schedules downstream work.
4. A crash after `running` commits but before terminal commit reopens as
   `interrupted`/`paused` and performs no automatic retry.
5. A crash after terminal commit but before the next scheduling pass starts
   each newly ready step exactly once after resume.
6. A pending approval survives restart with the same request ID; duplicate
   identical responses are harmless and conflicting responses fail closed.
7. Manual retry creates a new attempt and preserves the interrupted transcript.
8. Cancellation waits for active attempts to stop before reporting a terminal
   run.
9. Chat session replacement does not affect a run; process shutdown stops each
   active attempt exactly once and durable history remains readable.
10. A second process cannot schedule a run for the same workspace, while a
    different workspace remains independent; a restarted process can replay
    durable events after the last sequence number a client saw.
11. Logs, metrics, errors, and default telemetry contain no run input, prompt,
    tool argument, tool result, response text, API key, OAuth token, or auth
    header.

## Approved contracts

1. TOML DAG definitions; no YAML, loops, or conditions initially.
2. Interrupted steps pause and require explicit retry; no automatic retry.
3. Boolean approval gates only; no free-form or agent-initiated HITL initially.
4. Durable workflows are separate from ad-hoc `agent` tasks and reuse only the
   child execution path.

## Verification

Every `make check` component passed on the final tree on 2026-09-21:
`make check-fast`, the release build, `make rust-test`, the wasm32 checks and
Node wasm tests, PTY lifecycle tests, all Web UI tests, and `git diff --check`.
One monolithic rerun was interrupted after the unchanged Bash concurrency test
waited for more than three minutes; that test passed immediately in isolation,
and the complete 1,341-test native suite then passed on rerun. Focused workflow
tests retain the RED/GREEN regressions for DAG validation, transactional
dependency advancement, approval idempotence, workspace-scoped recovery,
explicit retry attempts, sibling cancellation, graceful shutdown, durable
event replay, workspace locking, metrics, and the Web workflow inspector.
