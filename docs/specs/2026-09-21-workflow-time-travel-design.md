Status: implemented, 2026-09-21.

# Durable Workflow Time Travel

Add time travel as an explicit fork from a committed workflow event boundary.
It creates a new run and never mutates, rewinds, or deletes the original run.

The first implementation is a correct fork, not a general replay engine. It
reuses the source run's workflow snapshot, runtime identity, workspace, and
input. It does not pretend to roll back external effects.

## User shape

```text
otto workflow fork <run-id> --after-step <step-id>
```

Server:

```text
POST /v1/workflows/{run-id}/fork
{"after_step":"step-id"}
```

`--after-step` selects that step's committed success event:

- agent and handoff steps use `step_succeeded`;
- approval steps use `approval_approved`;
- failed, canceled, waiting, pending, ready, running, and interrupted steps are
  not valid fork boundaries.

The fork keeps the original input. A fork with changed input would make copied
checkpoints lie about what produced them; use `workflow run` for a new input.

## Stored lineage

`workflow_runs` stores:

- `forked_from_run_id`
- `forked_from_event_seq`
- `forked_from_step_id`

`workflow_steps` stores source metadata for copied steps:

- `source_run_id`
- `source_step_id`
- `source_attempt`

These fields are exposed through CLI JSON, HTTP JSON, OpenAPI, and the Web UI.
Existing databases are migrated in place by adding nullable columns.

## Fork semantics

The store resolves the boundary event inside one transaction, then reconstructs
the source run state at that event sequence:

1. Find the success event for `after_step`.
2. Copy every step whose success event sequence is at or before the boundary.
3. For copied steps, preserve `status = succeeded`, `result`,
   `transcript_path`, `attempt`, and source metadata.
4. For uncopied steps, recompute `pending` versus `ready` from copied
   dependencies.
5. Create fresh approval requests only when forked execution reaches an
   approval step again.
6. If every step is copied, create a terminal `succeeded` fork.

This is event-boundary time travel. In a concurrent DAG, any sibling step that
had already succeeded before the boundary is copied too, even if it is not a
dependency of `after_step`.

## Execution

The controller starts a fork only when the new run is `running`; otherwise it
returns the terminal fork. Future attempts use normal scheduling, recovery,
retry, cancellation, and transcript storage.

Copied attempts are references to immutable prior attempts. They are not
re-executed and they do not copy old approval requests.

## Deferred

- Forking by raw event sequence in the CLI.
- Editing input or definition while keeping prior checkpoints.
- Replaying provider/tool calls.
- Reverting external side effects.
- Branch visualizations beyond simple lineage fields.
