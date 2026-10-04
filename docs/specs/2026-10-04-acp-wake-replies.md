# Wake-turn replies over ACP

**Status:** Approved for implementation (2026-10-04). Supersedes "Background
results reach the model at the next prompt" and the first "Not in this change"
item of [ACP agent server](2026-10-02-acp-agent-server.md), and the "Delivering
otto's background results" item of [chat connector](2026-10-02-otto-connect.md).

## Problem

A `remind` timer set from a Telegram chat fires, and no message reaches the
chat.

- `otto acp` (the connector's default agent command) runs no wake turns. The
  `[timer]` notification stays in the session inbox until the next
  `session/prompt`, which delivers it to the model before the user's text.
- `otto acp --attach` and ACP TCP relay only the turns their own
  `session/prompt` started. `otto serve` runs the wake turn, but its reply is
  not forwarded.
- `otto-connect` routes `session/update` only to a chat whose prompt is
  running, and sends the reply when that prompt returns. Updates outside a
  prompt are dropped, except Otto reflection notices.

## Decisions

- D1 (2026-10-04). The text the chat receives is the assistant text the model
  writes in the wake turn, not the stored `remind` message.
- D2 (2026-10-04). Every wake turn's reply is forwarded: a fired timer, a
  finished sub-agent, a `[feishu]` inbound message under `otto serve`.
- D3 (2026-10-04). The connector loads every session bound in its state file
  at startup.
- D4 (2026-10-04). The TUI, the REPL and the Web UI keep their current
  behavior.

## Design

### Producer: `Controller` wake-reply queue

`Controller` holds a second `auto_reflection::Notices` queue, `wake_replies`
(20 entries, ids from 1, `since(after)` cursor, watch signal). It is read
through `wake_replies_since(after)` and `wake_replies_changed()`.

`Controller::run_with_channel` pushes one entry when its first step is
`Step::Wake` and the run ends with any stop other than `Cancelled`. The entry
is the assistant text from the run's `TextDelta` events, with a blank line
between text before and after a tool call, trimmed and redacted. Empty text,
an error or a cancelled run push nothing. The text is taken from events, not
from `history()`, because `history()` is empty in a session that withholds
dynamic content.

The TUI and the REPL run wakes with `WakeOperation::run`, not
`run_with_channel`, and do not read the queue. `otto serve` runs wakes through
`run_with_channel`, so its controllers fill the queue; the Web UI does not read
it.

### `otto acp` runs wake turns

Each session loaded in a local `otto acp` connection has a wake loop. It
checks once when the session opens, then on every sub-agent task registry
update and every release of the session's prompt slot. When the slot is free
(no prompt, no `/queue` worker, no follow-up), it claims the slot, calls
`prepare_wake`, and runs the wake step with no streamed updates. An approval
request in a wake turn is denied. A `session/prompt` that arrives while a wake
turn holds the slot waits for it and then runs. The loop stops when the
connection ends.

### ACP emission

The notice watcher of each session also follows `wake_replies`. Each entry is
sent as one `session/update` `agent_message_chunk` with text content and
`_meta = {"otto": {"wakeReply": true, "replyId": <id>}}`.

| Connection | Source | Start cursor |
| --- | --- | --- |
| `otto acp` | the session's controller | 0 |
| ACP TCP to `otto serve` | the session's controller in `otto serve` | current |
| `otto acp --attach` | `GET /v1/sessions/{id}/wake-replies?after=N`, polled every 1 s | current |

`GET /v1/sessions/{id}/wake-replies` has the response shape and error
behavior of `GET /v1/sessions/{id}/notices`.

### Connector delivery

`agent.WakeReply(update)` accepts an `agent_message_chunk` with text content,
`wakeReply == true` and an integer `replyId >= 1`. `Bridge.Update` sends the
text with Markdown rendering and no reply target to every chat whose stored
session is the update's session. If that chat has a prompt running, the wake
reply is sent after that prompt's reply and is not merged into it.

At startup the connector calls `session/load` once for each distinct session
bound to a chat of a configured platform. A load error is logged and the
binding is kept.

## Known limitations

- A wake turn that writes no assistant text sends nothing.
- Through `otto serve`, a reply produced while no ACP connection has the
  session loaded is not sent later; the cursor starts at the current id.
- An elevated `bash` request in a local `otto acp` wake turn is denied.
- A wake turn has no chat channel context; the Telegram or Feishu guidance
  that a connector prompt adds is absent from it.
- A wake reply is sent once per connection that follows the session. Two
  connector processes bound to the same session both send it.
