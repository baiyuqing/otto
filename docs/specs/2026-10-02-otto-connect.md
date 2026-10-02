# Chat connector (`otto-connect`)

Status: approved 2026-10-02 with D1-D6 as written. All three changes of
"Delivery in three changes" are implemented: 1 (core and Telegram), 2
(Feishu) and 3 (removal of `[inbound.feishu]`). The user manual's "Chat
connector" section describes current behavior.

Implementation notes for change 2 (`channel-sdk-go` v0.1.0), where the SDK
differs from the "Facts" below:

- Batching is off (`Safety.Batch.DelayMs = 0`). A merged message carries
  the last message's sender and message id with the text of the whole
  batch, so in a group an unlisted sender's text would be attributed to a
  listed sender.
- The SDK acknowledges an event when its dispatcher has queued it, before
  the `OnMessage` handler runs. Delivery stays at most once.
- The SDK drops messages older than its stale window without a callback.
  The adapter sets that window to 24 h so the bridge's 30-minute rule
  applies and the chat gets the notice.
- Messages the SDK policy rejects never reach the bridge; the adapter logs
  them from `OnReject` with the bridge's rejection log message, so the
  chat and sender ids can still be read from the log. Rejections with
  reason `no_mention` are not logged: the SDK checks the group before the
  mention, so the chat is already allowed, and the Telegram adapter drops
  unmentioned group messages without a log line too.
- With an empty `chats` list the adapter sets a group allowlist that no
  chat id matches, and with an empty `senders` list `DMMode = "disabled"`,
  because an empty SDK group allowlist admits every group.

Supersedes the "IM access goes through cc-connect" section of the
[ACP agent server design](2026-10-02-acp-agent-server.md). `otto acp` itself
is unchanged.

## Telegram and Feishu reach otto through an ACP client owned by this repository

The user wants to talk to otto from Telegram and Feishu without cc-connect
and without the `lark-cli` subprocess that `[inbound.feishu]` uses today. The
split:

- `otto acp` is the agent. It knows nothing about chat platforms.
- `otto-connect` is a separate program and an ACP client. It starts an ACP
  agent as a child process (by default `otto acp`), maps chats to ACP
  sessions, and talks to Telegram and Feishu. It links no otto code. Its
  only contract with otto is ACP v1 on stdio, so platforms, fixes and
  features are added to the connector without changes to otto, and the
  connector can drive any ACP agent.

`otto-connect` is written in Go because Feishu publishes an official Go SDK
for chat bots (`channel-sdk-go`) and no Rust SDK; see "Facts".

In this document "platform" means a chat service (Telegram, Feishu). It is
not a model provider; otto's provider list is unchanged.

## Facts this design relies on

otto at `947e4b3`:

- `otto acp` serves one workspace per process and may hold several
  sessions. A second `session/prompt` in a session that is already running
  one is rejected. Prompts accept text only (`docs/user-manual.md`, "ACP
  agent server").
- `session/load` sends the stored conversation as `session/update`
  notifications, then responds with `sessionId`
  (`crates/otto/src/acp/mod.rs:229-264`).
- Updates sent during a prompt: `agent_message_chunk`, `agent_thought_chunk`,
  `tool_call`, `tool_call_update` (`crates/otto/src/acp/update.rs`).
- An elevated bash command produces `session/request_permission` with options
  `allow_once` and `reject_once` while the `session/prompt` is still open;
  `allow_once` runs the retry in the same prompt
  (`crates/otto/src/acp/approval.rs`).
- `otto acp` sends no updates outside a prompt.
- `[inbound.feishu]` in `otto serve` runs
  `lark-cli event consume im.message.receive_v1 --as bot` and writes each
  message to every open session inbox; replies are not wired
  (`crates/otto/src/inbound/`, `docs/user-manual.md`, "Feishu inbound").

Feishu (read 2026-10-02):

- The `larksuite` organization publishes Go, Java, Python and Node SDKs, and
  no Rust SDK. `larksuite/channel-sdk-go` (MIT, created 2026-05-29, last push
  2026-08-06, depends on `oapi-sdk-go/v3` v3.9.7) is a bot SDK: WebSocket
  long connection, `OnMessage` with a `NormalizedMessage` (`ChatID`,
  `ChatType`, `UserID`, `MessageID`, `Content`, `MentionedBot`,
  `CreateTimeMs`), `Send` with `Text` or `Markdown` and `ReplyMessageID`,
  `Stream`, `OnCardAction`, `OnReject`.
- Its defaults admit everyone: `Policy.DMMode` is `"open"`, an empty
  `GroupAllowlist` allows all groups, and `DMMode` values other than
  `"disabled"` and `"allowlist"` behave as open. It drops messages older than
  30 minutes (`Safety.StaleMessageWindowMs`), deduplicates event IDs in
  memory for 1 hour, merges messages that arrive within 600 ms, and splits
  outgoing text at 3500 runes.
- The long-connection protocol is not documented apart from the SDKs
  (`oapi-sdk-go` `ws/`: `/callback/ws/endpoint`, protobuf `Frame` with 9
  fields, fragment reassembly, per-frame acknowledgement, ping frames).
  Implementing it in Rust would follow that source; this is why the
  connector uses Go.

ACP:

- `coder/acp-go-sdk` (Apache-2.0, v0.13.5, 2026-06-02) is the first Go
  library listed in the ACP documentation. Its connection processes
  notifications in order on one goroutine with a 1024-entry queue, closes
  the connection when the queue overflows, and returns a response only after
  the notifications received before it have been handled
  (`connection.go`). A `LoadSession` call therefore returns after the
  replayed updates have reached the client handler.

Telegram Bot API:

- `getUpdates` long polling needs no public endpoint. The server keeps
  undelivered updates for at most 24 hours; `offset` acknowledges earlier
  ones. `sendMessage` text is limited to 4096 characters. A
  `sendChatAction` `typing` status lasts 5 seconds or until the next
  message. With privacy mode on (the default), a bot in a group receives
  only commands, replies to its messages, and messages that mention it.

## One agent process, one session per chat

- `otto-connect` serves one workspace: it starts `[agent].command` with the
  workspace as working directory and passes the workspace as `cwd` in
  `session/new` and `session/load`.
- A chat is identified by `<platform>:<chat id>`. Each chat has at most one
  current ACP session. The map is stored in the state file and survives
  restarts.
- The first message of a chat with no session calls `session/new`. A chat
  whose session is not open in the current agent process calls
  `session/load` first. If `session/load` fails, the connector creates a new
  session and tells the chat that the previous session could not be loaded.
- Updates received for a session while its `session/load` is pending are
  dropped. They are the replayed history; the chat already shows it.
- If the agent process exits, every running prompt ends; each affected chat
  gets one message with the exit status and the last 20 lines of the
  agent's stderr. The connector starts the agent again on the next message,
  with a delay that doubles from 1 s to at most 60 s after consecutive exits
  within 60 s of a start.

## Messages are queued per chat and run one at a time

- A message from an admitted sender in an admitted chat (see "Admission")
  is appended to the chat's queue. The queue holds at most 10 messages; a
  message beyond that is not queued and the chat is told so.
- One prompt runs per chat. When it ends, the next queued message is sent.
  Messages from different chats run concurrently.
- The prompt text is the message text. Images, files and other attachments
  are not sent (otto accepts text prompts only); the chat is told that the
  attachment was ignored.

## The reply is the turn's text, sent when the prompt ends

- `agent_message_chunk` text is collected in order. A `tool_call` after
  text starts a new paragraph, so text before and after a tool call is
  separated by a blank line.
- `agent_thought_chunk`, `tool_call` and `tool_call_update` are not sent to
  the chat.
- When `session/prompt` returns, the collected text is sent as a reply to
  the message that started the turn. An empty reply sends nothing for
  `end_turn`; for `cancelled` the chat gets "stopped"; for other stop
  reasons the chat gets the stop reason.
- Telegram: plain text (no `parse_mode`), split at line boundaries into
  parts of at most 4096 characters; a `typing` action is sent every 4 s
  while the prompt runs.
- Feishu: `Send` with `Markdown`; the SDK splits it.

Streaming partial text into an edited message is not in this change.

## Connector commands are handled before the queue

A message whose text is exactly one of these commands (Telegram also accepts
`/command@botname`) is handled by the connector and not sent to the agent:

| Command | Effect |
| --- | --- |
| `/new` | The chat's next message starts a new session. The old session stays in otto's session store. |
| `/stop` | `session/cancel` for the running prompt; the queue is cleared. |
| `/allow` | Answers the chat's pending permission request with `allow_once`. |
| `/memory`, `/memory accept <id>`, `/memory reject <id>` | Memory review, see [memory review from the chat connector](2026-10-02-connect-memory-review.md). |
| `/deny` | Answers the chat's pending permission request with `reject_once`. |

Any other text, including other words starting with `/`, goes to the queue.

## Permission requests are answered in the chat

- `session/request_permission` sends the chat one message containing the
  command from the tool call and "Reply /allow or /deny".
- The request is answered by the first `/allow` or `/deny` from an admitted
  sender in that chat, by `/stop` (outcome `cancelled`), or after 10 minutes
  with `reject_once`; the chat is told which.
- `/allow` or `/deny` with no pending request gets "no pending request".

## Admission fails closed

Each platform section of the config has two lists, `chats` and `senders`.
A message is admitted only if its chat is in `chats` and its sender is in
`senders`. An empty list admits nothing, and the connector logs at startup
that the platform will ignore all messages. The bridge checks both lists for
every platform, so an adapter cannot admit a message on its own.

- Telegram: in a group, a message is also required to mention the bot,
  reply to one of its messages, or be a connector command addressed to it.
- Feishu: the connector sets the SDK policy from the config
  (`DMMode = "allowlist"`, `DMAllowlist = senders`,
  `GroupAllowlist = chats`, `RequireMention = true`), so the SDK also
  rejects other chats and senders. The bridge check still applies, because
  an empty SDK allowlist admits everyone.

Rejected messages are logged with platform, chat and sender IDs and get no
reply.

## Delivery is at most once

A message is acknowledged to the platform when it is queued, before the
agent runs it, so a crash does not run a prompt twice.

- Telegram: the `offset` is written to the state file after the update is
  queued or rejected.
- Feishu: the `OnMessage` handler only queues, so the SDK acknowledges the
  event immediately.
- Messages older than 30 minutes when received are dropped, matching the
  Feishu SDK default; the chat gets one message naming the dropped
  message's time.

## Configuration, secrets and state

`~/.config/otto/connect.toml` (override with `--config`):

```toml
[agent]
command = ["otto", "acp"]          # any ACP v1 agent
workspace = "/Users/me/work"

[telegram]
token_env = "OTTO_CONNECT_TELEGRAM_TOKEN"
chats = ["123456789"]
senders = ["123456789"]

[feishu]
app_id = "cli_xxx"
app_secret_env = "OTTO_CONNECT_FEISHU_APP_SECRET"
domain = "feishu"                  # or "lark"
chats = ["oc_xxx"]
senders = ["ou_xxx"]
```

- A platform is enabled when its section is present. Unknown keys fail the
  config load.
- The bot token and app secret come only from the environment variables the
  config names. A key such as `token` or `app_secret` in the file is an
  unknown key and fails the load.
- The agent process is started with the connector's environment minus the
  variables the config names for secrets, so tools run by the agent cannot
  read them.
- State: `~/.otto/connect/state.json`, mode `0600`, written by rename:
  chat-to-session map and the Telegram offset.
- Logs go to stderr. Message text is not logged.
- `otto-connect` runs in the foreground until SIGINT or SIGTERM; on either it
  cancels running prompts, stops the platform connections, and closes the
  agent's stdin.

Running it as a login service is not in this change. A launchd plist or
systemd unit that sets the secret environment variables writes the secrets to
a file, which the safety rules forbid; the service change has to solve that
(for example macOS Keychain, systemd `LoadCredential`).

## Layout

```text
connect/                     Go module github.com/baiyuqing/otto/connect
  go.mod                     go 1.26; acp-go-sdk, channel-sdk-go, BurntSushi/toml
  cmd/otto-connect/          flags, config, signal handling, wiring
  internal/config/           connect.toml parsing and validation
  internal/state/            state.json
  internal/agent/            child process, ACP client, restart, load/replay handling
  internal/bridge/           chat-to-session map, queues, commands, replies, permissions, admission
  internal/telegram/         Bot API over net/http
  internal/feishu/           channel-sdk-go adapter
```

`internal/bridge` uses one interface implemented by each platform:

```go
type Platform interface {
    Name() string
    // Run delivers messages until ctx ends. deliver must not block.
    Run(ctx context.Context, deliver func(Message)) error
    Send(ctx context.Context, chatID, replyTo, text string) error
    Typing(ctx context.Context, chatID string) error // no-op where unsupported
}
```

Telegram uses `net/http` and `encoding/json` from the standard library; the
four Bot API calls it needs (`getUpdates`, `sendMessage`,
`sendChatAction`, `getMe`) do not justify a dependency.

## Build and gates

- `make connect-check`: `cargo build -p otto` (for the end-to-end test),
  then `gofmt -l` (must be empty), `go vet ./...` and `go test -race ./...`
  in `connect/`. `make check` and `make check-linux` run it.
- `make connect-build` writes `target/otto-connect`.
- CI installs Go with `actions/setup-go` from `connect/go.mod`.

## Tests

Offline. No platform credentials, no network.

- `internal/bridge`, against a fake ACP agent (the test binary started as a
  child process, using the acp-go-sdk agent side, so process exit and
  restart run the production code path) and a fake platform:
  - a message produces one reply containing the chunk text in order; thought
    chunks and tool calls are absent; text around a tool call is separated by
    a blank line;
  - a second message during a prompt waits and runs after it; the 11th
    queued message is refused;
  - `/stop` sends `session/cancel` and clears the queue; `/new` makes the
    next message call `session/new`;
  - permission: `/allow` answers `allow_once`, `/deny` `reject_once`, the
    timeout `reject_once`, `/stop` `cancelled`; `/allow` from a non-admitted
    sender is ignored;
  - `session/load` that replays 5000 updates returns, sends nothing to the
    chat, and the next prompt's reply contains only that prompt's text;
  - the agent exiting mid-prompt produces one error message to the chat, and
    the next message starts a new agent and loads the session;
  - admission: wrong chat, wrong sender, and empty lists admit nothing;
  - a message older than 30 minutes is not sent to the agent and the chat
    is told its time.
- `internal/telegram`, against an `httptest` Bot API: offset persistence
  and resume, group mention and reply filtering, `/cmd@bot`, 4096-character
  splitting at line boundaries, attachment and caption handling.
- `internal/feishu`: config-to-policy mapping (including that empty lists
  never produce an open SDK policy), `NormalizedMessage` to `Message`
  mapping, and `Send` arguments. The WebSocket
  connection is not exercised offline.
- `internal/config`: secret keys in the file and unknown keys fail.
- End to end: when `OTTO_BIN` is set (`make connect-check` sets it to
  `target/debug/otto`), a test starts the real `otto acp` with a Go `httptest` fake
  OpenAI-compatible provider and a fake platform, sends two messages,
  restarts the agent, and checks the replies and the `session/load` resume.

Manual acceptance before merge: a real Telegram bot and a real Feishu app,
one direct chat and one group each: prompt, tool call, `/allow`, `/deny`,
`/stop`, `/new`, connector restart with resume, and a message sent while the
connector was stopped.

## Changes outside `connect/`

- `AGENTS.md`: task map entry for `connect/`; the safety rule names the bot
  token and app secret as environment-only secrets.
- `docs/development.md`: Go toolchain and the `connect-check` gate.
- `docs/user-manual.md`: a "Chat connector" section; the "ACP agent server"
  section keeps the protocol reference and drops the cc-connect setup and
  its limitation notes.
- `README.md`: one line.
- The ACP agent server design: a note at the top that its cc-connect section
  is superseded by this document.

## Delivery in three changes

1. `connect/` with the agent, bridge, config, state and Telegram, the gates
   and CI, the end-to-end test, docs for Telegram.
2. Feishu adapter and its docs.
3. Removal of `[inbound.feishu]` (decision D5) and its docs.

## Decisions

- D1. `otto-connect` is a Go module at `connect/` in this repository, built
  and tested by `make check` and `make check-linux`. The alternative is a
  separate repository with its own CI.
- D2. The ACP client is `coder/acp-go-sdk` v0.13.5. The alternative is a
  hand-written client of about 300 lines; cc-connect's hand-written client
  had the replay deadlock reported in cc-connect issue 1941.
- D3. Replies are sent once per turn. Streaming edits are a later change.
- D4. Permission requests are answered with `/allow` and `/deny` text, with
  a 10-minute timeout that denies. Buttons (Telegram inline keyboard, Feishu
  card) are a later change.
- D5. `[inbound.feishu]` and its `lark-cli` dependency are removed after the
  Feishu adapter ships. This removes a documented `otto serve` option.
- D6. cc-connect is no longer documented. Issue #264 is closed as not
  planned; #265 (acceptance), #266 (cron) and #267 (login service) are
  rewritten for `otto-connect`.

## Not in this change

- Scheduled prompts. With the connector in place they belong in the
  connector or in otto; that is a separate design (#266).
- Running as a login service (#267).
- Delivering otto's background results (for example `remind`) to the chat
  outside a prompt: `otto acp` sends no updates outside a prompt.
- Streaming replies, buttons, attachments, session listing and switching
  from the chat (later added as `/sessions` and `/use`; see
  [shared session](2026-10-02-shared-session.md)), more than one workspace
  per connector process, platforms other than Telegram and Feishu.
