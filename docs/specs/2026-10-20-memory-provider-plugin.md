# A plugin as the memory source

Status: approved 2026-10-20.

## Problem

`[memory] enabled = false` disables only the built-in file memory: it is the
condition at `crates/otto/src/runtime/startup/memory_start.rs:18`. The otto-mem
plugin built on [the hooks spec](2026-10-20-memory-plugin-hooks.md) is a
`before_prompt` hook, a `turn_end` hook and three package tools, and these run
whatever `[memory]` says. With the plugin installed and built-in memory on:

- Two recall blocks reach a request: the built-in request-local memory message
  and the hook's `<plugin_context>` in the system prompt.
- `/remember` and the `memory_search`, `remember` and `forget` tools read and
  write the built-in store; the model also sees `plugin__otto-mem__search`,
  `plugin__otto-mem__remember` and `plugin__otto-mem__forget`.
- Otto has no record that the package provides memory.

## Decisions

- 2026-10-20 (user): one memory source is active at a time; a plugin source
  replaces the built-in memory.
- 2026-10-20 (user): `/remember`, the built-in memory tools and the review flow
  use the active source.
- 2026-10-20 (user): `plugin.json` declares the memory role; Otto does not
  match package names.
- 2026-10-20 (user): running built-in and plugin memory together is not a real
  need. `[memory] enabled = false` turns off all memory, and `[memory]` selects
  the one source.

This replaces the decision in the hooks spec that the plugin "runs beside the
built-in memory". Change 1 (`turn_end` turn content) and change 2 (package
credential bindings) of that spec stay; they are general hook features.

## Configuration

```toml
[memory]
backend = "plugin:otto-mem"
```

- `backend` is `files` (default; `turso` and `sqlite` stay aliases) or
  `plugin:<package>`. Any other value is a configuration error.
- `enabled = false`: no memory source. Otto opens no file store and never runs
  the package's memory command; memory tools are absent; `/remember` reports
  that memory is disabled. The package's other hooks and tools still run.
- `required`, `recall_tokens`, `max_results`, `require_encryption` and
  `workspace_ids` apply to a plugin source. `[memory.turso]` only locates the
  import database of the file store and is ignored.
- Project layers cannot set `[memory]` (existing rule,
  `crates/otto-core/src/config/layered.rs:330-363`), so only user and managed
  configuration select a plugin source.
- The `backend = "remote"` error names `backend = "plugin:otto-mem"`.
- A host-injected `MemoryExtension` (embed API) together with
  `backend = "plugin:..."` is a startup error.

Installing or trusting a package never changes the memory source; only
`backend` does.

## Package declaration

```json
{
  "extensions": {
    "dev.otto": {
      "version": 1,
      "memory": {"command": ["${PLUGIN_ROOT}/bin/otto-mem-plugin", "memory"], "timeout": "10s"}
    }
  }
}
```

- One `memory` object per package, with `command` and `timeout` as for a hook
  (default 10 s, at most 300 s). It is validated with the other components
  (`crates/otto/src/plugin/hooks.rs:93-122`) and counts as an executable
  component for trust.
- `otto plugin trust` lists the memory command in its disclosure
  (`crates/otto/src/cli/plugin.rs:93-110`).

## Command protocol

Each memory operation runs the command once, through the runner that package
hooks and tools use (`crates/otto/src/plugin/hooks.rs:254-381`): the same
sandbox, the workspace as working directory, the package's bindings, redaction
of bound values, a 1 MiB stdin and 64 KiB stdout limit, and a timeout of the
declared value or the remaining operation time, whichever is shorter.

Input, one JSON line:

```json
{"version": 1, "operation": "recall", "request": {"context": {}, "query": "build", "limit": 12, "token_budget": 2000}}
```

Output, one JSON object:

```json
{"result": {"items": [], "used_tokens": 0, "coverage": "complete"}}
```

or `{"error": {"kind": "unavailable"}}`.

- `operation` is `describe` or an `otto_core::memory::Operation` name
  (`recall`, `inspect`, `remember`, `forget`, `observe`, and the others the
  description declares).
- `request` and `result` are the serde JSON forms of the request and result
  types in `crates/otto-core/src/memory/memory_types.rs`: `RecallRequest` to
  `RecallResult`, `RememberRequest` to `Receipt`, and so on. `describe` has no
  request and returns a `ServiceDescription`. These forms become a plugin wire
  contract: later changes to the types keep the JSON compatible, or `version`
  changes.
- `error.kind` is an `ErrorKind` name. Otto keeps the kind and its fixed
  message; text from the plugin is not shown.
- A non-zero exit, invalid JSON or output over the limit is `unavailable`. A
  timeout or cancellation after the process started is `outcome_unknown` for an
  effectful operation and `unavailable` for `describe`, `recall` and `inspect`.
- Results pass the checks Otto applies to every `MemoryService` today
  (`crates/otto/src/memory/client.rs`): scope, receipt and content bounds.

## Lifecycle

- Otto builds the plugin source for each runner, where the runner's plugin
  runtime exists (`crates/otto/src/runtime/builder/runner_assembly.rs:148-150`),
  and runs `describe` once there. Memory tools and the recall binding are built
  after it.
- The source follows the package's hooks: enable, disable and trust take
  effect at the next runner build (new session, resume, `/sandbox reload`);
  revoke and remove take effect at the next call, because each call checks the
  package digest (`crates/otto/src/plugin/mod.rs:186-249`).
- When the package is missing, disabled, not trusted, has no `memory`
  component, `describe` fails, or `require_encryption` is set and
  `encryption_at_rest` is not `true`: with `required = true` the runner build
  fails with that reason; with `required = false` (default) Otto writes a
  warning and the runner has no memory.

## What uses the plugin source

| Consumer | With `backend = "plugin:<package>"` |
| --- | --- |
| Recall before each request (`runtime/wiring.rs:185-262`) | `recall`; rendered as the request-local memory message within `recall_tokens`; a failure is a warning and the turn continues |
| `memory_search` tool | `recall`, or `inspect` for an id |
| `remember` tool | `remember`, when the description declares delegated-learning support (`tool/memory_service.rs:25-44`) |
| `forget` tool | `forget`, when the description also declares item targets and suppression |
| `/remember` | `remember` with origin `human` (`app/management.rs:328-426`) |
| Turn content | `observe` after `turn_end`; see [turn observation](#turn-observation) |
| File memory model tools (`tool/memory.rs`) | Not registered |
| `/memory list\|show\|search\|forget\|review`, TUI review panel, ACP `_otto/memory/pending\|review`, `otto memory`, reflection, advice, session memory notices | Report that the memory source `plugin:<package>` does not support the command; the built-in store is not opened |
| Sub-agents | No memory (unchanged) |

The refusals reuse the paths that already refuse for an injected service
(`app/management.rs:440-449`, `cli/run/subcommands.rs:18-23`).

## Authorization

- Selecting `backend = "plugin:<package>"` in user or managed configuration is
  the user's learning grant. Otto builds the client in `DelegatedLearning` mode
  with a grant for the user and workspace scopes, the input classes `message`,
  `tool_result` and `explicit_memory`, and background evolution allowed.
- Package trust is still required, as for its hooks and tools.
- The model's `remember` and `forget` calls go to the service under that grant.
  Otto keeps no review queue for a plugin source.

## Turn observation

When the description declares `observe` with delegated-learning support, Otto
calls `observe` with mode `learn` after the `turn_end` hook, using the messages
recorded for the turn (`crates/otto-core/src/agent/turn_run.rs:171-178`).

- One observation per `text`, `tool_call` and `tool_result` block, the block
  set of the hook `turn` data. Observation content is at most 8 KiB
  (`MAX_CONTENT_BYTES`, `crates/otto-core/src/memory/validate.rs`), so a block
  over 8 KiB becomes several observations, split at UTF-8 character
  boundaries. `event_id` is `<turn_id>:<message>:<block>:<part>`, with `part`
  from 0.
- One `observe` request holds at most 64 observations and 256 KiB of content
  (`MAX_ITEMS`, `MAX_BATCH_BYTES`). A turn that exceeds either limit is sent
  as several requests in order; the first failed request ends the turn's
  observation with a warning.
- `text` content is the text without the control characters that observation
  validation rejects (all but newline, carriage return and tab); blank parts
  are left out. `tool_call` content is the JSON text
  `{"id","name","arguments"}`; `tool_result` content is the JSON text
  `{"id","name","text","is_error"}`.
- `source.role` is `user`, `assistant`, `context`, `tool_call` or
  `tool_result`; `occurred_at` is absent; `trust` is `external` for
  `tool_result` and `local` otherwise. The client's input class mapping
  (`memory/client.rs:549-556`) gains `tool_call` and `context` as `message`.
- Recording starts when hooks or an observing memory source are active; today
  it starts only with hooks (`agent/turn_run.rs:133`).
- Each observation passes the memory content guard
  (`crates/otto/src/memory/guard.rs:45-70`). An observation that fails it, for
  example text holding a `[REDACTED]` marker, a private key delimiter or a
  field over 64 KiB, is left out; the other observations are sent. The
  `turn_end` hook sends such text redacted. I have not measured how many
  blocks this removes.
- A request whose command input exceeds the 1 MiB limit fails as
  `unavailable` and Otto writes a warning. With 256 KiB of content per request
  this happens only when JSON escaping expands the content about four times.
  In 1557 local turns measured on 2026-10-17 the 99th percentile of the hook
  input was 749,589 bytes, which is three or more `observe` requests.
- A failure is a warning; the turn result does not change. The call runs
  before the turn returns, bounded by the memory command timeout.

## Compatibility

- `registry.json` stores the parsed `dev.otto` components, and they are
  `deny_unknown_fields`. After a package with a `memory` component is
  installed, an Otto binary built before this change rejects the whole
  registry ("invalid plugin registry", `crates/otto/src/plugin/mod.rs:92-93`).
  Downgrading requires `otto plugin remove <package>` with the new binary
  first. The CHANGELOG states this.
- Configurations without `backend` keep the file store. A plugin that still
  uses hooks and tools keeps running beside the file store until it moves to
  the `memory` component.
- No new session entry types.

## Outside Otto: otto-mem

A separate otto-mem PR after this one merges:

- `plugin.json` declares the `memory` component and drops the `before_prompt`
  and `turn_end` hooks and the `search`, `remember` and `forget` tools.
- The plugin binary answers `describe`, `recall`, `inspect`, `remember`,
  `forget` and `observe` with otto-mem's own protocol.
- Setup adds `[memory] backend = "plugin:otto-mem"`; the bindings under
  `[plugins.bindings.otto-mem]` do not change.

## Limitations

- No Otto-side review for a plugin source. Model `remember` and `forget`
  take effect in the service under the grant, and the review commands report
  unsupported. A review flow for external sources needs a pending-changes
  operation and `prepare`/`apply` in the TUI and ACP; it is not in this change.
- `/memory list|show|search|forget` are not routed to the plugin.
- Each operation starts one process: one per request for recall, one or more
  per turn for observe, one per runner build for `describe`, as hooks do
  today.
- The user scope sent to a plugin source is `user/default`. The service
  identifies the user from its own credentials.
- The redaction-boundary check runs before the runner builds the source, so
  for a plugin source it includes all three memory tool definitions
  (`memory_search`, `remember`, `forget`), whether or not the plugin declares
  the capabilities.
- Each call rehashes the package snapshot, as hooks do.
- The command's stderr is not recorded (existing follow-up for hooks).

## Tests

All offline, with a shell-script package fixture that logs each input to a
file in the test directory.

- Configuration: `plugin:x` resolves; `plugin:` with no name and unknown values
  fail; the `remote` error names the plugin form.
- Package validation accepts a `memory` component and rejects an invalid
  command; the trust disclosure lists it.
- Recall: the fixture's item appears in the request-local memory message; a
  fixture exit 1 gives a warning and the turn completes.
- `/remember` sends `remember` with origin `human`; no file store is opened.
- `enabled = false` with a plugin backend: the fixture log stays empty.
- Turn with one tool call: `observe` holds the user text, assistant text,
  tool call and tool result; an observation holding `[REDACTED]` is left out
  and the others are sent.
- A `remember` timeout reports `outcome_unknown`, not success.
- `required = true` with an untrusted package fails the runner build;
  `required = false` gives a warning and no memory tools.
- Revoking the package between two calls makes the second call
  `unavailable`.

## Documentation

- `docs/manual/memory-and-extensions.md`: source selection, the `memory`
  component and its command protocol; replace the beside-built-in otto-mem
  setup.
- The hooks spec gets a status line pointing here for the replaced decision.
- CHANGELOG entry, including the registry downgrade note.
- The `memory_search`, `remember` and `forget` descriptions are already
  service-neutral (`crates/otto/src/tool/memory_service.rs:319-336`) and do not
  change.
