# Otto Context Compaction Design

**Date:** 2026-08-28
**Status:** Approved in chat; pending written-spec review

## 1. Purpose

Otto currently sends its stable system prompt, ordered tool definitions, and complete active session context on every OpenAI-compatible Chat Completions request. That preserves fidelity and prompt-cache reuse, but indefinitely growing sessions eventually become expensive, lose focus, or exceed a model's context window.

This design adds production-quality context compaction while preserving Otto's Stage 1 boundaries:

- The only supported transport remains `openai-compatible`.
- GPT and Claude model metadata may be recognized when those models are exposed by an OpenAI-compatible endpoint.
- This does not add Anthropic API support, Claude subscription support, or Codex subscription support.
- Persistent sessions remain append-only Pi v3 JSONL.
- Existing Otto v1 files remain untouched and unsupported.

Compaction replaces an old portion of the active model context with a structured summary while retaining recent messages verbatim. It never rewrites or deletes the underlying transcript records.

## 2. Goals

- Avoid context-overflow failures during long coding sessions.
- Reduce repeated input-token cost while retaining goals, constraints, decisions, progress, and exact engineering details.
- Preserve assistant tool-call and tool-result protocol validity.
- Minimize prompt-cache invalidation by compacting infrequently and removing enough history to amortize the one-time cache miss.
- Support manual, proactive threshold, and reactive overflow compaction.
- Persist genuine Pi v3 compaction checkpoints that Pi 0.84.3 can consume.
- Keep cancellation, retries, session replacement, and process shutdown race-safe.
- Keep default tests offline and deterministic.
- Make automatic behavior work without requiring mainstream GPT or Claude users to configure context limits.

## 3. Non-goals

- Provider-native opaque compaction APIs from OpenAI, Anthropic, or xAI
- Anthropic Messages or OpenAI Responses transport support
- Claude or Codex subscription authentication
- Embeddings, retrieval-augmented memory, vector databases, or hierarchical long-term memory
- A separately configured summarization model
- Runtime internet lookup or downloading model metadata
- A comprehensive catalog of model families beyond GPT and Claude
- Exact provider tokenization before every request
- Rewriting, deleting, or garbage-collecting old session records
- Restoring pre-compaction transcript entries into the TUI after reopening a session

## 4. Research basis

The design follows common behavior across current agent systems and model platforms:

- Pi 0.84.3 compacts at a threshold, retains a recent tail, supports `/compact [instructions]`, keeps tool calls/results valid, records `firstKeptEntryId` and `tokensBefore`, and performs at most one overflow compact-and-retry for a failing call.
- OpenAI, Anthropic, Claude Code, LangChain, and Semantic Kernel all recommend retaining stable instructions, summarizing older history, and preserving function/tool-call sequences.
- Claude Code and Pi preserve a structured task checkpoint rather than an unstructured prose synopsis.
- Provider-native compaction is useful but is not portable across generic Chat Completions endpoints.
- Prompt caching rewards a stable prefix, whereas compaction necessarily creates a new prefix. A high-water trigger and a much smaller retained tail provide the required hysteresis.

Primary references consulted for this design:

- Pi 0.84.3 `docs/compaction.md`, `docs/session-format.md`, `dist/core/compaction/*`, and `dist/core/agent-session.js`
- OpenAI model catalog and per-model pages: <https://developers.openai.com/api/docs/models/all>
- OpenAI compaction and prompt-caching guides: <https://developers.openai.com/api/docs/guides/compaction> and <https://developers.openai.com/api/docs/guides/prompt-caching>
- Anthropic model overview and pricing: <https://platform.claude.com/docs/en/models/overview> and <https://platform.claude.com/docs/en/about-claude/pricing>
- Anthropic compaction and context editing: <https://platform.claude.com/docs/en/build-with-claude/compaction> and <https://platform.claude.com/docs/en/build-with-claude/context-editing>
- Claude Code context-window guide: <https://docs.anthropic.com/en/docs/claude-code/context-window>
- LangChain short-term memory: <https://docs.langchain.com/oss/python/langchain/short-term-memory>
- Semantic Kernel chat-history reducers: <https://devblogs.microsoft.com/semantic-kernel/managing-chat-history-for-large-language-models-llms/>
- OpenRouter's OpenAI-compatible model catalog for routed GPT/Claude ID spellings: <https://openrouter.ai/api/v1/models>

The built-in model values are a static snapshot verified from official GPT and Claude documentation during design. They are not fetched at runtime.

## 5. Model-limit catalog

### 5.1 Catalog data

The configuration package owns a small, static model-limit catalog because model selection and runtime settings are resolved there. Provider wire packages do not own compaction policy.

Each record contains:

```text
canonical model family
accepted exact aliases and snapshots
combined context window
maximum documented input tokens, when distinct and documented
maximum documented output tokens
cost-aware working window
official source URL
```

The combined context window is model capacity. The working window is the default input high-water mark used for proactive compaction. A documented maximum-input limit is also treated as a hard input ceiling.

Initial coverage is intentionally limited to mainstream text/coding GPT and Claude families:

| Family | Combined context | Max input when separately documented | Max output | Default working window |
| --- | ---: | ---: | ---: | ---: |
| GPT-5.6 Sol/Terra/Luna and aliases | 1,050,000 | 922,000 | 128,000 | 272,000 |
| GPT-5.5 / GPT-5.5 Pro | 1,050,000 | 922,000 | 128,000 | 272,000 |
| GPT-5.4 / GPT-5.4 Pro | 1,050,000 | 922,000 | 128,000 | 272,000 |
| GPT-5.4 Mini/Nano | 400,000 | 272,000 | 128,000 | 272,000 |
| GPT-5, GPT-5.1, GPT-5.2 and their Pro/Codex/Mini/Nano variants | 400,000 | 272,000 where documented | up to 128,000 | 272,000 |
| GPT-5.3 Codex | 400,000 | 272,000 | 128,000 | 272,000 |
| GPT-5 chat-latest variants | 128,000 | — | 16,384 or 32,000 by exact model | 128,000 |
| GPT-4.1 / Mini / Nano | 1,047,576 | — | 32,768 | 1,047,576 |
| GPT-4o / GPT-4o Mini and documented snapshots | 128,000 | — | 4,096 or 16,384 by snapshot | 128,000 |
| o1 / o1-pro / o3 / o3-mini / o3-pro / o4-mini | 200,000 | — | 100,000 | 200,000 |
| Claude Fable 5 / Opus 5 / Sonnet 5 | 1,000,000 | 1,000,000 | 128,000 | 1,000,000 |
| Claude Opus 4.6/4.7/4.8 and Sonnet 4.6 | 1,000,000 | 1,000,000 | 128,000 | 1,000,000 |
| Claude Sonnet 4.5 | 1,000,000 | 1,000,000 | 64,000 | 1,000,000 |
| Claude Opus 4.5 and Haiku 4.5 | 200,000 | 200,000 | 64,000 | 200,000 |
| Claude Opus 4/4.1, Sonnet 4, Sonnet 3.5/3.7, Haiku 3/3.5 | 200,000 | 200,000 | model-specific | 200,000 |

OpenAI documents a higher-price tier for GPT-5.4, GPT-5.5, and GPT-5.6 requests above 272K input tokens. Their cost-aware working window is therefore 272K even though their hard capacity is larger. GPT-5-family models with a 400K combined context also document a 272K maximum input for the listed variants.

Anthropic documents standard pricing throughout the 1M window for Claude 4.6 and later. Their working window therefore remains the full documented input window. Otto still subtracts its output reserve before triggering.

### 5.2 Matching

Matching never changes the model string sent to the endpoint.

The resolver may normalize only an allowlisted routing wrapper:

- `openai/<model>`
- `anthropic/<model>`
- the documented OpenRouter `:batch` routing suffix

It then performs exact alias/snapshot matching. The initial baseline IDs are:

```text
GPT:
gpt-5.6, gpt-5.6-sol, gpt-5.6-terra, gpt-5.6-luna
gpt-5.5, gpt-5.5-pro
gpt-5.4, gpt-5.4-pro, gpt-5.4-mini, gpt-5.4-nano
gpt-5.3-chat-latest, gpt-5.3-codex, gpt-5.3-codex-spark
gpt-5.2, gpt-5.2-pro, gpt-5.2-codex, gpt-5.2-chat-latest
gpt-5.1, gpt-5.1-codex, gpt-5.1-codex-mini, gpt-5.1-codex-max,
gpt-5.1-chat-latest
gpt-5, gpt-5-pro, gpt-5-mini, gpt-5-nano, gpt-5-codex,
gpt-5-chat-latest
gpt-4.1, gpt-4.1-mini, gpt-4.1-nano
gpt-4o, gpt-4o-mini
o1, o1-pro, o3, o3-mini, o3-pro, o4-mini

Claude:
claude-fable-5, claude-opus-5, claude-sonnet-5
claude-opus-4-8, claude-opus-4-7, claude-opus-4-6,
claude-opus-4-5, claude-opus-4-1, claude-opus-4
claude-sonnet-4-6, claude-sonnet-4-5, claude-sonnet-4,
claude-3-7-sonnet, claude-3-5-sonnet
claude-haiku-4-5, claude-3-5-haiku, claude-3-haiku
```

For OpenRouter-style Claude IDs, the exact decimal-generation spellings are aliases of the corresponding hyphenated record: `claude-opus-4.8`, `claude-opus-4.7`, `claude-opus-4.6`, `claude-opus-4.5`, `claude-opus-4.1`, `claude-sonnet-4.6`, `claude-sonnet-4.5`, `claude-3.7-sonnet`, `claude-3.5-sonnet`, `claude-haiku-4.5`, and `claude-3.5-haiku`.

Documented pinned IDs add only an anchored vendor date suffix to one of these exact families, using OpenAI's `-YYYY-MM-DD` or Anthropic's `-YYYYMMDD` shape. Snapshot-specific records override their family when context or output limits differ. No other suffix is inferred.

Arbitrary substring or fuzzy matching is prohibited. Names such as an Azure deployment alias or `my-gpt-model` remain unknown unless explicitly configured. This avoids silently assigning a large window to an unrelated private deployment.

### 5.3 Resolution precedence

1. Explicit profile limits
2. Exact built-in catalog match
3. Unknown

Unknown models retain manual compaction. When automatic compaction is enabled, they also receive reactive overflow recovery if the endpoint returns a recognized overflow error. They do not receive proactive threshold compaction because Otto cannot safely guess their limit.

A gateway may impose a smaller limit than the original model vendor. Operators can override that limit, and reactive overflow remains the final safety net.

## 6. Configuration

The default behavior requires no new configuration:

```toml
[agent.compaction]
auto = true
reserve_tokens = 16384
keep_recent_tokens = 20000
```

All fields are optional. Their meanings are:

- `auto`: enables proactive threshold compaction and reactive overflow recovery. Manual `/compact` remains available when false.
- `reserve_tokens`: desired room left for the next response and protocol overhead.
- `keep_recent_tokens`: target amount of recent context retained verbatim after compaction.

A profile may override catalog limits for a private deployment or constrained gateway:

```toml
[profiles.private]
provider = "openai-compatible"
base_url = "https://example.invalid/v1"
model = "private-deployment"
api_key_env = "PRIVATE_API_KEY"
context_window = 131072
compaction_window = 100000
```

Rules:

- Both overrides must be positive integers when present.
- `compaction_window` requires `context_window` and cannot exceed it.
- If only `context_window` is set, it becomes both the hard and working window. This is an explicit request to bypass a catalog soft boundary.
- `context_window` and `compaction_window` must each be at least 4,096 tokens.
- Configured reserve and recent-tail values are desired targets. For a known working window `W`, Otto computes `effectiveReserve = min(reserve_tokens, max(1024, floor(W/4)))`, then `effectiveKeepRecent = min(keep_recent_tokens, max(1024, floor((W-effectiveReserve)/2)))`. This keeps small windows usable without changing defaults for windows of 128K or larger.
- Invalid combinations fail during runtime resolution, before credentials are used for a provider call.
- No API keys or other secrets enter catalog or compaction settings.

No new CLI flags or environment variables are added for this feature. Known GPT/Claude models work automatically, while custom deployments can use TOML.

## 7. Package responsibilities

### `internal/config`

- Decode and validate compaction settings and optional profile limits.
- Own the static GPT/Claude model-limit catalog.
- Resolve hard and working windows into non-secret runtime settings.

### `internal/model`

- Continue to hold provider-neutral messages and usage.
- Preserve the semantic invariant that `InputTokens` is total prompt input and `CachedInputTokens` is a subset of it.

### `internal/provider`

- Define a provider-neutral typed context-overflow error carrying only bounded, sanitized metadata.
- Continue to expose normal completion requests and stream events.

### `internal/provider/openaicompat`

- Classify bounded OpenAI-compatible HTTP error responses as context overflow.
- Keep all HTTP status, JSON error-shape, and message-pattern handling inside this package.
- Never implement threshold, summarization, retry orchestration, or session writes.

### `internal/agent`

- Estimate request context.
- Decide whether and why to compact.
- Select safe cut points.
- Build and execute summarization requests.
- Orchestrate proactive and overflow retry behavior.
- Emit bounded typed compaction events.

The implementation should use focused files rather than expanding the existing loop indefinitely, for example `compaction.go`, `context_estimate.go`, and `overflow.go` within the existing package.

### `internal/session`

- Validate and append one Pi v3 compaction checkpoint.
- Rebuild context from the durable checkpoint.
- Keep memory and JSONL implementations behaviorally equivalent.
- Never call a provider or select a cut point.

### `internal/app`

- Add compaction to the shared lifecycle and backend contract.
- Keep prompt, compaction, replacement, and close mutually exclusive.

### `internal/tui` and `internal/repl`

- Parse `/compact [focus]` and render events/results.
- Never make compaction decisions or mutate session records directly.

### `cmd/otto`

- Pass resolved settings through runtime construction.
- Preserve signal and process lifecycle behavior.

## 8. Token estimation

### 8.1 Usage-anchored estimate

The latest successful, ordinary assistant message with nonzero provider usage is the preferred anchor.

For an OpenAI-compatible response, `prompt_tokens` describes the request prefix before that assistant message and already includes cached prompt tokens. The estimate for the next request is:

```text
last prompt_tokens
+ estimated serialized content of that assistant message
+ estimated serialized messages appended afterward
```

The estimate does not add `CachedInputTokens` again. It also does not add all reported completion tokens blindly: reasoning tokens or other provider output that Otto does not persist are not necessarily resent. Persisted visible assistant text and tool calls are estimated from the actual message instead.

The anchor includes the system prompt and tool schemas from that call. Stage 1 has a stable system prompt and fixed default tool set. If no usable anchor exists, Otto estimates the complete current request.

A compaction-summary call is never a valid anchor for the normal conversation because it used a different prompt. Immediately after compaction, Otto estimates the rebuilt summary plus retained tail from content until the next normal provider response supplies a fresh anchor.

### 8.2 Fallback estimate

Fallback estimation includes:

- system prompt
- ordered tool definitions and JSON schemas
- role/content framing
- assistant tool names and arguments
- tool-result text
- context summaries

The heuristic is conservative for UTF-8 and code-heavy content and adds fixed framing overhead. It uses saturating arithmetic. Exact tokenization is not promised because Otto supports arbitrary OpenAI-compatible gateways and Claude tokenization is not available as a stable local library.

Provider-reported usage quickly replaces the heuristic after each successful call.

### 8.3 Thresholds

Let:

```text
softTrigger = effectiveWorkingWindow - effectiveReserve
hardTrigger = effectiveHardInputWindow - effectiveReserve
```

If no separately documented hard input window exists, combined context capacity supplies the hard base.

Cached input counts toward both thresholds. Cache discounts affect price, not context capacity.

## 9. Cut-point selection

Compaction operates on the active context returned by the session, not on abandoned branches or unsupported Otto v1 files.

The selector walks backward from the newest context-visible message until it has retained approximately `keep_recent_tokens`.

Invariants:

- The latest user request is always retained.
- A retained suffix never starts with a tool result.
- Every retained assistant tool call has all corresponding tool results in protocol order.
- A cut never leaves an unresolved tool call in either the summary source or retained context.
- Context-only metadata adjacent to a cut follows Pi-compatible ordering rules.
- If the retained suffix alone cannot fit, compaction reports that the current turn is too large instead of looping.

Valid primary cut points are user-like or assistant messages, never tool results.

### 9.1 Split turns

A long coding turn may contain many assistant/tool iterations. Requiring every cut to return to its first user message can retain most of the oversized context. Therefore Otto follows Pi's split-turn strategy.

When the selected suffix starts inside a turn:

1. Generate or update the normal historical summary for content before that turn.
2. Generate a smaller turn-prefix summary for the omitted beginning of the current turn.
3. Combine them with an explicit `Turn Context (split turn)` boundary.
4. Retain the selected assistant/tool suffix verbatim.

The turn-prefix summary records the original request, early progress, and information needed to understand the retained suffix.

## 10. Summary generation

### 10.1 Request shape

Compaction uses the active runtime's provider client, model, and thinking setting. It does not introduce a second model configuration.

The summary request:

- uses a dedicated stable summarization system prompt;
- exposes no tools;
- serializes source messages into one labeled transcript treated as untrusted data;
- does not stream summary prose into the user transcript;
- honors cancellation and the provider's existing transient retry rules;
- does not add nonstandard generic cache-control fields.

Serializing transcript data prevents the model from interpreting the old final user message as a new request to execute.

Labels distinguish user text, assistant text, assistant tool calls, and tool results. Each tool result is truncated to 2,000 UTF-8-safe characters in this one-off summary input and receives an explicit truncation marker. Full tool output remains in its original session entry.

### 10.2 Structured output

The initial summary prompt requires exactly these semantic sections:

```text
## Goal
## Constraints & Preferences
## Progress
### Done
### In Progress
### Blocked
## Key Decisions
## Next Steps
## Critical Context
```

It asks for concise continuation state and exact preservation of file paths, function names, commands, test results, and error messages. Validation requires every listed heading exactly once and in the listed order; text within sections remains model-authored Markdown.

Repeated compaction provides the previous structured summary separately and instructs the model to preserve still-relevant facts, incorporate new work, move completed items, update blockers, and replace stale next steps.

Manual `/compact <focus>` appends bounded focus instructions to this control prompt. Transcript text cannot replace the system instructions; it remains tagged as data.

### 10.3 Deterministic file metadata

Otto independently derives file operations from structured tool calls:

- `read` contributes a read file.
- `write` and `edit` contribute modified files.
- `grep`, `find`, and `ls` do not imply that every observed path was read.
- Arbitrary `bash` effects are not guessed.

Previous checkpoint details are unioned with newly summarized operations. Modified paths are retained before read-only paths when applying bounds. Lists are deduplicated and sorted, then limited to 1,024 total paths and 64 KiB of encoded path text; omitted counts are recorded as numeric detail fields rather than fake path entries. The retained lists are included in Pi `details` and appended to the summary in Pi-compatible `<read-files>` and `<modified-files>` blocks.

### 10.4 Validation

Before persistence, Otto requires:

- nonempty output after trimming;
- valid UTF-8;
- no tool call in the summary response;
- a bounded structured summary;
- successful existing secret/error redaction;
- a valid retained suffix and `firstKeptEntryId`.

An empty, malformed, tool-calling, or oversized response is a failed compaction and does not mutate session state.

## 11. Trigger modes

### 11.1 Manual

Both frontends support:

```text
/compact
/compact focus on the authentication refactor
```

Manual compaction:

- starts only while the controller is idle;
- may be canceled;
- does not submit or continue a user turn afterward;
- works for memory-only sessions;
- returns a typed no-op result when there is no meaningful prefix to summarize;
- does not depend on a known catalog limit.

### 11.2 Proactive automatic

Before every provider call, including calls after tool execution, the agent estimates the complete request.

If it exceeds the soft trigger:

1. Attempt one proactive compaction for the active turn.
2. Rebuild and re-estimate context after success.
3. Continue the same turn if the new request fits.
4. Fail clearly if the retained current work alone cannot fit.

Only one proactive attempt occurs per active user turn. This prevents repeated failed summary calls inside a tool loop. A later user turn may try again.

Checking immediately before provider dispatch avoids paying for compaction at the end of a session that never receives another prompt. The triggering user message or tool result has already been durably appended before this check. Cancellation or compaction failure leaves that normal interrupted-turn record in place; Otto never rolls it back.

### 11.3 Reactive overflow

If the provider returns a typed context-overflow error before visible output, the failed provider attempt has no assistant record to remove. Otto then:

1. Prepares compaction from the still-valid persisted context.
2. Compacts once.
3. Retries that provider call once with the rebuilt context.

A single failing provider call cannot enter a compaction loop. If its retry also overflows, Otto returns a clear terminal error. A later provider step in the same tool-driven run may independently recover if new tool output causes a distinct overflow.

Otto never treats a normal `finish_reason: length` as context overflow. That finish reason generally means output truncation, and discarding a visible response would risk duplicated work.

An overflow reported after visible stream output is not automatically retried because the frontend may already have displayed non-idempotent text.

## 12. Failure and cancellation policy

### 12.1 Soft-boundary failures

When a proactive attempt was triggered only by a cost-aware soft boundary and the request remains below the hard safety boundary:

- cancellation still stops the turn;
- a non-cancellation summary failure emits a bounded warning;
- Otto attempts the original provider request once;
- no checkpoint is appended.

This prevents an optional cost optimization from making an otherwise valid request unavailable.

### 12.2 Hard-boundary failures

Near or beyond the hard safety boundary, a failed compaction aborts the provider step. Otto does not knowingly send a request expected to overflow.

Reactive recovery failure returns the original overflow context plus a concise compaction failure classification without embedding an unbounded provider body.

### 12.3 Cancellation boundary

Before durable append, cancellation leaves the old active context unchanged.

The append operation checks cancellation before writing. Once the complete checkpoint write begins, it runs as one durable operation. If append and sync succeed, the checkpoint is committed even if cancellation arrives immediately afterward; the turn then stops before another provider call.

This prevents returning a cancellation result that falsely claims a successfully committed checkpoint did not happen.

## 13. Provider overflow classification

`internal/provider` exposes a typed `ErrContextOverflow` identity. The OpenAI-compatible adapter constructs it only from bounded non-success responses, primarily HTTP 400, 413, or 422.

Recognized evidence includes allowlisted structured codes/types such as:

```text
context_length_exceeded
context_window_exceeded
max_context_length
```

and narrowly matched messages such as “maximum context length” or “input tokens exceed the context window.” Generic `max_tokens` validation errors are not enough on their own because they may describe an invalid output setting rather than oversized input.

Classification requirements:

- Parse only the already bounded error body.
- Redact credentials and authorization values first.
- Preserve `errors.Is`/`errors.As` identity.
- Optionally retain parsed current/maximum token counts as integers.
- Do not expose raw JSON or endpoint userinfo.
- Do not apply the normal transient HTTP retry loop to deterministic overflow errors.

## 14. Session checkpoint transaction

### 14.1 Session contract

The neutral session interface gains one append operation and one read-only metadata view equivalent to:

```go
AppendCompaction(ctx, checkpoint)
LatestCompaction() (metadata, bool)
```

`LatestCompaction` exposes only provider-neutral summary/details from the latest active checkpoint. It lets repeated compaction preserve deterministic file metadata, including from an external Pi checkpoint, without leaking Pi wire structs into the agent package.

The append checkpoint contains:

```text
summary
first kept message/entry ID
tokens before compaction
summary provider usage
bounded read/modified file details
```

The agent selects content; the session implementation validates identity, active-path membership, protocol pairing, encoding, size, and persistence.

### 14.2 Pi v3 record

Otto writes the Pi 0.84.3-compatible legacy checkpoint shape:

```json
{
  "type": "compaction",
  "id": "...",
  "parentId": "...",
  "timestamp": "...",
  "summary": "...",
  "firstKeptEntryId": "...",
  "tokensBefore": 258000,
  "details": {
    "readFiles": ["..."],
    "modifiedFiles": ["..."]
  },
  "usage": {"...": "Pi usage"}
}
```

Otto continues to read newer `retainedTail` checkpoints, but its own writer uses `firstKeptEntryId` because that is consumed by the installed Pi 0.84.3 `SessionManager` implementation.

The first-kept ID must be an actual prior entry on the active path. Repeated Otto compaction therefore remains valid without synthetic retained-tail IDs.

A retained-tail-only checkpoint loaded from another harness is a special case because its materialized tail messages have no standalone Pi entry IDs. On the next Otto compaction, all such synthetic tail messages are summarized and the kept suffix starts at the first genuine message entry appended after that checkpoint, even when this retains fewer than the target 20K tokens. If no genuine post-checkpoint message exists, manual compaction returns a no-op and automatic compaction waits for one. Otto never writes a synthetic ID into `firstKeptEntryId`. When an external checkpoint contains both a valid active-path `firstKeptEntryId` and `retainedTail`, Otto may use the real entry path to preserve IDs.

### 14.3 Durable order

For persistent sessions:

1. Validate the complete candidate checkpoint and rebuilt context in memory.
2. Encode it and enforce record/session size limits.
3. Append one complete JSONL record.
4. `fsync` the pinned session descriptor.
5. Update entries, leaf ID, active messages, aggregate usage, and file-byte accounting in memory.
6. Emit completion.

No old record is modified or deleted.

A write or sync error poisons the current store with the existing fatal persistence identity. In-memory context is not advanced. On reopen, a complete final record is treated as committed; an incomplete final record is repaired using existing tail-recovery behavior.

Memory sessions perform the same candidate validation and context replacement without disk I/O.

## 15. Usage and cache accounting

### 15.1 Model usage semantics

`model.Usage` retains these semantics:

```text
InputTokens       total provider prompt tokens
OutputTokens      total provider completion tokens
CachedInputTokens subset of InputTokens served from cache
```

Cached input is never added to total input a second time.

### 15.2 Pi conversion

For newly persisted usage:

```text
Pi input      = InputTokens - CachedInputTokens
Pi cacheRead  = CachedInputTokens
Pi cacheWrite = 0 for current OpenAI-compatible responses
Pi output     = OutputTokens
Pi total      = InputTokens + OutputTokens
```

When reading external Pi usage, Otto reconstructs total prompt input from uncached input plus cache-read/cache-write components. `CachedInputTokens` reflects cache reads, not cache writes.

Existing Otto session records with all prompt tokens in Pi `input` and zero `cacheRead` remain valid.

### 15.3 Summary usage

Successful summary calls are usage-bearing provider calls when the compatible endpoint supplies nonzero usage. Supplied usage is:

- persisted once on the compaction entry;
- added once to aggregate session totals;
- emitted once through `CompactionCompleted`, not duplicated through a second usage event;
- not attached to a normal assistant transcript message;
- never used as the next normal-context estimation anchor.

Some compatible endpoints omit streaming usage despite `stream_options.include_usage`. Otto does not fabricate provider usage and does not fail an otherwise valid summary; the checkpoint simply omits its usage field. Split-turn compaction combines all supplied summary-call usages with overflow-safe addition.

### 15.4 Cache behavior

Compaction intentionally changes the message prefix and normally causes one cache miss. Otto minimizes this cost by:

- keeping stable system instructions and tool ordering;
- triggering at a high-water mark;
- reducing active history to a structured summary plus roughly 20K recent tokens;
- avoiding repeated compaction attempts in one turn;
- leaving the new prefix stable for subsequent append-only requests.

The one-time miss is justified only when future uncached/cached input savings exceed the summary and cache-reset cost. The cost-aware GPT boundary enforces this for documented long-context premium tiers.

## 16. Controller lifecycle and concurrency

The shared runner/backend contract gains:

```text
Compact(ctx, instructions, emit) -> CompactionResult
```

`CompactionResult` contains bounded metadata such as reason, tokens before, estimated tokens after, and summary usage; it does not carry the full summary through frontend worker messages.

The controller treats prompt and manual compaction as the same class of active operation. These operations remain mutually exclusive:

- `Prompt`
- `Compact`
- `NewSession`
- `ResumeSession`
- `Close`

Requirements:

- A second active operation returns the existing busy identity.
- `Close` waits for active prompt or compaction completion.
- Cancellation cannot deadlock replacement or close.
- Reentrant close from an event callback follows the controller's existing deadlock-safe behavior.
- A runner/session pair captured for an operation cannot be replaced underneath it.
- In-process resume and `/new` construct runners with the correct resolved catalog/override limits.

## 17. Agent events

Add bounded typed events for:

```text
CompactionStarted
CompactionCompleted
CompactionWarning
```

Metadata includes only bounded fields:

```text
checkpoint ID after success
reason: manual | threshold | overflow
tokens before
estimated tokens after
automatic flag
summary usage
bounded error identity/message for warnings
```

The full structured summary is never placed in an event. Frontends obtain persisted history through the backend when needed. Existing text/tool events retain their semantics.

Automatic compaction occurs inside `Run`; manual compaction uses the same event types through `Backend.Compact`.

## 18. Frontend behavior

### 18.1 Command parsing

Both TUI and REPL accept:

```text
/compact
/compact <focus instructions>
```

The instruction tail is trimmed, control-safe, and limited to 8 KiB. `/compactly` is not interpreted as `/compact`.

TUI slash suggestions include `/compact`; suggestions close once free-form focus text begins. Existing exact commands retain their behavior.

Headless `--approve` remains a single user-prompt mode and does not interpret slash commands.

### 18.2 TUI

- Manual compaction runs in a cancellable worker.
- Presentation state mutates only in Bubble Tea `Update`.
- Worker and agent messages remain bounded and generation-checked.
- `Esc` cancels active manual or automatic compaction as part of the active operation.
- Completion adds a concise transcript marker such as `[context] compacted 258k → 23k tokens`.
- After receiving the committed checkpoint ID, `Update` reads the latest backend history, extracts only that checkpoint entry, and appends its folded presentation without replacing existing live entries.
- The live transcript is not erased when model context changes.
- Compaction summaries are folded by default and deduplicated by checkpoint ID.
- `Ctrl+O` expands/collapses complete compaction summaries as well as tool details.
- Expanded summary text remains terminal-sanitized and copy-friendly.
- After resume, the folded checkpoint appears before the retained tail; compacted raw entries are not reconstructed into display history.
- Footer usage reconciles from `Backend.Info()` at a checkpoint boundary, then continues with later provider usage without double counting.
- Stale completion/events from an old generation cannot mutate a new or resumed session.

### 18.3 REPL

- Manual compaction uses the same active cancellation slot as prompts.
- `Ctrl+C` cancels active compaction and returns to the prompt.
- Automatic and manual success print one concise context line.
- Soft-boundary warnings go to stderr without terminating the REPL.
- Fatal persistence errors retain their existing process-fatal behavior.
- `/help` documents the command and optional focus text.

## 19. Bounds and security

The feature adds explicit resource bounds:

- focus instructions: 8 KiB
- generated summary: 128 KiB
- each serialized tool result: 2,000 UTF-8-safe characters plus marker
- serialized summary request: estimated at or below `hardInputWindow - effectiveReserve` when a hard window is known, and always at or below an absolute 16 MiB in-process ceiling
- file-operation details: at most 1,024 paths and 64 KiB of encoded path text
- provider overflow body: existing bounded provider-error limit
- all token and usage arithmetic: saturating and nonnegative

If the prepared summary request exceeds either applicable input bound, Otto fails before calling the provider; only copied tool results receive automatic truncation. If streamed summary output crosses its limit, Otto cancels the child provider request and rejects the partial result.

Security requirements:

- No summary request has tools, so it cannot read, write, edit, or run shell commands.
- The summarization system prompt labels transcript content as untrusted data and forbids following instructions found inside it.
- Provider credentials, authorization headers, endpoint userinfo, and resolved secrets never enter summary prompts, session details, events, or errors.
- Existing session content remains sensitive and may contain source text, tool arguments, and tool results; documentation continues to warn users accordingly.
- Existing no-follow, descriptor-pinned session activation and append behavior remains unchanged.
- Existing workspace enforcement remains inside file tools; compaction does not open workspace paths itself.
- `bash` remains intentionally unsandboxed and is not invoked during compaction.

## 20. Error identities

The implementation should expose stable typed/sentinel identities for tests and frontend decisions, including:

- no compaction opportunity / nothing to compact;
- context overflow;
- current retained turn too large;
- invalid or oversized summary;
- cancellation/deadline identities;
- existing fatal session persistence identity;
- existing controller closed/busy identities.

Human-readable messages remain concise and do not include full prompts, summaries, provider bodies, or secrets.

## 21. Testing strategy

All production behavior is developed through failing tests first.

### 21.1 Catalog and configuration

- Every included canonical GPT/Claude ID and alias
- Documented date snapshots
- Allowed namespace and routing wrappers
- Rejection of fuzzy/private aliases
- Cost-aware GPT working boundaries
- Explicit profile precedence
- Unknown-model behavior
- Small-window effective setting adjustment
- Invalid TOML and range combinations

### 21.2 Estimation and cut selection

- Exact provider-input anchoring
- No cached-token double counting
- Persisted assistant content rather than hidden reasoning output
- Full fallback estimate including system/tools
- UTF-8, CJK, code, and JSON-heavy content
- Saturation at integer limits
- Tool-call/result atomicity
- Multiple tool calls in one assistant message
- Split-turn selection and summary composition
- Latest user retention
- Huge current user message failure
- Repeated compaction over a previous summary

Property-style table tests should assert that every generated retained context passes the existing pending-tool-call validator.

### 21.3 Agent integration

Use fake providers and `httptest` only:

- Manual compaction and focus text
- Proactive threshold compaction
- No trigger below threshold
- Tool-loop preflight compaction
- Soft-boundary summary failure followed by a safe normal call
- Hard-boundary summary failure with no normal call
- Overflow compact-and-retry exactly once
- Overflow retry failure
- No retry after visible output
- Unknown model manual and reactive paths
- Empty, malformed, tool-calling, and oversized summary responses
- Summary transient retry and cancellation
- Summary requests contain no tools and truncate only copied tool output
- Summary usage emission exactly once

### 21.4 Session durability and interoperability

- Exact Pi v3 compaction JSON
- Valid active-path `firstKeptEntryId`
- `tokensBefore`, details, and usage
- Cached usage round-trip
- Aggregate usage without duplication
- Memory/store parity
- Lazy file creation
- Repeated compaction followed by resume
- Follow-on Otto compaction after external retained-tail-only and dual-form checkpoints
- No synthetic `firstKeptEntryId` emission
- Context equality before close and after reopen
- Write, short-write, and sync failure behavior
- Complete and incomplete crash tails
- Existing external `retainedTail` fixtures
- Existing Otto v1 rejection and preservation
- Optional Pi 0.84.3 Node interoperability probe

Default Go tests never require Node, Pi, credentials, a real endpoint, or network access.

### 21.5 Controller and frontend

- Prompt/compact/new/resume/close mutual exclusion
- Cancellation and reentrant-close races
- Runner replacement isolation
- TUI command filtering and focus parsing
- Bounded generation-checked worker messages
- Folded summary rendering and `Ctrl+O`
- Live transcript preservation
- Usage reconciliation
- REPL command output and interrupt behavior
- Offline PTY lifecycle and terminal restoration

### 21.6 Completion gates

Before completion:

```bash
go test ./...
go test -race ./...
go vet ./...
go run honnef.co/go/tools/cmd/staticcheck@latest ./...
go build -trimpath -o ./otto ./cmd/otto
go test ./cmd/otto -run TestTUIPseudoTerminalLifecycle -count=1
test -z "$(gofmt -l .)"
git diff --check
```

Focused compaction, session, controller, and PTY tests also run repeatedly and under race detection where applicable.

## 22. Documentation

README updates will document only implemented Stage 1 behavior:

- automatic compaction defaults;
- GPT/Claude catalog scope through OpenAI-compatible endpoints;
- the fact that this is not Claude subscription/provider support;
- `/compact [focus]` in TUI and REPL;
- optional profile overrides;
- cost-aware GPT behavior;
- append-only checkpoint and sensitive-session behavior;
- prompt-cache reset trade-offs;
- concise frontend markers and `Ctrl+O` summary expansion.

Command examples must match the final CLI exactly. No Codex or Claude subscription functionality is described as working.

## 23. Implementation boundary

This design is one architectural feature and can proceed as one implementation plan, but the plan must sequence small TDD tasks across:

1. model limits and configuration;
2. usage persistence and estimation;
3. pure cut-point and summary preparation;
4. session checkpoint append;
5. manual compaction core;
6. proactive and overflow orchestration;
7. controller lifecycle;
8. REPL and TUI behavior;
9. interoperability, documentation, and final verification.

No implementation begins until this written specification is reviewed and explicitly approved.
