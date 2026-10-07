# AUTO mode authority boundaries

Approved scope: 2026-10-07. Extends
[session automatic approval](2026-10-05-session-auto-mode.md), which assumed
the session policy may decide every session-owned request.

## Problem

A session's AUTO policy decided every request owned by that session. Three of
those are not the policy's to decide:

- a model-originated `confirmation` — the model asks whether the user accepts a
  plan or approach, and AUTO let the asking model approve its own proposal;
- an authorization that leaves the sandbox (an unsandboxed command) or that
  reaches an external system (an MCP call);
- an authorization that establishes durable authority (a persistent sandbox
  read grant, a scheduled unattended run).

The unified-approval contract already states that a model invocation cannot
supply its own authority and that a `confirmation` is a human judgement. AUTO
must not become a way around that contract, and the session mode must not
widen the sandbox boundary on its own.

## Contract

A request carries the authority class its producer declares. Only
`Authority::Bounded` may be decided by the session AUTO policy; every other
class waits for a user decision in either mode.

| `authority` | Meaning | AUTO may decide |
| --- | --- | --- |
| `consent` | A judgement about a proposal, plan or workflow step. | no |
| `boundary` | This operation leaves the sandbox or the workspace, or acts on an external system. | no |
| `durable` | This operation establishes or extends durable authority (persistent grants, scheduled unattended runs, memory, skills, profile changes). | no |
| `bounded` | Inside the sandbox and the workspace, granting nothing durable. | yes |
| `unclassified` | No declared class; only a request persisted before this field existed decodes this way. | no |

`Request::auto_decidable()` is the single policy predicate:

```text
authority == bounded && kind == authorization && persistent == false
```

The conjunction is deliberate: a wrong or hostile declaration must not make a
confirmation or a persistent grant decidable by policy, because those are the
two classes whose whole purpose is a human answer.

Enforcement points, all of which must agree:

1. `otto::approval::Service::auto_approve` decides nothing else, so `create`,
   `wait` and the `set_auto_mode(true)` sweep of pending requests all follow the
   same rule.
2. The commit fence rechecks `Request::auto_decidable()` together with the
   persisted mode inside the committing transaction, so a stale automatic
   decision cannot publish even if it was computed before the request changed.
3. No other component may approve automatically; the recorded decision
   transport stays `auto-mode`, and a request that is not decidable stays
   `pending` with no decision event until a user decides it.

Producer declarations (native code, never model arguments):

| Producer (`source`) | Class |
| --- | --- |
| `conversation` (`request_approval`) | `consent` |
| `sandbox`, unsandboxed command | `boundary` |
| `sandbox`, persistent read grant | `durable` |
| `mcp` | `boundary` |
| `plugin` | `bounded` |
| `schedule` (`prompt` item) | `durable` |
| `delivery` | `boundary` |
| `memory` | `durable` |
| `reflection_skill` | `durable` |
| `workflow` step gate | `consent` |
| `config` profile change | `durable` |

## Compatibility

`authority` is an additive persisted-envelope field: it is added to the codec's
known-field list, it is serialized with the request, and a row written before
it existed decodes as `unclassified` and therefore stays a user decision. The
wire `Request` schema stays strict (`deny_unknown_fields`) and its
`schema_version` stays `1`.

A session's stored mode keeps its meaning: AUTO selects automatic decisions for
bounded requests, MANUAL selects none. Nothing about the mode API
(`GET|PUT /v1/sessions/{id}/approval-mode`, `_otto/approval_mode/get|set`,
`/auto on|off`) changes; a request that AUTO cannot decide is presented to the
user exactly as it is in MANUAL, and its arrival is still recorded as a
`pending` event so chat notifications cannot miss it.

## Consequences

- A plan review, a sandbox escalation, a persistent grant, an MCP call and a
  scheduled unattended run always reach the user, whatever the session mode.
- AUTO keeps its meaning where it is safe: an authorization that stays inside
  the sandbox and the workspace is still decided without a dialog.
- A tool call waiting for a decision AUTO cannot make behaves exactly like a
  MANUAL request: it blocks and reports `not started`, and the recorded
  `application` outcome stays separate from the decision.

## Follow-ups

- `approvals_reviewer = auto_review`: a separate reviewer agent for
  boundary-crossing requests, with a deny policy for credential exposure,
  persistent weakening and irreversible damage, and a denial circuit breaker.
  This is how Codex keeps unattended flow without widening the boundary.
- MCP: honor `readOnlyHint`/`destructiveHint` annotations so a read-only call
  can be `bounded`; today Otto cannot tell them apart, so every MCP call is a
  user decision.
