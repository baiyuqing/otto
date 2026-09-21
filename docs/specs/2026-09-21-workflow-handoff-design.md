Status: implemented, 2026-09-21.

# Durable Workflow Handoff

Add the smallest useful handoff primitive to the durable workflow runtime: a
static `kind = "handoff"` step that starts a named receiving agent after its
dependencies succeed.

This is not cross-process agent transfer, dynamic routing, or group chat. It is
a durable workflow boundary that makes the handoff visible in definitions,
status JSON, events, CLI output, API responses, and the Web UI while reusing the
existing child-agent execution path.

## TOML shape

```toml
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "Take over from research and produce the review."
needs = ["research"]
```

Rules:

- `handoff` requires `agent`, like an `agent` step.
- `handoff` requires at least one dependency, because a root handoff has
  nothing to hand off.
- The target agent must exist and is snapshotted with the run, just like agent
  steps.
- Recovery, retry, cancellation, transcripts, concurrency limits, and fail-fast
  behavior are identical to agent steps.

## Execution

The controller schedules `handoff` with the same executor used by `agent`.
The attempt prompt keeps the current workflow input and dependency result
sections, plus a short handoff heading so the receiving agent can distinguish a
transfer from ordinary assigned work.

## Deferred

- Conditional handoff chosen by an agent.
- Cross-process or remote-agent handoff.
- Shared group-chat conversation state.
- Handoff-specific approval or policy language.
