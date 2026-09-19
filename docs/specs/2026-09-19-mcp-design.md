# MCP client design

Status: proposed 2026-09-19; scope confirmed by the user on 2026-09-19
(HTTP transport and OAuth are required; stdio servers run unsandboxed in
this iteration). Branch `feat/mcp`, worktree `.worktree/mcp`.

## Deviations from this design

The implementation departs from this design in three places. The paragraphs
below are marked "(superseded, see Deviations)"; the current behavior is the
one documented in `docs/user-manual.md`.

- **Connect order**: servers connect one at a time, in configuration order,
  not concurrently. A hung server's `connect_timeout_secs` therefore delays
  every server declared after it.
- **stdio restart**: an exited stdio server's process is not restarted. Once
  disconnected, it stays disconnected for the rest of the session; see
  "Not yet implemented" in the user manual's MCP section.
- **stderr handling**: the child's stderr is not drained to the debug log.
  A bounded tail (last few KiB) is kept in memory and folded into the
  `failed: ...` reason reported when the child exits or the connect fails;
  it is never logged on its own.

## Goal

Let Otto call tools exposed by external Model Context Protocol (MCP) servers.
A configured server's tools appear in the tool list alongside the built-in
tools, under a prefixed name, and calls are forwarded over the server's
transport. The server list and connection status are visible through a
`/mcp` REPL command.

Non-goals for this iteration (listed as follow-ups at the end): resources,
prompts, sampling, elicitation, tasks, `notifications/tools/list_changed`
hot reload, the deprecated 2024-11-05 HTTP+SSE transport, per-tool approval
prompts, Client ID Metadata Documents, and exposing MCP tools to the web UI
beyond what the shared runner already provides.

## Protocol scope

Two protocol eras are in use on real servers, and a client that speaks only
one fails against roughly half of them.

| Era | Versions | Handshake | Per-request framing |
|-----|----------|-----------|---------------------|
| legacy | 2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25 | `initialize` request, then `notifications/initialized` | none |
| modern | 2026-07-28 | none (stateless) | `params._meta["io.modelcontextprotocol/protocolVersion"]`, `params._meta["io.modelcontextprotocol/clientCapabilities"]`, optional `clientInfo` |

The client implements both:

- stdio: send `server/discover` with modern `_meta`. A result means modern.
  A `-32601` (method not found) error, a `-32020` UnsupportedProtocolVersion
  error whose `data.supported` contains no modern version, or a 5-second
  timeout means legacy; the client then sends `initialize` with
  `protocolVersion: "2025-11-25"`, accepts any version the server answers,
  and sends `notifications/initialized`.
- HTTP: POST `tools/list` with modern headers and `_meta`. HTTP 200 means
  modern. HTTP 400 or a JSON-RPC `-32020`/`-32602` body means legacy; the
  client then runs the legacy `initialize` exchange and keeps the
  `Mcp-Session-Id` response header, if any, for later requests.

The negotiated era is stored per server and reported by `/mcp`.

Only the tools feature is used: `tools/list` (with cursor pagination) and
`tools/call`. `resultType` other than `"complete"` on a modern result is
reported as an error result to the model.

## Configuration

Servers are declared in `config.toml`, following the `[skills]` and
`[agents]` table pattern:

```toml
[mcp]
enabled = true              # default true; false skips every server
call_timeout_secs = 60      # default 60; per tools/call
connect_timeout_secs = 20   # default 20; handshake and tools/list

[mcp.servers.github]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
cwd = "."                   # default: workspace path

[mcp.servers.docs]
transport = "http"
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${DOCS_MCP_TOKEN}" }

[mcp.servers.remote]
transport = "http"
url = "https://remote.example.com/mcp"
auth = "oauth"              # default "none"; "oauth" enables the flow below
oauth_client_id = "otto"    # optional; used when the server offers no dynamic registration
oauth_scopes = ["mcp:tools"] # optional; default: scopes the server advertises

[mcp.servers.legacy]
transport = "stdio"
command = "./bin/legacy-server"
enabled = false             # declared but not started
```

Rules:

- `deny_unknown_fields` on every table, matching the existing config
  contract.
- Server names match `^[A-Za-z0-9_-]{1,32}$`. The name is part of every tool
  name, and `safe_prompt_tool_name` limits tool names to 64 bytes of
  `[A-Za-z0-9_-]`.
- `${VAR}` and `${VAR:-default}` in `env` values, `headers` values, `url`,
  `command`, `args`, and `cwd` expand from the process environment at
  resolution time. An unset variable without a default is a configuration
  error that names the server and key, without printing the value. This is
  the only way credentials reach a server; the config file never holds them
  verbatim, and `resolve_mcp` rejects a header or env value that contains a
  literal `Bearer ` token or `ghp_`/`sk-`-style prefix outside a `${...}`
  reference.
- A stdio server's child environment is exactly the `env` table plus
  `PATH`, `HOME`, `TMPDIR`, `LANG`, and `TERM` copied from Otto's own
  environment. No other inherited variables.
- `cwd` resolves like `skills.paths`: `~/` and relative paths through
  `resolve_roots`. Default is the workspace path.
- Resolution lives in `crates/otto-core/src/config/mcp.rs` as
  `resolve_mcp(&File, env, workspace) -> Result<McpRuntime, ConfigError>`.
  It is pure (no I/O) and wasm-safe.

## Tool naming

An MCP tool `t` on server `s` is registered as `mcp__<s>__<t'>` where `t'`
is `t` with every byte outside `[A-Za-z0-9_-]` replaced by `_`. This matches
the Claude Code convention, so prompts and skills written for Claude Code
transfer unchanged.

- If the combined name exceeds 64 bytes, the tool is skipped and a warning
  is logged with the original name. Truncation is not attempted because two
  tools could truncate to the same name.
- If two tools on the same server sanitize to the same name, both are
  skipped with a warning.
- Cross-server collisions cannot occur because the server name is unique in
  the config table. Collisions with built-in tools cannot occur because no
  built-in name starts with `mcp__`; `Registry::new` still enforces the
  invariant.

The advertised `ToolDefinition` is:

- `name`: the prefixed name.
- `description`: `[<server>] ` followed by the server's `description`, or
  `title` when `description` is empty, capped at 1024 bytes. Annotations are
  not forwarded; the spec marks them untrusted.
- `parameters`: the server's `inputSchema` verbatim. If it is absent or not
  an object, `{"type": "object"}` is used.

## Result mapping

`ToolResult` is text-only. A `tools/call` result maps as follows:

1. `isError: true` sets `is_error`.
2. Every `content` block contributes one segment, joined by `\n`:
   - `text` → the text.
   - `image` → `[image <mimeType>, <n> bytes base64 omitted]`.
   - `audio` → `[audio <mimeType>, <n> bytes base64 omitted]`.
   - `resource_link` → `[resource <uri>] <title or name>`.
   - `resource` → the embedded `text` if present, otherwise
     `[resource <uri>, <mimeType>, <n> bytes blob omitted]`.
3. If `content` is empty and `structuredContent` is present, the segment is
   `structuredContent` serialized as JSON.
4. The joined text goes through `capped_text_result(text,
   max_output_bytes)`, the same cap the built-in tools use.
5. Every value that was substituted from `${VAR}` in the server's `env` or
   `headers` is redacted from the text with `redact_exact_text` before the
   cap, so a server that echoes its token back cannot leak it into the
   transcript.

A JSON-RPC error response (protocol error, unknown tool, invalid arguments)
becomes an error result whose text is `mcp <server>: <code> <message>`.
Transport failures (child exited, HTTP 5xx, timeout) become
`mcp <server>: <description>`; the model can retry after the client
reconnects.

`persisted_content` is left `None`; the transcript stores the capped text.

## Transports

### stdio

`crates/otto/src/mcp/stdio.rs`. The client spawns the command with
`tokio::process::Command`, `stdin` and `stdout` piped, `stderr` piped and
drained to the log at debug level (line-buffered, capped at 64 KiB per line)
(superseded, see Deviations: stderr is kept as a bounded tail folded into
the failure reason, not logged).
Messages are newline-delimited JSON-RPC; a line that is not valid JSON-RPC
is logged and skipped.

The sandbox executor (`crates/otto/src/sandbox/process.rs`) closes the
child's stdin, so it cannot host a stdio server. In this iteration stdio
servers run unsandboxed, in the same process group as Otto, with the
restricted environment described above. The user manual states this. Running
the child under the generated Seatbelt profile is a follow-up, listed below.

Shutdown on `Runner::close`: close stdin, wait 2 s for exit, `SIGTERM`, wait
2 s, `SIGKILL`.

Unexpected exit: pending calls fail with `server exited (<status>)`. The next
call re-spawns the server once; a second failure within 30 s marks the
server `failed` in `/mcp` and every later call returns the error without
spawning.

### Streamable HTTP

`crates/otto/src/mcp/http.rs`, using the already-pinned `reqwest` client.
Every request is one POST to `url` with:

- `Content-Type: application/json`
- `Accept: application/json, text/event-stream`
- `MCP-Protocol-Version: <negotiated>`
- `Mcp-Method: <method>` and `Mcp-Name: <tool name>` on modern servers (the
  base64 sentinel form when the name is not a valid header value)
- `Mcp-Session-Id` on legacy servers when the server issued one
- the configured `headers`

A `text/event-stream` response is read until the event carrying the JSON-RPC
response with the matching `id`; other events are ignored. The SSE reader is
a 40-line parser in `crates/otto/src/mcp/sse.rs` (`data:` accumulation,
blank-line dispatch, `event:`/`id:` ignored); `openaicompat::stream` is
provider-specific and is not reused. The response is treated as complete
when the stream ends; `notifications/cancelled` is not sent over HTTP,
cancellation closes the stream.

Redirects are not followed. TLS is `rustls` through the existing `reqwest`
features.

### OAuth 2.1 authorization (HTTP only)

`crates/otto/src/mcp/oauth.rs`, following the MCP authorization
specification (OAuth 2.1 authorization code grant with PKCE S256, RFC 9728
protected resource metadata, RFC 8414 authorization server metadata, RFC
7591 dynamic client registration, RFC 8707 resource indicators). A server
with `auth = "oauth"` goes through this flow; `auth = "none"` (default)
sends only the static `headers`.

Discovery, run by `otto mcp login <server>` and again whenever a stored
token is rejected:

1. POST `tools/list` without a token. Expect HTTP 401 with
   `WWW-Authenticate: Bearer resource_metadata="<url>"`. If the header is
   absent, try `<origin>/.well-known/oauth-protected-resource<path>` then
   `<origin>/.well-known/oauth-protected-resource`.
2. Fetch the protected resource metadata; take `resource`,
   `authorization_servers[0]`, and `scopes_supported`. `resource` must equal
   the configured `url` after normalization (scheme and host lowercased, no
   trailing slash, no fragment); otherwise the login fails with
   `resource mismatch` and the token is never requested.
3. Fetch the authorization server metadata: try
   `/.well-known/oauth-authorization-server<path>`, then
   `/.well-known/oauth-authorization-server`, then
   `/.well-known/openid-configuration<path>`, then
   `/.well-known/openid-configuration`. Every URL must be `https` or a
   loopback `http` address; anything else fails the login.
4. Client identity: if the metadata has `registration_endpoint`, POST a
   dynamic registration with `client_name: "otto"`,
   `redirect_uris: ["http://localhost:1455/auth/callback",
   "http://localhost:1457/auth/callback"]`, `grant_types:
   ["authorization_code", "refresh_token"]`, `token_endpoint_auth_method:
   "none"`, and store the returned `client_id`. Otherwise use
   `oauth_client_id`; if that is unset the login fails with
   `server requires a client_id (set oauth_client_id)`.
5. Authorization: `listen_loopback(&LOOPBACK_PORTS)` (reused from `auth`),
   `generate_verifier`, `s256_challenge`, `random_state` (reused), then the
   authorization URL with `client_id`, `redirect_uri`, `response_type=code`,
   `code_challenge`, `code_challenge_method=S256`, `state`, `scope`
   (`oauth_scopes` joined by spaces, else `scopes_supported`, else the
   `scope` parameter from the 401 `WWW-Authenticate` header, else omitted),
   and `resource=<canonical url>`. The URL is opened through the same
   opener `otto login` uses. The callback listener is the existing
   `auth::login::serve_callback`, made `pub(crate)`.
6. Token: POST `token_endpoint` with `grant_type=authorization_code`,
   `code`, `redirect_uri`, `code_verifier`, `client_id`, `resource`.
   Refresh: `grant_type=refresh_token`, `refresh_token`, `client_id`,
   `resource`. Both use `auth::oauth::http_client()` (no redirects).

Token storage: `~/.otto/auth/mcp/<server>.json`, 0600 in a 0700 directory,
written atomically by the same temp-file-and-rename path `Credentials::save`
uses (that code moves into a shared `auth::write_secret_file`). Fields:
`access_token`, `refresh_token`, `expiry` (RFC 3339), `client_id`,
`token_endpoint`, `resource`, `scope`. The file is loaded at runner start;
the token is sent as `Authorization: Bearer <access_token>` on every request
to the configured `url` only. A configured static `Authorization` header and
`auth = "oauth"` on the same server is a configuration error.

Runtime behavior:

- Access token expired (or expiring within 60 s) → refresh before the
  request. Refresh failure → the server's status becomes `needs login`, the
  tool call returns `mcp <server>: authorization required; run 'otto mcp
  login <server>'`, and no browser is opened. Login is never started from
  inside a tool call.
- HTTP 401 with a stored token → one refresh attempt, then the same
  `needs login` state.
- HTTP 403 with `error="insufficient_scope"` → error result naming the
  required scope; the user re-runs login with `oauth_scopes` set.
- At runner start a server whose token file is missing is reported as
  `needs login` and contributes no tools; the runner still starts.

Commands: `otto mcp login <server>` (runs discovery and the flow, then
prints `logged in to <server>`), `otto mcp logout <server>` (removes the
token file), and `/mcp login <server>` in the REPL, which runs the same
flow and then prints `logged in to <server>; restart otto to load its
tools`. The tool registry is immutable after `build_runner`, and adding
in-session re-registration is out of scope for this iteration. `/mcp`
shows `needs login` for such servers.

Error reporting follows the `auth` module: every failure maps to a
fieldless variant, so no token, code, or endpoint body reaches a log or the
terminal. Access and refresh tokens are added to the redaction set for that
server's results and stderr log.

### Shared JSON-RPC layer

`crates/otto/src/mcp/jsonrpc.rs`: `Request`, `Response`, `Notification`,
`Error` types with `serde`, id allocation, and the `_meta` builder for
modern requests. `crates/otto/src/mcp/client.rs`: `Client` holding one
`Box<dyn Transport>`, the negotiated era, the cached tool list, and a
`tokio::sync::Mutex` guarding the in-flight request map. `Transport` is the
one trait with two implementations because there are two real consumers.

## Lifecycle

- `build_runner` calls `mcp::connect_all(runtime, workspace)` after
  `build_catalogs` and before `Registry::new`. Servers connect concurrently
  with `connect_timeout_secs` (superseded, see Deviations: connects run one
  server at a time, in configuration order). A server that fails to connect
  is logged, reported by `/mcp` as `failed: <reason>`, and contributes no
  tools; the runner still starts. This mirrors how a missing skills
  directory is handled.
- `connect_all` returns `McpTools { tools: Vec<Box<dyn Tool>>, status: Arc<McpStatus> }`.
  The tools are pushed into the tool vector; the status handle is stored on
  `Runner` and exposed through `Controller::mcp()` for the REPL.
- Subagents (`child_tools`) receive the same MCP tool instances; the
  `Client` is `Arc`-shared and `execute` takes `&self`.
- `Runner::close` calls `McpStatus::close_all`, which shuts every stdio child
  down as described above and drops HTTP clients.
- Cancellation: `execute` selects on the `CancellationToken`. On cancel the
  stdio client sends `notifications/cancelled` with the request id and
  returns the `context canceled` error result; the HTTP client drops the
  response future.

## `/mcp` command

`repl_commands.rs` gains `mcp_command`, formatted like `skills_report`:

```
mcp servers (2 configured, 1 connected)
  github   stdio  modern   connected   12 tools
  legacy   stdio  -        disabled    -
  docs     http   legacy   failed: connect timeout after 20s
```

## Security

- Tool descriptions, schemas, and results are untrusted text from the
  server. They are capped and redacted as above and are never executed or
  interpolated into shell commands.
- The tool name prefix and the `[<server>]` description prefix let the model
  and the user attribute a tool to its server.
- No secret is written to logs: `${VAR}` values are redacted from the debug
  stderr log the same way they are redacted from results (superseded, see
  Deviations and `docs/user-manual.md`: only `env`/`headers` substitutions
  are secrets, and stderr is kept as an in-memory tail, not logged).
- HTTP servers are contacted only over the configured URL; no redirects.
- OAuth: the bearer token is sent only to the configured `url`; metadata and token endpoints must be `https` (or loopback `http`); `resource` in the protected resource metadata must match the configured URL; `state` is checked on the callback; the client never handles a client secret (`token_endpoint_auth_method: none`).
- stdio servers run unsandboxed in this iteration. The user manual says so
  in the MCP section and in README Limitations.

## Package layout

```
crates/otto-core/src/config/mcp.rs      Mcp, McpServer tables; resolve_mcp; ${VAR} expansion
crates/otto/src/mcp/mod.rs              connect_all, McpStatus, McpTools, module contract doc
crates/otto/src/mcp/jsonrpc.rs          message types, _meta builder
crates/otto/src/mcp/client.rs           Client, era negotiation, tools/list, tools/call
crates/otto/src/mcp/stdio.rs            StdioTransport
crates/otto/src/mcp/http.rs             HttpTransport, bearer injection, 401/403 handling
crates/otto/src/mcp/oauth.rs            discovery, registration, PKCE flow, token file
crates/otto/src/mcp/sse.rs              minimal SSE frame reader
crates/otto/src/auth/mod.rs             write_secret_file extracted from Credentials::save
crates/otto/src/cli/mcp.rs              `otto mcp login|logout <server>` subcommand
crates/otto/src/tool/mcp.rs             McpTool: Tool adapter, naming, result mapping
crates/otto/src/cli/wiring.rs           push MCP tools; Runner field; close
crates/otto/src/cli/repl_commands.rs    /mcp
crates/otto/src/app/mod.rs              Controller::mcp()
crates/otto/tests/mcp_stdio.rs          end-to-end against a fixture server
testdata/mcp/fake_server.py             stdio fixture: legacy and modern modes, echo/error/image tools
```

No new crate dependencies. `rmcp` (the official Rust SDK) was considered
and rejected for this iteration: it pulls in its own transport and schema
stack, the repository pins every dependency exactly, and the tools-only
client is under 1,000 lines.

## Development plan

Each step is one subagent task with bounded file ownership. Tests are
written first in every step.

1. `otto-core` config (`config/mcp.rs`, `config/mod.rs` `File.mcp`):
   parsing, validation, `${VAR}` expansion, error messages. Model: sonnet.
2. JSON-RPC types and SSE reader (`mcp/jsonrpc.rs`, `mcp/sse.rs`): pure
   codecs with unit tests. Model: haiku.
3. `McpTool` adapter (`tool/mcp.rs`): naming, definition, result mapping,
   redaction, cap, against a fake `Client` trait object. Model: sonnet.
4. stdio transport and `Client` (`mcp/client.rs`, `mcp/stdio.rs`,
   `mcp/mod.rs`): era negotiation, tools/list pagination, tools/call,
   cancel, shutdown, restart (superseded, see Deviations: restart was not
   implemented); tests drive `testdata/mcp/fake_server.py`.
   Model: sonnet.
5. HTTP transport (`mcp/http.rs`): modern and legacy against an in-process
   `tokio` TCP listener that serves scripted responses (JSON and SSE),
   bearer injection from a token file, 401 → refresh → `needs login`.
   Model: sonnet.
6. OAuth (`mcp/oauth.rs`, `auth::write_secret_file`, `cli/mcp.rs`):
   discovery chain, dynamic registration, PKCE flow, token file, refresh;
   tests use `auth::testserver` extended with metadata and registration
   routes. Model: sonnet.
7. Wiring, `/mcp`, `/mcp login`, `Controller::mcp()`, `Runner::close`,
   docs (user manual MCP section, README Limitations, AGENTS.md task map,
   development.md). Model: sonnet.

Steps 1, 2, 3, 6 run concurrently. Step 4 needs 2; step 5 needs 2 and 6's
token file type (agreed up front as a small struct in `mcp/oauth.rs`
created by step 6 first); step 7 needs 3, 4, 5, 6. The parent reviews each
step's diff and runs `make check-fast`, then `make check` after step 7.

## Follow-ups

- Run stdio servers under the Seatbelt profile (needs a sandbox executor
  variant with piped stdin).
- `notifications/tools/list_changed` and `/mcp reload`.
- Client ID Metadata Documents as a client identity option.
- In-session tool re-registration after `/mcp login` (mutable registry).
- Resources and prompts.
- Per-tool approval prompts, following `BashApprovals`.
- Structured (non-text) tool results once `ToolResult` supports them.

## Decisions taken

- Both protocol eras are supported from the first iteration.
- Configuration is TOML in `config.toml`, not a separate `.mcp.json`.
- Tool names use the `mcp__<server>__<tool>` convention.
- No new dependencies.
- stdio servers run unsandboxed with an explicit environment (user decision, 2026-09-19).
- HTTP transport and OAuth 2.1 are in the first iteration (user decision, 2026-09-19).
- Tokens live in `~/.otto/auth/mcp/<server>.json`, next to the ChatGPT credential file, with the same file permissions and atomic write.
