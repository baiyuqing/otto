# Context inspector: what the next provider request contains

Status: implemented 2026-09-25 with D1 (a): the report shows the last turn's memory recall. Current behavior is documented in the [user manual](../user-manual.md).

## Problem

A session's context is not observable. The TUI and the web UI show one
number, the provider-reported input tokens of the last response
(`session::Snapshot::context_input_tokens`). Neither shows what the request
is made of, how many tokens each part costs, or the text of each part.

The request is assembled in one place,
`otto_core::agent::Agent::build_normal_provider_request`
(`crates/otto-core/src/agent/mod.rs`):

| Request part | Source |
|---|---|
| `system_prompt` | `Options::system_prompt`, one string concatenated in `cli/runtime_builder.rs`: the base prompt (`cli/prompt.rs::system_prompt_for`), `## Environment` and `## Workspace instructions` (`cli/workspace_context.rs`), `## Skills` (`skill/prompt.rs`), and `## Agents` (`subagent/prompt.rs`). |
| `tools` | `Registry::definitions()`: built-in tools and `mcp__<server>__<tool>` tools. |
| `messages` | `Session::messages()`, which starts with a `[Compaction summary]` user message when a compaction checkpoint is in force, with the tool-result overlay applied. |
| memory recall | A user message inserted before the current user message. It is per-turn state (`RunDispatchState::memory_context`) and is never written to the session. |

`agent::context_estimate` already estimates tokens (3 bytes per token plus
fixed framing costs). It is the estimate that the compaction triggers use.

## Design

### 1. One report, computed from the request the agent builds

`otto-core` adds `agent::context_report`:

```rust
pub struct ContextReport {
    pub model: String,
    pub context_window: i64,          // 0 when not configured
    pub compaction_threshold: i64,    // 0 when automatic compaction is off
    pub estimated_total: i64,         // estimate_request(), the value compaction uses
    pub reported_input_tokens: Option<i64>, // last provider-reported input tokens
    pub sections: Vec<ContextSection>,
}

pub struct ContextSection {
    pub kind: SectionKind,  // system_prompt | tools | compaction_summary | memory | messages
    pub label: String,      // "Workspace instructions", "mcp__github__search", "assistant", ...
    pub tokens: i64,        // estimate_string / estimate_message / estimate_tool_definition
    pub items: Vec<ContextItem>,
}

pub struct ContextItem {
    pub label: String,  // part heading, tool name, or "#12 tool_result read"
    pub tokens: i64,
    pub text: String,   // the exact text sent: prompt part, tool JSON schema, message blocks
}
```

`Agent::context_report(&self)` calls `build_normal_provider_request` and
breaks the returned `Request` down. The report and the provider call
therefore use the same messages, overlay, tool list, and system prompt.
They cannot diverge.

- **System prompt parts:** `Options` gains `system_prompt_parts: Vec<(String,
  String)>` (label, text). The composition root fills it with the pieces it
  already concatenates. A test asserts that the concatenated parts equal
  `system_prompt` byte for byte. If they differ, the report shows the whole
  prompt as one part labeled `system prompt`. Splitting the final string at
  `## ` headings is not used, because AGENTS.md content contains its own
  `## ` headings.
- **Tools:** one item per definition, with its serialized JSON. Built-in and
  `mcp__*` tools are two sections.
- **Messages:** one item per message, in request order, with its role, its
  tool name for tool calls and results, and its full text. Reasoning blocks
  are not sent, so they are listed with 0 tokens.
- **Totals:** the sum of the section estimates and `estimated_total` can
  differ. `estimated_total` is anchored on the last provider-reported usage
  when there is one. Both totals are shown, and every section number is
  labeled as an estimate.

The report is a read-only snapshot. It reads the session's current
messages, so a report taken during a running turn shows the state at that
moment. It does not add to or change the session file.

### 2. Transport

- **HTTP:** `GET /v1/sessions/{id}/context` returns the report as JSON. The
  server already returns every message through `/history`, so this endpoint
  exposes nothing new except the system prompt and tool schemas. Both are
  already redacted before they reach `Options`.
- **Wire type:** the report struct lives in `otto-core` so that `otto-web`
  can deserialize it and the TS UI can use the same field names.

### 3. TUI

`/context` opens a modal:

```text
Context  gpt-5 · ~41.2k / 272k tokens (estimate) · last reported 39.8k · compacts at 200k
  System prompt            6.1k  ██
    Base                   1.2k
    Environment            0.4k
    Workspace instructions 3.9k
    Skills                 0.6k
  Tools (built-in, 12)     4.3k  █
  Tools (MCP, 5)           2.0k
  Compaction summary       3.1k  █
  Messages (84)           25.7k  ████████
```

Up and Down move the selection, Enter expands a section into its items or
opens an item's full text in a scrollable view, and Esc goes back. The
modal reuses the existing session picker modal and scroll handling.

### 4. Web UI

A "Context" button in the header opens a side panel. It shows the same
header line, one bar per section, and a `<details>` element per section and
per item that renders the item text in a `<pre>`. The panel is refreshed
when it is opened and after each `turn_completed` event.

## Out of scope

- A history of the context per turn, which would need a new session record.
- Exact tokenizer counts. `openai-compatible` endpoints have no common
  tokenizer.
- The REPL. It can print the same report later if needed.

## Decision needed

**D1: memory recall.** Recall runs at the start of each turn with that
turn's user text, so the recall the next turn will insert is unknown until
that turn starts.

- (a) Recommended: the agent keeps the last turn's recalled text, and the
  report shows it as a `Memory (last turn)` section, marked as recomputed
  on the next turn.
- (b) Leave recall out of the report, and state that in the modal footer.

## Tests (TDD order)

1. `otto-core`, `agent::context_report`:
   - The sections' messages, tools, and system prompt equal the `Request`
     the scripted provider received for the next step.
   - Parts that do not concatenate to `system_prompt` fall back to a single
     part.
   - The per-item estimates sum to the section totals.
2. `otto`:
   - The composition-root parts concatenate to `system_prompt`.
   - The HTTP endpoint returns 200 with JSON, and 404 for an unknown session.
   - The TUI modal renders the header and the sections, and supports
     expand and back.
3. `ui`:
   - The panel renders the sections from a fixture report.
