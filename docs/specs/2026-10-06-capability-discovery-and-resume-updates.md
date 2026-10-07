# Tool-set change notice for resumed sessions

Status: approved, 2026-10-06.

## Problem

When a session is opened or resumed, Otto rebuilds the runner from the current
binary and configuration. Runner construction (`crates/otto/src/runtime/builder.rs`)
builds a new tool registry and a new system prompt, and every ordinary provider
request carries the registry's current tool definitions. The transcript is not
rebuilt: earlier messages still show the tools and call patterns of the runtime
that wrote them, and nothing in the request marks that the tool set differs.

Observed: a session created before an Otto upgrade and resumed afterwards did
not use a tool the upgrade added, while a new session on the same binary and
model did.

## Existing capability guidance

The current system prompt already contains the capability guidance that
depends on the registry. `system_prompt_for` in
`crates/otto/src/runtime/prompt.rs` renders:

- `<available_tools>`: the names of the registered tools (bash is omitted when
  the sandbox makes it unusable);
- one policy section per capability whose rules involve several tools or the
  session lifecycle, each rendered only when its tool is registered:
  `otto_help_policy`, `approval_policy`, `reflection_policy`,
  `task_control_policy`, and `delegation_policy`.

The remaining capabilities state their purpose and limits in their tool
descriptions: `remind` (session timer, at most 8, 1 to 3600 seconds, contrasted
with `schedule`), `schedule`, `remember`/`forget` (human-reviewed),
`mcp_search_tools` (search before `mcp_call_tool`), and the web tools. Because
the prompt is rebuilt on resume, a resumed session already receives this
guidance for the current tool set.

This design therefore adds no capability catalog. A second catalog would copy
text that the policy sections and tool descriptions already hold, and the two
copies would need to be kept identical. What is missing is a statement that
the tool set changed relative to the transcript; this design adds that.

## Goals

1. When a session's tool set differs from the tool set recorded for it, add
   one statement to the session history saying which tools were added and
   removed. The statement stays in the model context on every later request,
   including after compaction.
2. Give sessions written before this change, which have no record, the same
   statement once, listing the current tools.
3. Keep session history append-only and Pi v3 compatible. Old sessions open
   unchanged.
4. Show the statement to the user in the transcript.

## Non-goals

- A separate capability catalog or capability metadata registry (see above).
- Detecting changes to a tool's description or schema. The request already
  carries the current definition; a notice that only says "schema changed"
  gives the model no information the definition does not.
- Detecting skill, agent-definition, or sandbox changes. Skills and agents are
  listed in the current system prompt, and sandbox state is rendered in
  `<sandbox_policy>`.
- Using the record for authorization. Registry availability, approval,
  workspace containment, sandboxing, and effect-time guards decide what runs.
- Using build provenance (version, Git revision, Git time) to detect change.
  Two builds can have the same tool set, and the tool set can change without
  a new build (configuration, MCP servers, project trust).

## Design

### Session record `otto.capabilities`

Otto appends a Pi v3 `custom` entry with `customType` `otto.capabilities`:

```json
{"tools": ["agent", "agent_wait", "bash", "edit", "read"]}
```

`tools` holds the names of the runner's tool definitions, sorted and without
duplicates. It holds no hashes, descriptions, schemas, paths, or
configuration values; every name in it is sent to the provider as a tool
definition.

The record is resolved the same way as `otto.runtime`: the latest
`otto.capabilities` entry on the active branch path applies. An entry on
another branch does not apply. A record whose data does not decode as the
shape above is ignored with a session warning, as if absent; it never prevents
the session from opening. `pi_entry_to_context_messages` skips `custom`
entries, so the record never becomes a model message or a transcript line.

`Session` gains `fn observed_tools(&self) -> Option<Vec<String>>`, returning the
resolved record. As with `append_custom`, the method has no default body:
every implementation, including wrappers such as `SharedSession`, forwards or
states its behavior. The in-memory `MemorySession` keeps the latest record it
was given; the native store resolves it from the active path.

### The notice is a persisted context message

At the start of each run, after calls left unanswered by a takeover are
replayed and queued notifications are delivered, and before the run's user or
follow-up message is appended, the agent compares the sorted names of its
current tool definitions with `observed_tools()`:

| Record | Session history | Notice |
| --- | --- | --- |
| equal to current names | any | none |
| differs from current names | any | changed |
| absent | has an assistant message or a compaction | legacy |
| absent | none | none |

A notice is appended as a `Role::Context` message with context type
`capability_update` and `display` set, built by `new_context_message`, the
same construction task notifications use. It is stored as a Pi
`custom_message` entry. `capability_update` joins `task_notification` and
`parent_message` as context types whose text is reloaded without the
`[Custom context: ...]` prefix, so a reopened session sends the same text.

Changed:

```xml
<runtime_capability_update>
The tools available in this session changed after the earlier messages were written. Those messages may show tools or usage that no longer apply.
Added: agent, agent_wait
Removed: web_search
Use only the tools defined in the current request. Their descriptions and the policy sections of the system prompt state how to use them.
</runtime_capability_update>
```

`Added:` or `Removed:` is omitted when its list is empty.

Legacy:

```xml
<runtime_capability_update>
The tool set in effect when the earlier messages in this session were written was not recorded. Those messages may show tools or usage that no longer apply.
Current tools: agent, agent_wait, bash, edit, read
Use only the tools defined in the current request. Their descriptions and the policy sections of the system prompt state how to use them.
</runtime_capability_update>
```

Names are listed only if they satisfy the rule `<available_tools>` uses (ASCII
letters, digits, `_` and `-`, at most 64 bytes). That rule moves from
`crates/otto/src/runtime/prompt.rs` to `otto-core` so both use one function.
If filtering leaves no name to list, no notice is appended.

After the notice, or when there is no notice, the agent appends a new
`otto.capabilities` record if the record is absent or differs. The message is
written before the record: if the record write fails, the next run appends a
second notice, which is accepted; the reverse order could lose the notice.
Either failed write fails the run with a persist error, as other session
writes do.

The notice is placed before the user's message for the same reason queued
notifications are: the user's prompt then reads as following the change. It
is in the session history, so every later provider request contains it at
that position, or, after compaction summarizes it, in the summary's
`<capability-updates>` block (see Compaction). Each later change
appends one more notice at the position where that change took effect.

### Prompt cache

The notice is appended after the existing history, and both providers send a
context message as a separate `user` message at its position
(`openaicompat/protocol.rs`, `openairesponses/protocol.rs`). The system
prompt, tool definitions, and earlier messages of the request are unchanged by
the notice, so a cached prefix up to the previous request's end stays valid.
When the tool set changes, the tool definitions and the registry-dependent
system prompt sections at the start of the request change, which invalidates
the cached prefix once; that cost comes from the tool-set change, not from the
notice. Later requests have the same prefix again.

### Compaction

Compaction groups a leading run of context messages with the turn that
follows it (`group_compaction_messages` in `agent/compaction_select.rs`), so
the notice stays with the user message after it, and a compaction either
keeps the notice verbatim in the retained tail or summarizes it together with
every earlier message.

A summary is model output and can omit the notice. Otto therefore carries
every summarized notice verbatim, the same way it carries summarized user
messages (`agent/summary_details.rs`): the complete summary gains a
`<capability-updates>` block after the `<user-messages>` block, one JSON
string per line, oldest first:

```text
<capability-updates>
"<runtime_capability_update>\nThe tools available in this session changed ...\nAdded: agent\n...\n</runtime_capability_update>"
</capability-updates>
```

- Its lines are the notices the previous summary carried, followed by the
  text of each `capability_update` message in the summarized source
  (historical source, then turn prefix), in transcript order.
- Before a summary is summarized again, the block is stripped with the other
  trailing blocks and rebuilt from the stored lines, so the summarizer never
  rewrites it and a second compaction does not duplicate it.
- Bounds: a notice longer than 8,000 characters is cut and marked
  `[capability update truncated for compaction]`; when the block would exceed
  16 KiB, the oldest lines are dropped. The newest notice is therefore always
  carried, cut if necessary. Sixteen KiB holds the notices of at least two
  tool-set changes of 100 tools each.
- Lines are redacted like carried user messages.

The `otto.capabilities` record is not a message and is not summarized; after
compaction it still matches the current tools, so no second notice is
appended.

### Context report and transcript

The notice is an ordinary context message, so the context report lists it in
the Messages section and the transcript shows it as a context entry. It
becomes visible when the run that appends it starts.

### Sub-agents

A child task has its own transcript and its own registry. Its record lists
the child's tools and is written to the child transcript. A child never reads
or writes the parent's record.

A child started with `context: inherit` replays the parent's messages into
its own transcript without a record, so its first run appends the legacy
notice listing the child's tools. The legacy text therefore states only that
the earlier tool set was not recorded, which is true both for sessions written
before this change and for inherited snapshots.

## Compatibility

- Sessions without a record open unchanged and receive the legacy notice
  once, on their first run with history.
- Older Otto versions resolve only the custom types they know and skip
  `otto.capabilities`; they reload a `capability_update` message with the
  `[Custom context: capability_update]` prefix.
- Compaction appends a compaction entry and removes no entry, so the record on
  the active path survives compaction. Archive reopen and takeover resolve the
  record from the same file.
- A downgrade or a removed tool produces a `Removed:` line. Otto does not
  emulate removed tools that appear in earlier tool calls.

## Limitations

- The notice states the change; it does not guarantee that a model uses a new
  tool. Default tests are offline and verify what is sent, not model behavior.
  A live check is to reopen the session in which the problem was observed and
  give it a task that needs the added tool.

## Tests

Agent (`otto-core`, in-memory session and scripted provider):

- A new session appends no notice and writes a record.
- A session with a different record appends one `capability_update` message
  with the exact added and removed names before the user's message, and
  updates the record. That run's provider request contains the message; the
  next run appends no second notice, and its request still contains the first.
- A session with history and no record appends the legacy notice once.
- A session with a matching record appends nothing.

Native store:

- A `capability_update` message reloads with identical text and no prefix.
- A record survives reopen; the latest record on the active branch applies and
  a record on another branch does not.
- A malformed record is ignored with a warning and does not prevent opening.

Compaction (`otto-core`):

- A compaction that summarizes a `capability_update` message puts its text in
  the `<capability-updates>` block; the next provider request contains the
  block. A second compaction carries the line once, not twice.
- The block is stripped before re-summarization and survives a compaction
  whose source has no notice.
- An oversized notice is cut and marked; the oldest lines are dropped at the
  byte bound and the newest is kept.

Prompt:

- `<available_tools>` and the notice apply the same name rule.

## Documentation

- User manual: resumed sessions receive the current tools and, when the tool
  set changed, one `capability_update` transcript entry listing the change;
  the compaction section lists the `<capability-updates>` block next to
  `<user-messages>`.
- Development guide: the `otto.capabilities` custom entry, the
  `capability_update` context type, and the `Session::observed_tools`
  contract, next to the `append_custom` contract.
- Changelog: one entry under `Unreleased`.
