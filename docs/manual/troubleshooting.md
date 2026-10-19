# Troubleshooting

Previous: [Memory and extensions](memory-and-extensions.md) · [Documentation index](../README.md)

## Troubleshooting

Restart the process actually doing work after changing configuration,
credentials or installed extensions, including `otto serve`/ACP rather than
only its attaching client. A timeout or lost response does **not** prove an
external operation did nothing: inspect state before repeating a write.

### `otto: missing api key`

Set the active profile's `api_key_env` variable privately in the runtime's
environment, then restart. Use `otto config explain --json` to check the active
profile/source without resolving credentials. Never paste a key into chat.

### `otto: missing base_url`, `otto: invalid base_url`, or request failures

Check the selected profile, `--base-url` and endpoint path. OpenAI-compatible
requests go to `<base-url>/chat/completions`; confirm the endpoint supports
streaming chat completions. A server/container may need a different hostname
than the attaching terminal (`host.docker.internal` reaches the Mac host from
Docker Desktop).

### `read chat completion stream: ...` or stream ended without `[DONE]`

Check SSE support, streaming enablement, proxy buffering/rewriting and idle
connection limits. Before any text/reasoning/tool delta, eligible transport
failures retry three times (1, 2, 4 s) within the turn deadline. Failures after a
delta, HTTP errors and malformed events are not retried. A sent request may have
started remotely even if no output arrived; inspect effects before resubmitting.

### `warning: bash is unavailable because the configured sandbox could not be established ...`

On macOS, `auto` means Seatbelt; confirm `/usr/bin/sandbox-exec` is usable.
Narrow `read_paths` and move a workspace away from private sandbox cache/state
if grants overlap it. Prefer exact grants over broad home/cache access.

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
- Set profile `context_window`/`compaction_window` for private/unknown models,
  or select a profile/model with a larger window.

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

### Remote memory is disabled because the token variable is unset or empty

Set the configured `memory.remote.token_env` variable in the environment of
`otto serve`, ACP or the standalone runtime and restart. Attaching does not
forward the client's environment. `required = true` makes this fatal rather
than continuing without memory. Startup performs no service readiness probe.

### `memory service unavailable` with the remote backend

Inspect endpoint/status/code in runtime logs, then verify service reachability,
token/delegation, organization/project membership and schema compatibility.
Recall fails open **without memory**, not with another tenant or local fallback.
Organization memory is not an outage-proof policy channel. Do not retry an
unknown-outcome write blindly; exact-read state or consult the service first.
See [remote recovery](memory-and-extensions.md#failure-recovery-and-uploads) and
[protocol errors](remote-memory-protocol.md#errors-and-recovery).
