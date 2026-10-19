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
