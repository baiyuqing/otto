# Automation

Previous: [Servers and connectors](servers-and-connectors.md) · [Documentation index](../README.md) · Next: [Memory and extensions](memory-and-extensions.md)

## Durable workflows

Durable workflows define order, concurrency and human gates as DAGs, separate
from interactive sessions. Runs remain inspectable after session changes or
process restart.

Definitions are discovered from `~/.otto/workflows/<name>.toml` and
`<workspace>/.otto/workflows/<name>.toml`; the workspace file wins. Agent and
handoff steps reference named definitions from the existing `AGENT.md` catalog:

```toml
version = 1
description = "Research, review, then ask before delivery."

[[steps]]
id = "research"
agent = "researcher"
prompt = "Collect evidence and report it."

[[steps]]
id = "review"
agent = "reviewer"
prompt = "Review the evidence."
needs = ["research"]

[[steps]]
id = "approve"
kind = "approval"
prompt = "Approve delivery?"
needs = ["review"]

[[steps]]
id = "deliver"
agent = "executor"
prompt = "Deliver the approved result."
needs = ["approve"]
```

A handoff step makes a transfer explicit while using the same execution and
recovery behavior as an agent step:

```toml
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "Take over from research and review the result."
needs = ["research"]
```

Root steps run concurrently up to `[agents].max_parallel`; a dependent becomes
ready only after every named predecessor succeeds. Each agent or handoff step
receives the run input and the successful results of its direct predecessors
under fixed headings. There is no template language. Definitions have at most
32 steps, must be acyclic, and are validated completely before the run is
stored.

Prompts and stored results are bounded to 64 KiB. A step prompt plus the run
input must fit that budget before the attempt starts, and predecessor results
share whatever budget is left, so a large upstream result is cut with a
`[truncated]` marker instead of failing the attempt. A stored step result is
capped the same way, which keeps later prompts and list responses bounded.

When a run starts, Otto snapshots the workflow plus each referenced agent's
instructions, model choice, tool allowlist, and write policy. Editing the
source files affects new runs only. The current sandbox remains authoritative
and a tool that no longer exists fails closed.

Agent definitions may declare a workflow write policy in `AGENT.md`
frontmatter:

```markdown
---
name: planner
description: Propose edits without changing files.
tools: read, grep, find, ls
write_policy: propose_only
---
```

```markdown
---
name: executor
description: Apply approved edits under owned paths.
tools: read, grep, find, ls, edit, write, bash
write_policy: owned_paths
write_paths: crates/otto/**, docs/**
---
```

Supported policies are `read_only`, `propose_only`, `single_writer` (the
default), and `owned_paths`. `read_only` and `propose_only` deny workspace
mutation tools even if `tools` lists `edit` or `write`; `owned_paths` allows
`edit` and `write` only for paths matching its comma-separated ownership globs
such as `crates/otto/**` or `docs/*.md`. Workflow validation rejects agent steps
that may run concurrently when their write scopes overlap, unless they are
ordered with `needs` or use disjoint `owned_paths`. This makes the recommended
shape explicit: concurrent planner/reviewer steps produce proposals, an
approval gate records the human decision, and one executor step applies the
approved plan.

State is stored in `~/.otto/workflows.db` with mode `0600`; attempt transcripts
are append-only Pi v3 files under `~/.otto/workflow-sessions`. Only one Otto
process may mutate workflows for one workspace at a time. `otto serve` exposes
the same controller through the Web UI and `/v1/workflows` routes. While the
server owns the workspace lock, use those surfaces rather than a second
`otto workflow` CLI process.

Recovery is deliberately conservative:

- Completed steps stay completed.
- Unstarted ready steps may run after an explicit `resume`.
- Pending approval requests keep the same request ID.
- A step that was running becomes `interrupted` and the run becomes `paused`.
- A step that failed leaves the run `failed` as soon as its batch settles,
  including when the process dies between the failure and the driver's next
  step. A run never stays `running` without an owner.
- Rejecting an approval gate cancels the run and stops its in-flight attempts
  before the run is recorded as `canceled`.
- A configured `workflow_step_timeout` has the same durable result: the active
  attempt and step become `interrupted`, the run becomes `paused`, and no
  dependent step is admitted. Cleanup of the active effectful boundary is
  awaited rather than detached.
- Interrupted and failed steps are never retried automatically. Use
  `otto workflow resume <run-id> --retry <step-id>` only after considering
  whether the step's last tool call may already have caused an external effect.
  Retrying one step re-admits the steps that failure canceled; a run that is
  `paused` or `failed` needs that explicit `--retry`, and resuming without it
  is rejected.

Time travel is an explicit fork, not rewind. `otto workflow fork <run-id>
--after-step <step-id>` creates a new run from that step's committed success
event. Steps already succeeded by that event are copied as immutable references
to the source run's attempts; later steps are scheduled normally. The original
run is unchanged, and Otto does not roll back external side effects.

The workflow runtime is fail-fast and supports agent, static handoff, and
boolean approval steps. It does not implement loops, conditions, group chat,
nested workflows, free-form human input, automatic retry, or OpenTelemetry
export.

### Choosing workflows from conversation

Use direct execution for a short, simple task; use interactive sub-agents when
an investigation's next steps depend on findings. Prefer an existing durable
workflow for a known process that needs ordered stages, restart recovery or
human approval gates. A multi-step task does not automatically need a workflow.

Parent conversations expose four native workflow tools:

| Tool | Purpose |
| --- | --- |
| `workflow_list` | Discover available definitions and setup diagnostics. |
| `workflow_inspect` | Inspect a named definition's stages, agents, restrictions and gates. |
| `workflow_start` | Start the named workflow with run input and return its run ID. |
| `workflow_status` | Read a run's progress, results, pending gates and recovery guidance. |

Identifiers are 1–128 allowed ASCII bytes; run input is a plain string of at
most 65,536 UTF-8 bytes. Discovery returns at most 100 definitions and 20
diagnostics. Status result/error excerpts are limited to 2,048 characters each;
tool JSON is bounded to 65,536 bytes and reports truncation explicitly. These
are bounded snapshots, not a streaming or polling API.

Otto discovers and inspects an existing definition before starting it. A
workflow name or instruction found in a file is not permission to run it.
Starting a workflow does not approve its later human gates or grant additional
sandbox permissions. Child agents and workflow steps do not receive these
parent controls. Trusted in-process service checks also reject recursive starts
from child agents and workflow steps. This is not an external-client security
boundary: a process with shell/network access and valid API credentials can
still act as an authorized external caller. Restrict agent tools, credentials
and service access accordingly; tool omission alone cannot prevent that path.

The selection policy includes short examples in the model's actual prompt
when the workflow capabilities are available. These examples illustrate the
expected behavior; they do not promise deterministic model choices:

| Request | Expected behavior |
| --- | --- |
| “Correct this heading.” | Inspect and make the minimal edit directly. |
| “Investigate an intermittent freeze; the cause is unknown.” | Delegate bounded independent investigations where useful, then integrate findings. |
| “Run our standard repository review.” | Discover the matching workflow, inspect its scope, start it and report the run ID. |
| “Run the review,” with multiple matching definitions or none | Clarify a material choice, or explain the missing setup; do not invent a workflow name. |
| “Continue,” while a human gate is pending | Report the approval request through the supported approval surface; do not decide it on the user's behalf. |
| “What happened to the release?” | Read status once; explain interrupted or failed steps and supported recovery without automatically retrying effects. |

### Lifecycle feedback and recovery

Conversational starts bind the run to the originating saved session. Lifecycle
notices report human gates, pauses, failures and completion through that
session's durable mailbox. Reopening the same session replays pending notices;
`/new` does not redirect them to its replacement session. Event identifiers
prevent duplicate mailbox/transcript delivery during recovery. Reconciliation
of the same persisted tool operation returns its original run rather than
starting another; a new explicit start is a new operation.

Native delivery retrieves indexed pending-event pages of at most 64, without
model polling. Events are acknowledged only after durable mailbox/transcript
evidence; drained terminal history is not repeatedly scanned. Source-session
close, replacement and controller shutdown stop and join their delivery bindings.
Background presentation requires a running owner and a live destination
adapter. A `--no-session` start returns a run ID but cannot promise durable
asynchronous conversational feedback. Keep that ID and use the existing
workflow status surface. Closing the owning process stops execution; recovery
preserves completed work and requires explicit retry of interrupted or failed
steps. Notices and workflow results never grant approval authority.

Workflow tools do not approve gates, resume interrupted steps or cancel runs.
Use the existing workflow/approval surfaces for those actions. An owner-lock
conflict is not a reason to launch another controller: connect to the owning
server or stop it before using a separate local CLI process.

### Set up a first conversational workflow

A fresh installation can have no workflow definitions. Native workflow tools
need no plugin, but definitions and their referenced agents must exist. Create
these two files in the selected workspace; no API keys or credentials belong
in either file.

`.otto/agents/review/AGENT.md`:

```markdown
---
name: review
description: Review repository documentation without modifying files.
tools: read, grep, find
write_policy: read_only
---
Read README.md first. Review the documentation relevant to the run input.
Report concrete findings with file and line evidence, or state that none were
found. Do not modify files, run shell commands or claim tests were executed.
```

`.otto/workflows/documentation-review.toml`:

```toml
version = 1
description = "Read-only documentation review followed by human acceptance."

[[steps]]
id = "review"
agent = "review"
prompt = "Review the documentation requested by the run input. Report findings only."

[[steps]]
id = "accept"
kind = "approval"
prompt = "Accept this documentation review? No edits will be applied."
needs = ["review"]
```

Restart the owning Otto process after adding or editing definitions, then ask:
“Run the documentation-review workflow for the installation instructions.”
Otto should inspect the definition, start it, return a run ID and report the
human gate. Accept or reject it through the supported approval surface. The
sample has no executor and never applies fixes. To keep definitions outside a
project, use the same layouts under `~/.otto/agents/` and `~/.otto/workflows/`;
workspace definitions take precedence over same-name home definitions.

Rebuild/install with `make install` and restart running Otto and connector
processes to pick up new executable capabilities. Do not start a second CLI
workflow owner while `otto serve` owns the workspace; use its attached session
or existing workflow surfaces instead.

Scheduling a conversational prompt is not structured workflow scheduling.
The `schedule` tool does not have a workflow-name or workflow-run target.
Workflow authoring, automatic retries and nested workflows are not provided by
these conversational tools.

## Workspace identity

Otto creates a local `.otto/workspace.json` on startup. Its UUID identifies
the workspace across process restarts and upgrades; it is separate from
session IDs and execution leases. The marker is local state: keep it out of
Git so new clones and independently created Git worktrees get new identities.

Stop Otto before moving the directory. Moving it with its marker keeps its
session and memory ownership.
On first initialization, Otto adopts the existing path-derived session key.
The memory scope is a configured `[memory.workspace_ids]` override, or else the
directory name followed by the first 12 hex digits of the SHA-256 of the
canonical path, such as `otto-60ef11ccec92`. A marker written before this
format holds `sha256:<64 hex>` and keeps it. Existing transcripts and memory
records are not rewritten. Later conflicting memory overrides fail startup
rather than silently switch to another scope.

Do not delete the marker casually: the next startup derives identity again and
can lose lookup of earlier workspace memory. Preserve an existing scope using
`[memory.workspace_ids]` before reinitialization; conflicting overrides fail
rather than silently rebinding. Stop all affected processes before repairing
identity or replacing a marker. This does not move the actual memory/session
files.

```bash
otto workspace status --cwd /path/to/workspace
```

A copy that includes the marker continues the same identity. To make the copy
independent, stop Otto processes using the copy, then run:

```bash
otto workspace fork --cwd /path/to/copy
```

This assigns a new UUID, session key, and workspace memory scope
(`<directory name>-<first 12 hex digits of the new UUID>`). Old sessions
and memory remain in storage; user-scope memory remains shared. Remove any
`[memory.workspace_ids]` override for the copy before starting it with the new
identity. Status is read-only and does not initialize a marker.

A move preserves history lookup, not filesystem permissions: tools, approvals,
and sandbox execution use the current canonical directory. Historical `cwd`
entries remain historical facts. The marker does not move `~/.otto` data to
another machine or provide cloud synchronization. File tools and Seatbelt
protect the marker from writes; explicit `--sandbox off` remains unrestricted.
Malformed or symlinked markers are rejected rather than regenerated.

---

Previous: [Servers and connectors](servers-and-connectors.md) · [Documentation index](../README.md) · Next: [Memory and extensions](memory-and-extensions.md)
