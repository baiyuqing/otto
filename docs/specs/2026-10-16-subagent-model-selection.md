# Sub-agent model selection from an ordered profile list

Status: approved 2026-10-16.

## Problem

The `agent` tool accepts a model ID, but recent calls with an empty `model`
inherited the parent session's `gpt-6.1-sol`. With the `chatgpt` provider, Otto
cannot list models, and its system prompt forbids inventing model IDs. Other
configured profiles are not offered to the parent as sub-agent choices. A
general instruction to use cheaper children therefore has no reliable effect.

## Contract

Add an optional `subagent_models` array to a profile. Entries are provider model
IDs in the user's preferred order, from lowest cost/lowest capability to
highest capability. Otto has no price data and does not infer the order.

```toml
[profiles."chatgpt-6.1"]
provider = "chatgpt"
model = "gpt-6.1-sol"
subagent_models = ["gpt-5.4-mini", "gpt-6.1-sol"]
```

For a session on this profile, the parent prompt receives this ordered list and
asks the parent to pick the first model adequate for each task, explicitly
passing that ID in the `agent` call. If `model` is empty, the runtime uses the
first configured candidate. This makes the common case deterministic even
when the parent omits the field. Precedence is: call `model`, named agent's
`model`, first profile candidate, then parent session model. An explicit call
model remains possible even when it is outside the candidate list.

The list applies only when the active profile still uses the session's resolved
provider and connection. A provider, endpoint, or credential-source override
that changes the connection disables the list for that session; existing
inheritance behavior then applies. Each candidate is sent through the same
provider client as the parent. A model rejected by that provider fails the
child with the existing provider error; Otto does not rerun a child that may
already have performed effects. The parent may start a new task on another
model after inspecting the failure.

An absent or empty list preserves current behavior. Reject empty IDs,
duplicates, and an excessive list at configuration resolution, with an error
that names the profile and field. Do not fetch a price catalog or contact a
provider while loading configuration. The recorded task model and existing
status UI continue to show the model actually selected.

## Implementation and verification

- Add the profile field and its resolution/validation in `otto-core`.
- Pass the resolved candidate list into parent system-prompt construction and
  the sub-agent runner. Do not include it in child prompts, because children
  cannot start further children. Update the `agent.model` description to state
  the new default.
- Cover prompt visibility, call/definition/default precedence, no-list
  compatibility, override isolation, invalid configuration, and provider
  rejection with offline tests. The default-selection assertion must fail if
  the implementation falls back to the parent model.
- Document configuration, model selection, failure behavior, and the need to
  restart or start a new session after a profile edit in the user manual.
  Update the changelog if the change meets its entry rules.
- Run `cargo fmt --all` and focused Rust tests, then commit and open a PR under
  the repository workflow. No full `make` gate is planned.

## Scope limit

No automatic price discovery, provider switch, credential transfer, or retry of
a child after a model error. The ordering expresses user preference, not a
verified provider price or a guarantee that a model is available.
