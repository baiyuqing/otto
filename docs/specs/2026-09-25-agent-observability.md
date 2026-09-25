# Agent observability: reasoning, sub-agent transcripts, turn phase

Status: approved 2026-09-25. The user accepted D1 (Pi `thinking` blocks), D2
(per-parent directory), and D3 (no configuration switch) as recommended.

## Problem

While a turn runs, and when reviewing it afterwards, three pieces of state
cannot be observed. Each item names the code that causes it.

1. **Reasoning is discarded.** The Responses request sends only
   `reasoning.effort` (`openairesponses/protocol.rs`, `WireReasoning`), so the
   backend returns no reasoning summary. The stream assembler also ignores
   every output item that is not a function call
   (`openairesponses/stream.rs`, `ignores_output_items_that_are_not_function_calls`).
   Chat Completions ignores `delta.reasoning_content` and `delta.reasoning`
   (`openaicompat/protocol.rs`, `WireDelta`). `agent::Event` has no reasoning
   variant. While the model reasons, the TUI shows only `Thinking… Ns`
   (`tui/render.rs`, `thinking_line`).
2. **Sub-agent transcripts are not persisted.** `subagent/runner.rs` states
   that each child "owns its own in-memory transcript, which is never
   persisted". During a run the parent keeps the last 500 bytes of child
   text and the last tool name with a 60-character argument preview
   (`ChildProgress::handle`). After `remove_final`, or after the process exits,
   nothing records which tools the child called or what they returned.
3. **The turn phase is not shown.** `Thinking… Ns` counts from the start of
   the turn (`App::busy_since`), whether Otto is waiting for the first token,
   receiving reasoning, running `bash`, or waiting out a provider retry
   backoff. Retries in `provider/openaicompat.rs` (up to 3 attempts, 250 ms
   base backoff, or `Retry-After`) emit no event.

## Scope

In scope: the three items above for the TUI, the web UI, the HTTP/SSE event
stream, and the session file.

Out of scope: reasoning text in headless `--prompt` stdout; new `/metrics`
series; tracing/OpenTelemetry export; a separate session viewer command
(`--resume PATH` opens any session file, child sessions included).

## Design

### 1. Reasoning

**Provider layer (otto-core).**

- `StreamEvent::ReasoningDelta { text }` is added next to `TextDelta`.
- Responses: when `request.thinking` is non-empty, the request sends
  `"reasoning": {"effort": <thinking>, "summary": "auto"}`. The assembler
  handles `response.reasoning_summary_text.delta`: it appends the delta to a
  reasoning buffer and emits `ReasoningDelta`. Summary parts are joined with
  a blank line (`response.reasoning_summary_part.added` after the first one).
- Chat Completions: `WireDelta` decodes `reasoning_content` (DeepSeek, Qwen,
  vLLM) and `reasoning` (OpenRouter). A non-empty value is appended to the
  buffer and emitted as `ReasoningDelta`.
- `emitted` is set by reasoning deltas as well, so a stream that already
  showed reasoning is not retried (same rule as text).
- The assembled `Response::message` gets one `BlockType::Reasoning` block
  before the text block when the buffer is non-empty. The block holds the
  text only; no signature or encrypted content is kept.

**Agent layer (otto-core).**

- `Event::ReasoningDelta { text }`, wire name `reasoning_delta`.
- Reasoning deltas pass through their own `StreamRedactor` instance, so a
  secret that the redactor knows is replaced before it reaches an event or the
  session, the same as text deltas today.
- Both request builders skip `Reasoning` blocks. Reasoning is never sent back
  to a provider: DeepSeek rejects `reasoning_content` in input messages, and
  Responses with `store: false` cannot replay a reasoning item without
  encrypted content. Compaction input and context estimates also skip it.

**Session (Pi v3).** A `Reasoning` block is written as a Pi-native assistant
content block `{"type":"thinking","thinking":"<text>"}` and read back as
`BlockType::Reasoning`. Today the loader rejects `thinking` blocks
(`session/context.rs`, "Pi message content is not supported by Otto"); that
branch is changed to accept them for the assistant role only.

Compatibility cost: an Otto binary built before this change fails to open a
session that contains a `thinking` block. New binaries read every existing
session unchanged. See decision D1.

**Frontends.**

- `wire/events.rs` encodes `reasoning_delta`; `wire/transcript.rs` folds
  deltas into a reasoning entry and rebuilds it from history; `otto-web`'s
  event union and the UI add the entry type.
- TUI: reasoning lines are drawn dimmed, above the assistant text of the same
  response. The web UI shows the entry collapsed by default, with the first
  line visible.

### 2. Sub-agent transcripts

- When the parent session is a file-backed `Store`, each child gets its own
  file-backed `Store` in place of `MemorySession`:

  ```text
  ~/.otto/sessions/<workspace-key>/<parent-session-id>/<task-id>-<child-session-id>.jsonl
  ```

  The header's Pi field `parentSession` holds the parent session file path.
  The file is created lazily, as for a parent session, and written with the
  same `0600`/`0700` modes. `Store` gets a constructor that takes the target
  directory and `parentSession` explicitly; the default path layout is
  unchanged.
- The inherited context snapshot (`context: inherit`) is written into the
  child file, because it is the context the child ran with.
- `/resume`, `--continue`, and the session list stay unchanged: `list` reads
  only `*.jsonl` directly in the workspace directory, so child files do not
  appear there.
- Archiving a parent also moves `<parent-session-id>/` into `archive/` when
  that directory exists, so a child directory never outlives its archived
  parent in the active listing directory.
- `Task` gets `session_path`. `/tasks <id>` in the REPL and TUI, and the
  server's task JSON, show it. It is not added to the completion notification,
  so the model context does not change.
- With `--no-session`, or when the parent session is in memory, children stay
  in memory, as today.
- A write failure on a child file fails that child task with the store error;
  it does not affect the parent.

### 3. Turn phase

- `StreamEvent::Retry { attempt, max_attempts, delay, reason }` is emitted by
  `provider/openaicompat.rs` before each backoff sleep. The agent forwards it
  as `Event::ProviderRetry`, wire name `provider_retry`. `reason` is the HTTP
  status or the transport error class, never a response body.
- The phase is computed by the frontend from events, with no new state in
  the agent:

  | Last event | Phase shown |
  | --- | --- |
  | `agent_started`, `tool_call_finished`, `compaction_completed` | `waiting for model` |
  | `reasoning_delta` | `reasoning` |
  | `text_delta` | `responding` |
  | `tool_call_started` | `running <tool> <60-char argument preview>` |
  | `provider_retry` | `retry <attempt>/<max> after <reason>, waiting <delay>` |
  | `compaction_started` | `compacting` |

- TUI: `Thinking… Ns` becomes `<phase> · <phase seconds>s · turn <turn seconds>s`.
  Web UI footer shows the same string. The shared reducer in
  `wire/transcript.rs` computes the phase so both frontends use one
  implementation.

## Decisions

- **D1. Reasoning storage format.** Recommended: Pi-native `thinking` content
  blocks in the assistant message. Resumed sessions show reasoning with no
  extra plumbing, and the file stays Pi-readable. Cost: binaries older than
  this change cannot open new sessions that contain reasoning.
  Alternative: a separate `custom` entry (`customType: "otto.reasoning"`)
  that old binaries ignore. Cost: a new `Session` trait method for every
  implementation, and resumed history needs a second read path to show it.
- **D2. Child transcript location.** Recommended: the per-parent directory
  above. Alternative: a flat `subagents/` directory per workspace.
- **D3. Reasoning request for Responses.** Recommended: always send
  `summary: "auto"` when a thinking effort is set; no configuration switch.

## Verification

TDD per slice, each with a failing test first:

1. otto-core stream tests: Responses reasoning summary deltas and multi-part
   joining; Chat Completions `reasoning_content` and `reasoning`; `emitted`
   blocks a retry after reasoning only.
2. Request-builder tests: a `Reasoning` block never appears in either wire
   request; the Responses request carries `summary: "auto"` only with an
   effort.
3. Agent tests: reasoning deltas are redacted; the `Reasoning` block reaches
   the session; `ProviderRetry` is forwarded.
4. Session codec/context tests: `thinking` round-trips for assistant messages
   and is rejected for other roles.
5. Sub-agent tests: a child with a file-backed parent writes its own file
   with `parentSession`; `list` does not return it; archive moves the
   directory; `--no-session` keeps children in memory.
6. Wire/transcript reducer and TUI render tests for the reasoning entry and
   the phase string; UI tests for the collapsed entry and footer.

Gates: `make check-fast` per slice, `make check` before review.

Docs updated in the same change: user manual (Sessions, Events, TUI behavior,
sub-agent section), development guide event list.
