# Development guide

Read [`AGENTS.md`](../AGENTS.md) and the [code organization map](code-organization.md)
first, then use the relevant current contract below.

| Guide | Covers |
| --- | --- |
| [Architecture](development/architecture.md) | Package boundaries, layers inside `otto`, workspace/storage ownership, lifecycle and embedding API |
| [Workflow](development/workflow.md) | Worktrees, local checks, secret scanning, changelog and releases |
| [Testing](development/testing.md) | Test ownership, fault injection and Docker acceptance |
| [Implementation contracts](development/contracts.md) | Safety, approvals, plugins, agent implementation and capability descriptions |

User-facing behavior belongs in the [user manual](user-manual.md), not in design
history. Git preserves removed documentation; retained design notes explain only
rationale that remains useful alongside these contracts.
