# Troubleshooting

Previous: [Memory and extensions](memory-and-extensions.md) · [Documentation index](../README.md) · Next: —

## Troubleshooting

### `otto: missing api key`

Export the environment variable named by the selected profile's `api_key_env`,
or set `OTTO_API_KEY` as a fallback.

### `otto: missing base_url`, `otto: invalid base_url`, or request failures

Check the selected profile, `--base-url`, and endpoint path. Otto posts to
`<base-url>/chat/completions`.

### `read chat completion stream: ...` or stream ended without `[DONE]`

The provider or proxy did not deliver valid SSE chat-completions output. Otto
retries a transport interruption — `connection reset by peer`, a connection
closed before the message completed, a broken pipe, or a stalled read — three
times, 1, 2 and 4 seconds apart, while no part of the response has arrived and
the turn deadline allows it; the status line shows each retry. This message
therefore means the interruption kept recurring, or it happened after part of
the response had already been shown, which cannot be repeated. Check that
streaming is enabled, that SSE is not buffered or rewritten, and that a proxy
or load balancer is not dropping long-lived connections.

### `warning: bash is unavailable because the configured sandbox could not be established ...`

On macOS, `auto` means Seatbelt. On Linux there is no confined driver at all,
so this warning names `unsupported-platform` and `--sandbox off` is the only
way to run commands (see [platform support](getting-started.md#platform-support)). Otto does not
fall back to Docker or direct execution unless you explicitly choose
`--sandbox off` (or `driver = "off"` in config). Common fixes:

- confirm `/usr/bin/sandbox-exec` is present and usable;
- narrow `read_paths` so they do not include Otto's private sandbox cache root;
- move the selected workspace outside cache-like locations if it would overlap
  Otto's private sandbox state;
- prefer narrow `read_paths` plus exact config variables over broad home or
  cache access;
- use `--sandbox off` only if you accept unsandboxed current-user execution.

### `no chatgpt credentials; run 'otto login'`

The `chatgpt` provider has no stored OAuth credentials. Run `otto login` to sign
in with your ChatGPT subscription, or check state with `otto login --status`.
See [ChatGPT subscription](getting-started.md#chatgpt-subscription).

### `/mcp` reports `failed: ...` or `needs login`

Check `/mcp` for the exact reason. `failed: <reason>` means the stdio command
could not be spawned, the HTTP connection, handshake, or tool listing did not
complete within `connect_timeout_secs`, or the server's `env`/`headers`
secrets exceed the redaction limits; the server contributes no tools until
Otto is restarted with the problem fixed. `needs login` means the server
requires OAuth and has no valid stored token: run `/mcp login <server>` (or
`otto mcp login <server>`), then restart Otto. See
[MCP servers](memory-and-extensions.md#mcp-servers).

### Context-length or prompt-size failures

Otto tries one automatic checkpoint before the hard limit (when it knows the
model window) and one typed-overflow recovery checkpoint after a provider
context error. If you still hit a hard input limit:

- run `/compact [focus]` to create a manual checkpoint,
- use `/new` for a completely fresh session,
- reduce large pasted input or large tool output,
- set profile `context_window` / `compaction_window` for private or unknown
  model IDs,
- or choose a model/profile with a larger working window.

### A session disappeared from `/resume` or `--continue`

The session was likely archived. Archive moves the file (not deletion) into
`~/.otto/sessions/<workspace-key>/archive/<session-id>.jsonl`. To reopen it:

```bash
./otto --cwd /path/to/project --resume ~/.otto/sessions/<key>/archive/<session-id>.jsonl
```

The file is still intact; only the active-session surfaces (`/resume`,
`--continue`, and the `/archive` picker) exclude it.

### A session takes too long to start

Set `OTTO_STARTUP_TRACE=1` (`true`, `yes`, and `on` also work) to print one
line per startup phase to stderr, ending with the total time to a usable
prompt:

```bash
OTTO_STARTUP_TRACE=1 otto --cwd /path/to/project
```

```text
startup environment: 0ms
startup config/load: 1ms
startup sandbox/open: 214ms
startup memory/open: 3ms
startup session/activate: 0ms
startup runner/setup: 0ms
startup runner/catalogs: 2ms
startup runner/mcp: 0ms
startup runner/provider: 1ms
startup runner/workspace-context: 41ms
startup runner/subagents: 0ms
startup runner/registry-and-options: 0ms
startup runner/build: 44ms
startup ready: 265ms
```

Each line is the time that phase took. The `runner/*` lines break the runner
build down, and `runner/build` is their total; `startup ready` is the whole
elapsed time before the frontend starts. `runner/mcp` is near zero in an
interactive session because MCP servers connect in the background after the
prompt is usable (see [MCP servers](memory-and-extensions.md#mcp-servers)), and
`runner/workspace-context` includes the `git` calls the workspace header
makes.

### `warning: memory service token variable OTTO_MEM_TOKEN is unset or empty; continuing without memory`

`[memory] backend = "remote"` is selected and the token variable is not in the
environment. Export the variable named by `[memory.remote] token_env` (default
`OTTO_MEM_TOKEN`) in the shell that starts Otto. Otto runs without memory for
the process; with `[memory] required = true` startup fails with
`memory service token variable OTTO_MEM_TOKEN is unset or empty`.
See [Remote memory service](memory-and-extensions.md#remote-memory-service).

### `memory service unavailable` with the remote backend

A recall or write to the memory service failed: no connection, a timeout, a
non-2xx status, a response without the `Otto-Memory-Protocol: 1.<minor>`
header, or a recall that is not `complete` or breaks a
[protocol rule](remote-memory-protocol.md). Otto continues the turn without
memory. The `memory_request_failed` line on the stderr of `otto serve` or
`otto acp` names the endpoint, status and code (see
[Remote memory log lines](memory-and-extensions.md#remote-memory-log-lines)).

---

Previous: [Memory and extensions](memory-and-extensions.md) · [Documentation index](../README.md) · Next: —
