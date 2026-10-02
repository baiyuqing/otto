# ACP agent server (`otto acp`)

Status: approved 2026-10-02 with D1 = the official schema crate and D2 =
after-turn approval. D3 (upstream cc-connect PRs) is not part of this change.

## IM access goes through cc-connect, which drives `otto acp` over ACP v1

The user wants to talk to otto from Telegram and Feishu, does not want to
write or maintain an IM gateway, and requires the answering agent to be
otto. The chosen split:

- [cc-connect](https://github.com/chenhg5/cc-connect) (Go, open source, read
  at `dfad194`, 2026-09-29) owns everything IM-specific: 20 platform
  adapters including Telegram and Feishu, user allowlists, message
  splitting, buttons, `/new` `/list` `/switch` `/stop`. Its `type = "acp"`
  agent spawns any program that speaks the Agent Client Protocol on stdio.
- otto owns nothing IM-specific. It adds `otto acp`, the agent side of ACP
  v1 (protocol version `1`, newline-delimited JSON-RPC 2.0 on stdin/stdout).
  Any ACP client can use it; cc-connect is the client this change is
  verified against.

Gaps in cc-connect are fixed upstream, not worked around in otto (see
"cc-connect changes this depends on").

## Facts this design relies on

otto at `123c0c6`:

- Frontends share `app::Controller`. Turns: `prompt(text, emit, cancel)`
  (`crates/otto/src/app/mod.rs:630`); admission errors when closed or when a
  prompt is active (`:47-55`, `:253-267`). Sessions: `Controller::create`
  (`:1309`), `Controller::open(builder, path)` (`:1317`), `history()`
  (`:428`), `list_sessions(limit)` (`:871`).
- Events reach a frontend through `EventSink`, a synchronous
  `FnMut(Event)` (`crates/otto-core/src/agent/events.rs:237-241`). Tool
  calls run one at a time and `ToolCallStarted` carries the complete
  argument JSON (`crates/otto-core/src/agent/mod.rs:453-486`).
- A session file is `<sessions root>/<workspace key>/<id>.jsonl`
  (`crates/otto/src/session/store.rs:843-845`, `session/list.rs:23`). The id
  is 32 lowercase hex characters. `Store::create_lazy` writes no file until
  the first durable write (`store.rs:157-190`). `session::list` returns at
  most 20 rows, newest first (`list.rs:19`). A session open in another otto
  process fails with "session is already open by another Otto process"
  (`session/fsops.rs:74-104`).
- Elevated bash: a `bash` call with `sandbox_permissions: RequireEscalated`
  returns an error result containing `Approve in Otto: /approve <id>`,
  `Command:` and `Justification:` lines (`crates/otto/src/tool/bash.rs:573`).
  `BashApprovals` keeps at most one pending request per session
  (`bash.rs:131-151`). `Controller::approve_bash(id)` requires an idle
  controller and returns the retry prompt "The user approved {id} for one
  exact command. Retry the same elevated Bash command now."
  (`app/mod.rs:1203-1211`). The TUI parses the tool result at
  `tui/app.rs:1547-1565` and sends the retry prompt itself.
- Background wake turns (`app/wake.rs:16-54`) run only in frontends that
  start a wake loop (`server/mod.rs:798-923` for `otto serve`). Inbox
  notifications not consumed by a wake turn are delivered before the next
  user message (`agent/mod.rs:353`).
- The Feishu inbound consumer starts only under `otto serve`
  (`cli/serve.rs:845`).
- `otto serve` keeps one `Builder` per workspace (`cli/serve.rs:41-140`);
  serve makes a lease-less SIGTERM cancel instead of exit
  (`cli/serve.rs:752`, `cli/terminate.rs:20-90`).
- Warnings go to stderr; stdout is written only through the handle `main`
  passes in and by the TUI. Bash children get `stdin=null`.

cc-connect at `dfad194` (`agent/acp/`):

- One `otto acp` process per IM chat, one ACP session in it, started on the
  first message. The process is killed (SIGKILL via `exec.CommandContext`
  and `Process.Kill`) on `/new`, `/switch`, `/stop`-then-restart, idle
  timeout (120 min default) and errors, with no grace period
  (`session.go:727-745`).
- Handshake: `initialize` → `session/load {sessionId, cwd, mcpServers: []}`
  when it has a saved id and `agentCapabilities.loadSession` is true,
  otherwise `session/new {cwd, mcpServers: []}` (`session.go:163-249`). A
  failed load, or a load result without a non-empty `sessionId` field, falls
  back to `session/new`.
- `session/prompt` always carries one text block; images and files are
  saved to disk and referenced by path in the text (`session.go:600-628`).
- Update mapping (`mapping.go:34-49`): `agent_message_chunk` is reply text;
  `tool_call` and `tool_call_update` are tool progress (summary from
  `rawInput.command` for kind `execute`, `rawInput.path` for `read`/`edit`);
  `plan` is shown as thinking; `user_message_chunk` is dropped. Any other
  update whose JSON has a `content.text` field becomes reply text
  (`mapping.go:285-310`); this includes `agent_thought_chunk`.
- `session/request_permission` is shown as allow / deny buttons. Allow
  selects the first option whose kind contains `allow`; deny the first with
  `reject` or `deny`. `/stop` sends `session/cancel` and leaves the
  permission request unanswered.
- Updates are pushed into a 128-slot channel by the RPC read loop, which
  blocks when the channel is full (`session.go:87`, `:580-588`). During
  `session/load` nothing reads the channel, so a replay of more than 128
  mapped updates blocks the read loop before it reads the load response.
- Text sent outside a `session/prompt` is collected and never delivered,
  because the ACP adapter emits the turn-end event only when
  `session/prompt` returns (`core/engine.go:4962-5100`).

## `otto acp` serves one workspace per process

- Invocation: `otto acp`, parsed as a subcommand like `serve`
  (`cli/flags.rs:86`). It accepts `--config`, `--cwd`, `--profile`,
  `--provider`, `--base-url`, `--model`, `--thinking`, `--sandbox`,
  `--shell-timeout` and `--max-output-bytes`, and rejects `--ui`,
  `--prompt`, `--resume`, `--continue`, `--no-session`, `--archive` and the
  serve-only flags.
- The workspace is `--cwd`, else the process working directory,
  canonicalized. A `session/new`, `session/load` or `session/list` whose
  `cwd` canonicalizes to a different path is rejected with `-32602`.
  cc-connect starts the process in `work_dir` and passes the same path as
  `cwd`.
- The workspace is composed (sandbox, MCP servers, memory, approvals with
  elevation enabled, as for `otto serve`) on the first `session/new` or
  `session/load`, not at startup. `initialize` and `session/list` do not
  start MCP servers; cc-connect runs `session/list` in a separate short-lived
  process with a 15 s timeout.
- A connection may hold several sessions. Each has its own `Controller` on
  the shared `Builder`. One prompt per session at a time; prompts on
  different sessions run concurrently.
- stdout carries only JSON-RPC frames, one per line, written by a single
  writer task. Diagnostics go to stderr. A test asserts that every stdout
  line parses as a JSON-RPC message.
- The Feishu inbound consumer and the wake loop are not started.

## Methods

| Method | Behavior |
|---|---|
| `initialize` | Returns `protocolVersion: 1`, `agentCapabilities {loadSession: true, promptCapabilities {image: false, audio: false, embeddedContext: false}, mcpCapabilities {http: false, sse: false}, sessionCapabilities {list: {}}}`, `authMethods: []`, `agentInfo {name: "otto", version}`. A client version other than 1 gets `protocolVersion: 1` back, per spec. |
| `session/new` | Validates `cwd`, composes the workspace if needed, creates a session, returns `{sessionId}`. No file exists until the first prompt writes one. |
| `session/load` | Validates `cwd` and that `sessionId` is 32 lowercase hex, opens `<root>/<workspace key>/<id>.jsonl` directly (no 20-row limit), replays history (next section), then returns `{sessionId}`. `sessionId` in the result is not in the ACP schema; cc-connect requires it, and other clients ignore unknown fields. |
| `session/list` | Returns the newest 20 sessions of the workspace as `{sessionId, cwd, title, updatedAt}`, `title` = session name, else the last user text truncated to 80 characters. `cursor` is ignored and `nextCursor` is never set. |
| `session/prompt` | Concatenates `text` blocks with `\n`; a `resource_link` block becomes a line `<name>: <uri>`. Other block types get `-32602`. Runs `Controller::prompt`, streams updates, returns `{stopReason}`: `end_turn` on success, `cancelled` after `session/cancel` or a cancelled turn. |
| `session/cancel` | Cancels the running prompt and any pending permission wait of that session. |
| `session/request_permission` (otto → client) | Elevated bash approval, below. |

Not implemented, so not advertised: `authenticate` (no auth methods;
credentials come from environment variables and `otto login`),
`session/resume`, `session/close`, `session/delete`, `session/set_mode`
(otto has no modes), `session/set_config_option`. Unknown methods get
`-32601`; unknown notifications are ignored. otto never calls the client's
`fs/*` or `terminal/*` methods.

`mcpServers` must be empty; a non-empty list gets `-32602` with "configure
MCP servers in otto's config file". ACP says agents MUST accept stdio MCP
servers from the client; otto deviates because its MCP servers are composed
per workspace from configuration, and cc-connect always sends `[]`.

## Event mapping

| otto event | ACP `session/update` |
|---|---|
| `TextDelta` | `agent_message_chunk` `{type: text}` |
| `ReasoningDelta` | `agent_thought_chunk` `{type: text}` |
| `ToolCallStarted` | `tool_call` `{toolCallId, title, kind, status: in_progress, rawInput}` |
| `ToolCallFinished` | `tool_call_update` `{toolCallId, status: completed or failed, content: [text of the result]}` |
| everything else | not sent |

- `toolCallId` is the provider's tool call id, the same id stored in the
  session, so replayed and live ids match.
- `rawInput` is the parsed argument JSON; arguments that do not parse are
  sent as a JSON string.
- `kind`: `bash` → `execute`; `read` → `read`; `write`, `edit` → `edit`;
  `ls`, `find`, `grep`, `memory_search` → `search`; all others → `other`.
- `title`: the `command` argument for `bash`, `path` for `read`, `write`,
  `edit`, `ls`, `pattern` for `find` and `grep`, otherwise the tool name.

## `session/load` replays one update per block

ACP requires the full conversation as `session/update` notifications before
the load response. otto replays `history()` with one update per block, not
per streamed delta:

- `User` message: each `Text` block → `user_message_chunk`.
- `Assistant` message: `Reasoning` → `agent_thought_chunk`; `Text` →
  `agent_message_chunk`; `ToolCall` → `tool_call` with `status: pending`.
- `Tool` message: each `ToolResult` → `tool_call_update` with the final
  status and content.
- `Context` and other roles, and `Image` blocks, are not replayed.

## Elevated bash approval maps to `session/request_permission` after the turn

The bash tool keeps its current contract: an elevated call returns the
approval-required error result, and the turn continues. `otto acp` then
does what the TUI does, with the client's permission UI in place of
`/approve`:

1. During the turn, a `bash` `ToolCallFinished` whose result is an approval
   request is recorded (id, command, tool call id). The parser moves from
   `tui/app.rs:1547-1565` into `tool/bash.rs` next to the formatter, and the
   TUI calls the moved function.
2. After `Controller::prompt` returns `Ok`, if the last recorded request is
   still pending (`BashApprovals::pending_command`), otto sends
   `session/request_permission` with `toolCall {toolCallId, title: command,
   kind: execute, rawInput: {command}}` and two options:
   `{optionId: "allow_once", kind: "allow_once", name: "Allow once"}` and
   `{optionId: "reject_once", kind: "reject_once", name: "Deny"}`.
3. `allow_once` → `Controller::approve_bash(id)`, then
   `Controller::prompt(<retry prompt>)` inside the same `session/prompt`,
   streaming its updates; step 2 repeats for any new request.
4. `reject_once`, outcome `cancelled`, or an error response → the prompt
   returns `end_turn`. The request stays pending in `BashApprovals`, as it
   does when a TUI user ignores it.
5. `session/cancel` during the wait → the prompt returns `cancelled`; a late
   permission response is ignored.

No `allow_always` option: cc-connect never selects it (its "allow all"
button is a cc-connect-side setting for the chat). `/approve <id> always`
remains available in the REPL and TUI.

The retry prompt is stored in the session as a user message, as in the TUI.

## Background results reach the model at the next prompt

`otto acp` does not run wake turns. A finished sub-agent or an inbox item
is delivered to the model before the next user message
(`agent/mod.rs:353`). ACP v1 has no agent-initiated turn, and cc-connect at
`dfad194` does not deliver text sent outside a prompt.

## Errors

| Condition | JSON-RPC error |
|---|---|
| malformed JSON | `-32700`, id `null` |
| not a request/notification/response object | `-32600` |
| unknown method | `-32601` |
| bad params, wrong `cwd`, bad `sessionId` format, non-empty `mcpServers`, unsupported content block, empty prompt | `-32602` |
| unknown `sessionId` (not loaded in this connection, or no file for `session/load`) | `-32002` |
| prompt already running in the session | `-32603` "a prompt is already running" |
| session open in another otto process | `-32603` with otto's message |
| provider or other turn error | `-32603` with the `AgentError` display text |

## Shutdown

stdin EOF and SIGTERM both: cancel every running prompt, answer each with
`stopReason: cancelled` while stdout is writable, close every controller
through the same close path as the REPL exit, exit 0. SIGTERM uses serve's
termination mode.

cc-connect sends SIGKILL, so none of this runs there. What remains on disk
is what otto had already appended to the session file; the assistant
message in flight is lost. Seatbelt per-process directories
(`otto-sandbox-<hex>/` under the temp directory) are not removed on SIGKILL.
A cc-connect change to close stdin and wait before killing is listed below
and is not required for correctness.

## ACP types come from the official `agent-client-protocol-schema` crate

Use `agent-client-protocol-schema` (Apache-2.0, published by the ACP
maintainers, 1.10.2 of 2026-10-01) with `default-features = false`, pinned
to an exact version like otto's other dependencies, in `crates/otto` only.
It supplies the v1 request, response and notification types and the
JSON-RPC envelope types. otto writes the stdio loop on tokio; the runtime
crate `agent-client-protocol` 2.2.0 is not used (37 new packages, its own
non-tokio IO layer).

Cost, measured:

- 5 new packages (`agent-client-protocol-schema`, `anyhow`, `serde_with`,
  `serde_with_macros`, `unicode-xid`) and `syn` 3 beside `syn` 2.
- The crate enables `serde_json/preserve_order` and `serde/rc`. Cargo
  feature unification turns them on for every crate in a native workspace
  build: every `serde_json::Map` keeps insertion order instead of sorting
  keys. `otto-core` built alone for wasm32 keeps sorted keys.
- Running `cargo test --workspace` at `123c0c6` with `preserve_order`
  enabled and without it, in this environment: one test changes outcome,
  `agent::context_estimate::tests::estimate_request_falls_back_to_stable_system_messages_and_tools`
  (`crates/otto-core/src/agent/context_estimate.rs:290`), which compares a
  tool schema serialized with sorted keys. 122 other tests failed in both
  runs because the environment blocks loopback listeners and the Seatbelt
  sandbox; they are not covered by this comparison and run under
  `make check` during implementation.

The change updates that test to assert parsed JSON equality, and records
in the development guide that `serde_json` map order is insertion order in
native builds.

The alternative is hand-written serde types for the subset above, an
estimated 400 lines, with no dependency and no feature change; not chosen
(D1).

## cc-connect changes this depends on

| Problem at `dfad194` | Upstream status (2026-10-02) | Effect on otto users until merged |
|---|---|---|
| `agent_thought_chunk` becomes reply text | issue #1940; PR #1789, approved, not merged | otto's reasoning text appears in IM replies when thinking is on |
| `session/load` replay over 128 updates blocks the RPC read loop; #1941 reports that startup also holds an engine-wide mutex (not verified here) | issue #1941, fix in a fork commit, no PR; PR #1620 bounds load to 90 s and does not remove the block | resuming a long otto session hangs that chat and blocks other chats' session operations |
| SIGKILL with no grace period | not reported | sandbox temp directories accumulate; no data loss beyond the in-flight message |

The second row decides whether the setup is usable for long sessions. Plan:
otto implements the spec behavior; the user runs a cc-connect build with
the #1941 fix until it is released. Opening or updating upstream PRs is a
separate, outward-facing step that needs the user's go-ahead.

## cc-connect configuration

Documented in the user manual, with placeholders only:

```toml
[[projects]]
name = "otto"

[projects.agent]
type = "acp"

[projects.agent.options]
work_dir = "/path/to/workspace"
cmd = "otto"
args = ["acp"]
display_name = "Otto"

[[projects.platforms]]
type = "telegram"

[projects.platforms.options]
token = "<bot token>"
allow_from = "<your Telegram user id>"
```

Points the manual states:

- cc-connect's `allow_from` defaults to `*`. Anyone who can message the bot
  can then run otto in the workspace, including approving elevated bash.
  Set it to your own user id.
- Bot tokens and app secrets live in cc-connect's configuration, not
  otto's. otto's provider key must be in the environment cc-connect starts
  otto with (`OPENAI_API_KEY` or the configured variable), or use the
  `chatgpt` provider after `otto login`.
- Each chat is a separate `otto acp` process with its own sandbox and MCP
  servers.
- Do not configure the same Feishu app in both cc-connect and otto's
  `[inbound.feishu]`. Whether Feishu delivers each event to both consumers
  was not verified.
- A session open in the TUI cannot be loaded by `otto acp` at the same
  time; cc-connect then starts a new session for the chat.

## Ownership

- `crates/otto/src/acp/`: new module. `mod.rs` (connection loop, dispatch,
  writer task, per-session state), `update.rs` (event and history to
  `session/update`), `approval.rs` (the permission round trip).
- `crates/otto/src/cli/acp.rs`: composition and lifecycle, following
  `cli/serve.rs`. `cli/flags.rs`, `cli/run.rs`: the subcommand and its
  flag checks.
- `crates/otto/src/tool/bash.rs`: the approval-result parser moved from
  `tui/app.rs`.
- `crates/otto-core/src/agent/context_estimate.rs`: the test change.
- `Cargo.toml`, `crates/otto/Cargo.toml`: the dependency.
- Docs: `docs/user-manual.md` (new "ACP agent server" section with the
  cc-connect setup), `README.md` (one line), `AGENTS.md` task map (`acp`),
  `docs/development.md` package list and the map-order note, usage text in
  `cli/flags.rs`.

## Tests

All offline, using the fake provider the existing binary tests use.

- `crates/otto/tests/acp_stdio.rs` drives the `otto acp` binary over pipes:
  - `initialize` result fields; every stdout line is JSON-RPC.
  - `session/new` → `session/prompt` streams `agent_message_chunk`, a
    `tool_call` / `tool_call_update` pair for a `read` call, and returns
    `end_turn`.
  - `session/cancel` during a slow provider response returns `cancelled`.
  - `session/load` of the session from the previous step in a new process:
    replay arrives before the response, the response contains `sessionId`.
  - `session/load` with a non-hex id gets `-32602`; with an unknown id
    `-32002`.
  - `session/list` contains the session.
  - wrong `cwd` and non-empty `mcpServers` get `-32602`.
  - stdin EOF during a prompt: the prompt is answered `cancelled` and the
    process exits 0.
  - macOS only: an elevated `bash` call produces `session/request_permission`;
    `allow_once` runs the retry prompt in the same `session/prompt`;
    `reject_once` returns `end_turn` without a retry.
- Unit tests in `acp/update.rs`: each mapping row, and replay of a stored
  history fixture.
- The moved approval parser keeps its TUI tests.

Manual acceptance before merge: cc-connect with the #1941 fix, Telegram,
one chat: prompt, tool call, approval allow and deny, `/stop`, `/new`,
`/list`, `/switch`, and resume after the idle kill.

## Decisions

- D1. ACP types come from the official schema crate; `serde_json` maps are
  insertion-ordered in native builds. The alternative was hand-written types
  with no dependency.
- D2. Approval is the after-turn `session/request_permission` above, which
  keeps the bash tool contract unchanged. The alternative, a blocking
  approval hook inside the bash tool, changes the tool contract for every
  frontend and would be a separate design.
- D3. Opening a cc-connect PR from the #1941 fix and commenting on #1789 is
  not done by this change; the user decides on it separately.

## Not in this change

- Wake turns over ACP; revisit if cc-connect PR #1936 (out-of-turn text
  delivery) merges.
- `session/resume`, `session/close`, `session/delete`, config options
  (model, thinking), `usage_update`, `available_commands_update`, image
  prompts, client-supplied MCP servers.
- Slash commands typed in IM (`/approve`, `/sandbox`): they reach the model
  as plain text.
