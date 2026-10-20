# External memory as a plugin through hooks

Status: approved 2026-10-20 (changes 1 and 2; phase 2 as a separate PR).
The decision that the plugin runs beside the built-in memory is replaced by
[a plugin as the memory source](2026-10-20-memory-provider-plugin.md).

## Problem

`[memory] backend = "remote"` (#493, #495) places one service's client and data
model inside Otto:

- `crates/otto/src/memory/remote/` (2857 lines) holds the HTTP client, wire
  types and turn upload queue of the otto-mem protocol.
- otto-core carries `RemoteProjection`, the organization namespace and its
  fixed priority (`crates/otto-core/src/memory/memory_types.rs:88-127`) and
  `service == "remote"` (`crates/otto-core/src/memory/validate.rs:158`).
- Seven call sites branch on `identity == "remote"` or `record.remote`:
  `memory/client.rs:227,348,417`, `tool/memory_service.rs:127,144`,
  `runtime/wiring.rs:251`, `app/prompt.rs:369-379`, and the recall rendering in
  `otto-core/src/agent/memory.rs:111-130`.

A second memory service would need a second backend in Otto.

## Decisions

- 2026-10-20 (user): an external memory is a plugin that implements what Otto
  needs. The protocol between the plugin and its service belongs to the plugin.
- 2026-10-20 (user): the plugin uses the existing plugin hooks, extended where
  they lack data. It runs beside the built-in memory; it does not replace the
  memory backend.

## What a memory plugin uses

| Need | Mechanism | Change in Otto |
| --- | --- | --- |
| Recall before each user input, including service-placed items | `before_prompt` hook returns `context` | None |
| Model search, remember, forget | Package custom tools (`plugin__<package>__<tool>`) | None |
| Content of the finished turn | `turn_end` hook | [Change 1](#change-1-turn-content-in-turn_end) |
| Service credential | Package command environment | [Change 2](#change-2-credential-bindings-for-package-commands) |
| Network access to the service | `[sandbox].network = "allow"` (default) | None |
| Only the plugin memory, no built-in memory | `[memory] enabled = false` | None |

Today `turn_end` data is `status`, `stop_reason` and `error`
(`crates/otto-core/src/agent/turn_run.rs:166-172`). Hook and tool commands get
the sandbox environment (`crates/otto/src/runtime/builder/runner_entry.rs:16`),
which is the environment `bash` gets; credential bindings exist only for MCP
servers (`crates/otto/src/plugin/runtime.rs:448-490`).

## Change 1: turn content in `turn_end`

`turn_end` data gains `turn`:

```json
{
  "status": "success",
  "stop_reason": null,
  "error": null,
  "turn": {
    "messages": [
      {"role": "user", "blocks": [{"type": "text", "text": "fix the test"}]},
      {"role": "assistant", "blocks": [
        {"type": "text", "text": "Running it."},
        {"type": "tool_call", "id": "call1", "name": "bash", "arguments": {"command": "cargo test"}}
      ]},
      {"role": "tool", "blocks": [
        {"type": "tool_result", "id": "call1", "name": "bash", "text": "ok", "is_error": false}
      ]}
    ]
  }
}
```

- `messages` are the messages appended to the session during this run, in
  order. Roles are `user`, `assistant`, `tool`, `context`; block types are
  `text`, `tool_call`, `tool_result`. This is the block set the remote
  uploader sends today (`crates/otto/src/memory/remote/wire.rs:176-199`).
  Image and reasoning blocks are omitted.
- Messages are recorded when they are appended, so compaction during the turn
  does not remove them. The core agent records them while hooks are active
  for the run. The recording in
  `crates/otto/src/runtime/builder/shared_session.rs:22-38` is not reused,
  because the remote uploader starts and finishes it for each prompt step.
- The text is redacted by the redactor that already processes all hook input
  (`crates/otto/src/plugin/hooks.rs:273`).
- When the complete hook input would exceed the existing 1 MiB input limit,
  `turn` is replaced by `"turn_omitted": "size"` and the hook still runs.
  Today an input over 1 MiB makes the hook fail. In 1557 local turns measured
  on 2026-10-17 the 99th percentile was 749,589 bytes, so under 1% of turns
  lose their content.
- Sub-agent turns also run `turn_end`. `session.parent_session_id` in the hook
  input identifies them; the plugin decides whether to use them.
- Every trusted package with a `turn_end` hook receives `turn`. Trusted hooks
  already receive the user prompt (`before_prompt`) and tool output
  (`after_tool`); the new data adds assistant text.

## Change 2: credential bindings for package commands

`[plugins.bindings]` today accepts `plugin:server` keys for MCP servers. A key
that is a package name without `:` binds environment variables for that
package's hook and tool commands:

```toml
[plugins.bindings.otto-mem]
env = { OTTO_MEM_TOKEN = "${OTTO_MEM_TOKEN}" }
```

- Values resolve from Otto's process environment with the existing
  `${NAME}` template rules. Literal secrets are rejected, as for MCP bindings.
- Bound values are added to the hook input/output redactor and to the
  model-visible redactor.
- They are passed only to that package's commands; `bash` and other packages
  do not receive them.
- Only user and managed configuration can set bindings; project layers cannot
  (existing rule, `crates/otto-core/src/config/plugins.rs:11`).
- `headers`, `auth` and OAuth fields stay MCP-only and are rejected on a
  package key.

Package commands run in the same sandbox as `bash`. The Seatbelt contract
check `sibling_process_environment_is_unreadable` (2026-10-20) starts an
ad hoc signed process with a marker variable and runs `ps -E` on its pid. The
probe outside the sandbox prints the marker; the probe inside the profile does
not. Apple platform binaries do not expose their environment to `ps -E` even
outside a sandbox, so the check uses an ad hoc signed copy of `/bin/sleep`.

## Phase 2 (separate PR): remove the remote backend

After an otto-mem plugin works with changes 1 and 2:

- Delete `crates/otto/src/memory/remote/`, `[memory.remote]`, the remote
  types and validation in otto-core, and the seven branches listed above.
- `backend = "remote"` becomes a configuration error that names the plugin
  replacement.
- Move `docs/manual/remote-memory-protocol.md` to the otto-mem repository.

## Outside Otto

The otto-mem plugin lives in the otto-mem repository: a `plugin.json` with a
`before_prompt` hook (recall), a `turn_end` hook (upload) and tools (search,
remember, forget), plus the client for otto-mem's own protocol.

A `before_prompt` hook failure or timeout fails the turn
(`crates/otto-core/src/agent/turn_run.rs:150-153`). The earlier decision that
Otto keeps working when the memory service fails is met by the plugin: it
applies its own timeout below the hook timeout and exits 0 with no output when
the service fails.

## Limitations

- Recalled memory is rendered as `<plugin_context>Untrusted plugin context`,
  not with the memory preamble.
- `turn_end` runs before the turn returns, so the upload time adds to turn
  completion, bounded by the hook timeout (default 10 s, at most 300 s).
- Each event starts a new process. The plugin has no in-process queue between
  turns.
- `/remember` and the built-in `memory_search`, `remember` and `forget` tools
  keep using the built-in memory.
- With `[sandbox].network = "deny"` the plugin cannot reach a network service.

## Tests

- A scripted turn with one tool call: the `turn_end` input contains the user
  text, assistant text, tool call and tool result. Fails if `turn` is absent or
  a block is missing.
- A turn whose hook input exceeds 1 MiB: the hook runs and receives
  `turn_omitted: "size"`. Fails if the hook is not run.
- Compaction during a turn: `turn` still contains messages from before the
  compaction.
- A package binding reaches that package's hook command environment and is
  absent from the `bash` environment and from another package's command.
- A bound value in hook output is redacted.

## Documentation

Update the hook protocol table and plugin bindings in
`docs/manual/memory-and-extensions.md`, and add a CHANGELOG entry. Hooks are
not model-callable, so no capability description changes.
