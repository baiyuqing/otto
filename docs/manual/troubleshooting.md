# Troubleshooting

Previous: [Memory and extensions](memory-and-extensions.md) · [Documentation index](../README.md)

Restart the process actually doing work after changing configuration,
credentials or installed extensions, including `otto serve`/ACP rather than
only its attaching client. A timeout or lost response does **not** prove an
external operation did nothing: inspect state before repeating a write.

### `missing api key for provider "<name>"`

Otto still starts without a key; this error appears when a turn reaches a
Claude or OpenAI-compatible provider that has none, and nothing is sent. Either
switch to a model whose provider has credentials (`/model`, or `--model` on the
next start), or set the active provider's `api_key_env` variable privately in the runtime's
environment, then restart. For Claude this is normally `ANTHROPIC_API_KEY`; if
the key is not scoped to one workspace, also set the variable named by
`workspace_id_env`. When using `otto-connect`, change the environment of the
`otto acp` child or existing `otto serve` process that performs model requests,
not the chat connector's config. Use `otto config explain --json` to check the
active provider and its source without resolving credentials. Never paste a key into
chat.

### `otto: missing base_url`, `otto: invalid base_url`, or request failures

Check the selected provider, `--base-url` and endpoint path. Claude requests use
`<base-url>/v1/messages` and require the configured `anthropic-version`; the
default base URL is `https://api.anthropic.com`. OpenAI-compatible requests go
to `<base-url>/chat/completions`; confirm the endpoint supports streaming chat
completions. HTTP 401/403 usually means the key, workspace binding, or account
access is wrong; HTTP 429 may require waiting for the provider's retry window.
A server/container may need a different hostname than the attaching terminal
(`host.docker.internal` reaches the Mac host from Docker Desktop).

### `read chat completion stream: ...` or stream ended without `[DONE]`

Check SSE support, streaming enablement, proxy buffering/rewriting and idle
connection limits. Transient transport failures and model output idle
timeouts retry up to three times with jittered backoff (upper bounds 1, 2,
4 s) within the turn deadline, before
or after a text/reasoning/tool delta; a retry after a delta discards the
partial output and repeats the generation, which the endpoint bills again.
HTTP 429 and 503 retry before any delta; a numeric `Retry-After` replaces
backoff, capped at 30 s and bounded by the remaining deadline. Other HTTP errors and
malformed events are not retried. A sent request may have started remotely
even if no output arrived; inspect effects before resubmitting.

### `warning: bash is unavailable because the configured sandbox could not be established ...`

On macOS, `auto` means Seatbelt; confirm `/usr/bin/sandbox-exec` is usable.
Narrow `read_paths` and `deny_read_paths`, and move a workspace away from private
sandbox cache/state if entries overlap it. A `deny_read_paths` entry that equals or
contains the workspace or a linked worktree prevents the sandbox from starting.
Prefer exact exceptions over broad home/cache access.

Linux has no confined shell driver (`unsupported-platform`). Otto never falls
back to Docker or unconfined execution. `--sandbox off`/`driver = "off"` is an
explicit grant of unrestricted current-user shell access, not a routine fix.
File tools remain available. See [platform support](getting-started.md#platform-support).

### `no chatgpt credentials; run 'otto login'`

Run `otto login` in the runtime host/account, or `otto login --status` to inspect
sign-in. Restart the affected runtime. An attaching client's login does not
authenticate a remote server. See [ChatGPT subscription](getting-started.md#chatgpt-subscription).

### `/mcp` reports `failed: ...` or `needs login`

Inspect the exact `/mcp` reason. Check command/interpreter, HTTP reachability,
handshake/listing timeout and secret-redaction limits. A failed server contributes
no tools but does not disable other servers. For OAuth, run `/mcp login <server>`
or `otto mcp login <server>`. **Restart** to connect with changed declarations
or tokens; the running process does not reconnect merely because setup succeeded.
See [MCP servers](memory-and-extensions.md#mcp-servers).

### Context-length or prompt-size failures

Otto attempts automatic compaction before a known hard limit and one recovery
checkpoint for a typed context overflow. If still failing:

- `/compact [focus]` creates a manual checkpoint; `/new` starts fresh.
- Reduce large pasted inputs/tool output.
- Set `context_window`/`compaction_window` on the model entry
  (`models."<model_id>" = { context_window = N, compaction_window = M }` in
  `[providers.<name>]`) for private/unknown models, or select a model with a
  larger window.

See [context compaction](interactive-use.md#context-compaction); compaction does
not erase the append-only session history.

### A session disappeared from `/resume` or `--continue`

Check whether it was archived. Active pickers exclude `archive/`, but an explicit
path still resumes it for the matching workspace:

```bash
otto --cwd /path/to/project --resume ~/.otto/sessions/<key>/archive/<session-id>.jsonl
```

Archive preserves the file and does not reclaim space. `--continue` also skips
invalid/unsupported session formats. Check workspace identity before assuming
history was deleted; see [Sessions](interactive-use.md#sessions).

### A session takes too long to start

- Startup loads configuration and local state before contacting a provider.
  Use `otto config explain --cwd DIR --json` to diagnose source/trust problems
  offline.
- Terminal MCP connection is asynchronous; `/mcp` shows progress. Headless and
  server first turns wait for enabled servers within `connect_timeout_secs`.
  Disable an unused server or repair its command/network rather than increasing
  timeouts blindly.
- A lease-managed session held elsewhere cannot open concurrently. A lost host
  can require 7/6 of `lease_seconds` before takeover (35 s by default). See
  [continuing on another host](interactive-use.md#continuing-a-session-on-another-host).
- Inspect private execution diagnostics under `~/.otto/logs/` for observed
  phases, retries and heartbeats. Do not interpret network activity as proof
  that a tool/provider is making useful progress. See [logging](servers-and-connectors.md#logs-metrics-and-request-correlation).

### Startup fails with `[memory.remote] was removed` or `memory backend "remote" was removed`

The remote memory backend no longer exists. Delete `[memory.remote]` and
`backend = "remote"` from `config.toml`. To use an external memory service,
install it as a plugin package and set `backend = "plugin:<package>"`; see
[plugin memory source](memory-and-extensions.md#plugin-memory-source).

### Claude reports `invalid Claude stream sequence`

Otto rejected an invalid Claude message lifecycle or block/delta pairing. The
error suffix identifies the event and violated rule without response text, tool
names or arguments. Include the complete error and session ID when reporting an
issue; HTTP 200 alone does not establish that the stream completed successfully.

Message-level updates may occur multiple times and interleave with content
blocks. They update cumulative usage and stop metadata without closing the
blocks. An explicit `message_stop` completes the response and finalizes blocks
that did not receive `content_block_stop`, matching the official SDK's
accumulation behavior. Streamed text and tool arguments remain provisional until
then. Socket EOF alone does not complete a response.

Tool input must be one complete JSON object. A block stop closes the block and
retains any validation failure until final message metadata is available.
Malformed, truncated or duplicate-key arguments never become executable calls;
Otto never replaces them with empty arguments. Empty input fragments leave the initial
input object unchanged. Streamed and initial arguments preserve numeric literals.

If an older binary reports `message_delta: content block still open`, update and
restart the process performing the model request. That check incorrectly treated
message metadata as a content-completion barrier. See the official
[streaming protocol](https://platform.claude.com/docs/en/build-with-claude/streaming)
and [SDK accumulator](https://github.com/anthropics/anthropic-sdk-go/blob/main/messageutil.go).

### Claude reports `malformed Claude tool input`

The error identifies `incomplete_json`, `invalid_json`, `duplicate_key` or
`non_object`, with the block index, parser line/column, normalized stop reason,
reported output token count and request `max_tokens`. No argument text, keys,
values or raw parser messages are included. An unknown upstream stop reason
appears as `unknown`. A missing `message_stop` remains an incomplete stream,
not a confirmed token-limit failure.

For known Claude models, requests start at the smaller of 16,384 output tokens
and the [canonical model limit](../../crates/otto-core/src/config/model_limits.rs).
The current Fable 5.1, Opus 5.5, Sonnet 5.5 and Haiku 5.5 aliases resolve to their
published 128,000-token output ceiling. Unknown models keep a conservative 4,096
budget and do not grow automatically; no limit is guessed from a name prefix.

Only a completed `max_tokens` response with incomplete tool JSON can trigger
recovery. Each retry doubles the budget up to the known model ceiling. This
shares the existing four-attempt limit and operation deadline with transport and
HTTP retries, reports a retry event and discards provisional output. Regeneration
is billed again. Cancellation stops recovery; no retry is made if the budget
cannot grow or the remaining operation time is insufficient.

Complete syntax errors, duplicate keys, non-object arguments and incomplete
arguments with other stop reasons fail without automatic retry. Invalid inputs
are never repaired or passed to tools. Include the full content-free error and
session ID when reporting repeated failures. See the official
[truncated tool-use guidance](https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons#incomplete-tool-use-blocks).

## Provider endpoint is not trusted

This is a local policy error, not a provider/API-key failure. No rejected request
is sent. Existing custom endpoints are not grandfathered after an upgrade.
Independently verify the service and add its exact origin to the relevant
provider's `trusted_origins` in **user or managed** configuration; HTTP also
needs `allow_http_origins`. Project/model settings and `--base-url` cannot grant
trust. Follow [endpoint trust](getting-started.md#endpoint-trust), preserve model
entries, then restart Otto. Do not paste credentials into a URL, grant, or prompt.
If setup rejects a custom URL, configure the provider and model manually using
that guide; setup never converts URL input into trust. For redirects, use the
verified final endpoint directly. Metadata/link-local targets cannot be enabled.

Provider requests deliberately ignore environment/system proxy settings to keep
DNS endpoint checks enforceable. If a provider is reachable only through a proxy,
use a directly reachable independently trusted endpoint instead; proxy injection
is not supported. DNS answer sets containing metadata/link-local addresses fail
closed even when other answers are allowed.
