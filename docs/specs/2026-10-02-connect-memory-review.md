# Memory review from the chat connector

Status: approved 2026-10-02 and implemented.

## Problem

`remember` and `forget` only queue a pending memory candidate. Deciding it
is a human act: `Service::review` records `Origin::Human`, and the only
caller is the REPL's `/memory review` (`cli/repl_commands.rs`). A person
using otto through Telegram or Feishu has no way to decide a candidate. When
they write "approve" in the chat the message reaches the model, which can
only treat it as conversation. The model's explanation in that situation is
correct (it has no review tool) but the person is still stuck.

## Decision

Review goes through the connector, the human side of the chat, and never
through the model.

- `otto acp` gains two ACP extension methods (names start with `_`, as ACP
  extensibility requires), served only by the local backend:
  - `_otto/memory/pending` with `{sessionId}` returns the pending
    candidates visible to that session's controller: `id`, `action`, `kind`,
    `key`, `text`, `reason`, `origin`, `scope`.
  - `_otto/memory/review` with `{sessionId, candidateId, decision}` where
    `decision` is `accept` or `reject`. It calls `Service::review` with no
    edit and no target revision, and returns what `/memory review` prints:
    the resulting record, tombstone, or rejection.
- `initialize` advertises `agentCapabilities._meta.otto.memoryReview = true`
  when the backend is local. Whether memory is usable is known only per
  session, so the methods answer an internal error ("memory is not available
  in this session") when it is not. `--attach` mode does not advertise it and
  answers the methods with `-32601`.
- `otto-connect` handles three commands before the queue, like `/new`:

  | Command | Effect |
  | --- | --- |
  | `/memory` | Lists the chat session's pending candidates. |
  | `/memory accept <id>` | Accepts one candidate. |
  | `/memory reject <id>` | Rejects one candidate. |

  `<id>` may be a unique prefix, as for `/use`. The commands are exact
  matches in the sense of the existing command table: other text starting
  with `/memory` goes to the queue. They work while a prompt is running and
  need an existing session; with none the chat is told so. If the agent does
  not advertise `memoryReview`, the chat gets "this agent does not support
  memory review".
- Admission is unchanged: only admitted senders in admitted chats reach the
  command handler, the same trust as `/allow`.

## Why the human boundary holds

- No tool definition is added; the model's tool list is unchanged.
- The extension methods are JSON-RPC requests from the client. The model
  cannot send them, and text it writes in a reply is never parsed as a
  command by the connector (only inbound platform messages are).
- A prompt that contains `/memory accept x` goes to the connector's command
  handler, not to the agent, so the agent never sees it as an instruction.
- Decision source stays `Origin::Human` inside `Service::review`.

## Non-goals

Editing a candidate during review, `target_revision`, bulk accept, listing
accepted records, `forget` from chat, and the `--attach` backend.

## Tests

- Rust (`acp`): `pending` returns only pending candidates; `review accept`
  creates a record, `reject` writes none; unknown id and invalid decision
  fail with `-32602`/`-32002`; methods are absent from `otto acp --attach`
  and from initialize there; a guard test fails if a model-facing tool
  definition is named or described as review.
- Go (`connect`): the fake agent serves both methods; command parsing and
  prefix matching; unsupported agent message; commands bypass the queue and
  work during a running prompt; admission still applies.

## Docs touched with the change

User manual ("Chat connector" and "ACP agent server"), the
[chat connector](2026-10-02-otto-connect.md) command table, and the
[ACP agent server](2026-10-02-acp-agent-server.md) method list.

## Implementation notes

`coder/acp-go-sdk` v0.13.5 supports extension calls
(`ClientSideConnection.CallExtension`), so the connector needs no raw
JSON-RPC wrapper. A unique candidate prefix needs at least 4 characters and
is resolved by the connector against the pending list. A source guard
(`crates/otto/tests/memory_review_boundary.rs`) fails if anything under
`src/tool/` can decide a candidate.
