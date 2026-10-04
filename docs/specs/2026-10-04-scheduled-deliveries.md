# Scheduled deliveries (`schedule` tool)

Status: approved 2026-10-04. The user confirmed D1-D9 during the design
discussion and asked to write this spec and implement it without a further
approval round.

## A fired `remind` timer reaches the user only through a model-written wake reply

`remind` (`crates/otto/src/tool/remind.rs`) is a timer for the model: when it
fires, a `[timer]` notification starts a wake turn in the same session, and the
user sees whatever text that turn produces. Verified on `e8b58a9`:

- `deliver_notifications` (`otto-core/src/agent/mod.rs`) appends the `[timer]`
  entry and removes it from the inbox before the provider call. A provider
  error in the wake turn therefore loses the reminder; nothing retries it.
- An empty wake reply sends nothing.
- `otto-connect` sends a wake reply once (Telegram 429 is retried up to 3
  times) and keeps wake replies in a 20-entry in-memory queue.
- `remind` accepts 1-3600 s and 8 timers per session, and timers run only while
  a process has the session loaded.
- tokio and Go timers use monotonic clocks; on macOS Rust `Instant` uses
  `CLOCK_UPTIME_RAW`, which does not advance during system sleep.
- The system prompt carries only the UTC date (`<date>`,
  `cli/workspace_context.rs`), computed when the session is built.

Users also want results that need a model run at the due time, for example
"every day at 08:00 tell me whether it will rain in Beijing".

## Decisions

- D1. otto owns scheduling, due detection, delivery state and retries.
  `otto-connect` only sends the text it is given and reports success or
  failure.
- D2. Scheduled items live in one shared Turso database,
  `~/.otto/schedule.db`, not in session sidecars.
- D3. An item is either fixed `text` or a `prompt`. A prompt runs in a new
  session, in the workspace where the item was created, and its final reply is
  delivered.
- D4. An item is delivered to its origin: the chat it was created in, any
  running TUI/REPL on this host, or the Web UI. Items belong to the origin, not
  to the session, so they survive `/new` and `/use`.
- D5. Delivery is at-least-once. Failed attempts are retried with a backoff
  of 30 s doubling to a cap of 30 min. An occurrence that is not delivered
  within 24 h of its due time is marked failed.
- D6. One-shot items are due at `seconds` from now (up to 30 days) or at a
  local time `at`. Recurring items repeat `daily` or on `weekdays` at a local
  `HH:MM`. There is no cron syntax.
- D7. A recurring item that missed occurrences while no process could run it
  gets one catch-up run for the most recent missed occurrence, if that
  occurrence is less than 24 h old. Older occurrences are skipped.
- D8. After a successful delivery, the delivered text is appended to a
  session as a context entry at the start of the next user turn, so a
  follow-up question can refer to it. Only the final text is appended, not the
  run's tool calls.
- D9. The text shows `⏰ ` before the message or reply, and a late line when it
  is delivered more than 2 min after the due time.

Changes from the earlier draft, which the user approved as defaults 1-6:

- A separate `schedule` tool instead of a `notify_user` flag on `remind`. The
  new tool takes `text` or `prompt`, `seconds` or `at`, and `repeat`; a flag
  would put two tools with different lifetimes behind one name. `remind` is
  unchanged.
- The file is `schedule.db` and ids are `s<n>`, matching the module and tool
  name, instead of `reminders.db` and `u<n>`.

## The `schedule`, `schedule_list` and `schedule_cancel` tools

`schedule` arguments (strict JSON, unknown fields rejected):

| Field | Type | Rule |
|---|---|---|
| `text` | string | 1-500 characters after trimming. Delivered as `⏰ <text>`. |
| `prompt` | string | 1-2000 characters after trimming. Run at the due time; the reply is delivered. |
| `seconds` | integer | 1-2 592 000 (30 days). |
| `at` | string | `HH:MM` (next occurrence after now) or `YYYY-MM-DD HH:MM`, host time zone, at most 30 days ahead. |
| `repeat` | string | `daily` or `weekdays`. Requires `at` in `HH:MM` form. |

Exactly one of `text` and `prompt`, and exactly one of `seconds` and `at`,
must be present. Each origin may have at most 32 active items (state `waiting`
or `ready`).

The result names the item, its first due time with the UTC offset, the delay,
the target, and the current local time, so the model can check a date it
computed:

```
scheduled s7: prompt, first run 2026-10-05 08:00 +08:00 (in 14h40m), repeats daily; delivered to this Telegram chat
now: 2026-10-04 17:20 +08:00
```

`schedule_list` (no arguments) prints `now:` and one line per item of the
current origin: active items, then items that finished, failed or were
cancelled in the last 7 days, with the last error of failed items.
`schedule_cancel {"id": "s7"}` cancels an active item of the current origin;
a recurring item stops for good.

When the turn has no origin (see below), all three tools return an error that
says scheduled delivery is unavailable in this conversation and why. When
`~/.otto/schedule.db` could not be opened at startup, they return an error
that names the file.

The tool descriptions state the choice: `schedule` when the user asks to be
told something later or repeatedly, `remind` for the model's own wait inside
the current conversation. `remind`'s description gets the same sentence.

## Every user turn has an origin

| Origin | Set by |
|---|---|
| `terminal` | local TUI, REPL and `otto --prompt`; `otto --attach` sends it in the HTTP start-turn body |
| `web` | HTTP start-turn without `origin` (the browser UI) |
| `chat:<platform>:<chat id>` | ACP prompt `_meta.otto.chat`, or HTTP start-turn `origin` from the ACP relay |
| none | ACP prompts without `_meta.otto.chat`, scheduled runs, sub-agents, workflows, and wake turns before the first user turn of the loaded session |

`<platform>` is `telegram` or `feishu`. `<chat id>` is 1-128 characters with no
whitespace or control characters. The HTTP API rejects an invalid `origin`
with 400. An invalid `_meta.otto.chat` on an ACP prompt is ignored with a
warning on stderr and the turn has no origin. When both `channel` and a chat origin are given, their
platforms must match: the HTTP API returns 400 otherwise, and ACP ignores the
origin.

`app::Controller::run_with_channel` takes the origin next to the channel. A
user turn records its origin in the session's runner, where the schedule
tools read it; a wake turn uses the origin of the last user turn.

## Store: one row per item, one row per delivered occurrence

`schedule::Store` follows `subagent::record::Store`: parent directory 0700,
file 0600, `busy_timeout` 5 s, WAL, `PRAGMA user_version` = 1, errors that do
not carry database text, and `open_in_memory` for tests. It is opened in
`cli/run.rs` beside `tasks.db`; a failure is a warning and disables only the
schedule tools and workers.

```sql
CREATE TABLE items (
    id INTEGER PRIMARY KEY,       -- shown as s<id>
    origin TEXT NOT NULL,
    session_id TEXT NOT NULL,     -- the creating session
    workspace TEXT NOT NULL,      -- canonical path of the creating session
    kind TEXT NOT NULL,           -- text | prompt
    body TEXT NOT NULL,
    repeat TEXT NOT NULL,         -- '' | daily | weekdays
    local_time TEXT NOT NULL,     -- HH:MM for recurring items, '' otherwise
    due_at INTEGER NOT NULL,      -- unix ms of the current occurrence
    state TEXT NOT NULL,          -- waiting | ready | done | failed | cancelled
    result TEXT NOT NULL,         -- text to deliver while ready
    attempts INTEGER NOT NULL,    -- failed attempts in the current phase
    next_try_at INTEGER NOT NULL, -- unix ms
    claimed_until INTEGER NOT NULL,
    last_error TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE deliveries (
    item_id INTEGER NOT NULL,
    due_at INTEGER NOT NULL,
    origin TEXT NOT NULL,
    session_id TEXT NOT NULL,
    text TEXT NOT NULL,
    delivered_at INTEGER NOT NULL,
    appended INTEGER NOT NULL,    -- 0 until written back into a session
    PRIMARY KEY (item_id, due_at)
);
```

All times are compared against the wall clock (`Utc::now()`) on every check,
so system sleep delays a check but does not shift due times.

### State of the current occurrence

- `waiting`, kind `text`, due: `result = "⏰ " + body`, state `ready`.
- `waiting`, kind `prompt`, due: a process that can run it claims it
  (`claimed_until = now + 15 min`) and runs it.
  - Success: `result = "⏰ " + reply`, or `⏰ Scheduled prompt s7 finished
    with no reply.` when the reply is empty; state `ready`, `attempts = 0`.
  - Failure before any tool call started, and less than 24 h after the due
    time: `attempts + 1`, `next_try_at` from the backoff, still `waiting`.
  - Any other failure: `result = "⏰ Scheduled prompt s7 failed: <error>"`
    (error cut to 300 characters), state `ready`. A tool may have changed
    something, so the run is not repeated.
- `ready`: a process that delivers to the origin claims it
  (`claimed_until = now + 3 min`) and sends it.
  - Success: insert the `deliveries` row, then advance (recurring) or set
    `done`.
  - Failure: `attempts + 1`, `next_try_at = now + min(30 s * 2^(attempts-1),
    30 min)`, `last_error` set.
- An active occurrence whose due time is more than 24 h ago: a one-shot item
  becomes `failed` with `last_error = "not delivered within 24 h of the due
  time"`; a recurring item records that error and advances.
- Advance: `due_at` becomes the next occurrence of `local_time` after now
  (`weekdays` skips Saturday and Sunday), `state = waiting`, `result = ''`,
  `attempts = 0`.
- Catch-up (D7): before a recurring `waiting` item runs, `due_at` moves to the
  most recent occurrence at or before now. If that occurrence is more than
  24 h old, the item advances instead and nothing runs.
- `cancelled` is set only by `schedule_cancel`.

Claims are taken in one `BEGIN IMMEDIATE` transaction that selects a row with
`next_try_at <= now` and `claimed_until < now` and writes the new
`claimed_until`. A process that exits during a claim leaves the row to be
claimed again after the lease; a prompt run can therefore repeat. Items and
deliveries that ended more than 7 days ago are deleted by every worker's
periodic sweep.

Local times are computed with `chrono::Local`. A local time that does not
exist because of a daylight-saving change moves forward to the first valid
minute; an ambiguous one uses the earlier instant.

## Which process runs and which delivers

One worker type, `schedule::Worker`, runs in every long-lived process. It is
given the origins it may run, a way to run prompts (or none), and a sink for
the origins it delivers to. It loops: sweep, run claimable prompts, deliver
claimable ready items, then sleep until the earliest relevant due time, at
most 15 s, or until the in-process store signals a new item.

| Process | Runs prompt items | Delivers |
|---|---|---|
| local TUI and REPL | `terminal`, same workspace | `terminal` |
| `otto --prompt` | none | none |
| `otto --attach` TUI | none | `terminal` |
| `otto acp` (local) | `chat:<platform>:*` for platforms the client declared, same workspace | the same chats, over its ACP connection |
| `otto acp --attach`, ACP TCP connections of `otto serve` | none | `chat:<platform>:*` for platforms the client declared |
| `otto serve` | every origin, any workspace | `web`, through the HTTP routes below |

Prompt runs use the existing in-process path. `Controller::create(builder,
runtime)` creates a new session in the item's workspace (`otto serve` loads
the workspace with its factory). `run_with_channel(Step::Text, None, no origin,
..., decide = Deny)` then runs:

```
[scheduled run s7: created 2026-10-04 17:20 +08:00 from telegram chat; due 2026-10-05 08:00 +08:00; now 2026-10-05 08:00 +08:00. No user is present; approval requests are denied.]

<prompt>
```

The reply is collected the way wake replies are: text deltas, with a blank
line after each tool call. The collector is shared with wake turns
(`app::scheduled::ReplyCollector`). The run is cancelled after 10 min. The session is kept and appears in
the session list. Sub-agent tasks it starts are not awaited.

## Delivery to each frontend

- TUI (local and attach): a system line with the text, and the terminal bell.
  The worker hands the text to the TUI event loop and counts it as delivered
  when the loop has accepted it.
- REPL: the text is printed as a line, the same way wake output is.
- Web UI: `POST /v1/schedule/claim` claims up to 10 ready `web` items for
  2 min and returns `{"items": [{"id", "due_at", "text"}]}`;
  `POST /v1/schedule/{id}/delivered {"due_at"}` records the delivery. The page
  calls claim every 15 s while it is open, shows each text as a notice, and
  then posts `delivered`. With several tabs open, one tab shows each item.
- Chat: the agent sends the ACP extension request
  `_otto/schedule/deliver {"chat": "telegram:123", "text": "..."}` to the
  client and waits up to 60 s. A `{}` result is success; a JSON-RPC error or a
  timeout is a failed attempt.

The late line (D9) is added at delivery time:
`(scheduled for 2026-10-05 08:00 +08:00, delivered 2h3m late)`.

## Write-back into the conversation

At the start of a user turn with an origin, before the user message, otto
appends each `deliveries` row with `appended = 0` that matches. At most 10 rows
are appended, oldest first. A row matches when:

- for a chat origin, the row's origin is the turn's origin. This is the
  session the chat is bound to now, which `otto-connect` decides;
- for `terminal` and `web`, the row's session is the turn's session and the
  row's origin is not a chat.

Each row is appended as a `Role::Context` message with
`context_type = "scheduled_delivery"`, shown in history:

```
[scheduled s7, due 2026-10-05 08:00 +08:00]
⏰ <delivered text>
```

The row is marked appended after the session append succeeds. A crash between
the two appends it again on the next turn. A store or append error ends the
write-back for that turn without an error to the user and the turn continues;
the rows stay unappended, so the next user turn retries them. Wake turns and scheduled runs append nothing.

## `otto-connect` changes

- `initialize` declares `clientCapabilities._meta.otto.scheduleDelivery` as
  the list of configured platforms, for example `["telegram"]`.
- Prompts carry `_meta.otto.chat = "<platform>:<chat id>"` next to
  `channel`, when the agent advertises `channelContext`.
- The ACP client implements `HandleExtensionMethod`. For
  `_otto/schedule/deliver`, the bridge checks that the platform is configured
  and that the chat id is in that platform's `chats` allowlist. It then sends
  the text as Markdown with `Platform.Send` and returns `{}`, or returns the
  error. Other `_` methods return method-not-found.
- The connector connects to the agent at startup even when no chat is bound,
  and reconnects after the agent exits (with the existing restart delay), then
  loads the bound sessions again. Without a live connection the agent has no
  client to deliver to.

## HTTP API changes

- `POST /v1/sessions/{id}/turns` accepts `origin`: `terminal`, `web`, or
  `chat:<platform>:<chat id>`. Omitted means `web`.
- `POST /v1/schedule/claim` and `POST /v1/schedule/{id}/delivered` as above.
- `crate::client::Client::start_turn_with_channel` sends `origin`: the attach
  TUI sends `terminal`, and the ACP relay sends the chat origin from
  `_meta.otto.chat`.

`openapi.yaml` documents the field and both routes.

## Compatibility

- `remind`, its sidecar file and wake turns are unchanged.
- An older `otto-connect` declares no `scheduleDelivery` and sends no
  `_meta.otto.chat`; its chats have no origin, so the schedule tools return
  the unavailable error there. An older otto ignores the new client metadata
  and never sends `_otto/schedule/deliver`.
- The new HTTP field is optional.

## Tests

All tests are offline and use in-memory stores, fake providers, fake sinks
and fake ACP peers.

- Time rules: `HH:MM` next occurrence, dated `at`, past and too-far times
  rejected, `daily` and `weekdays` advance, weekend skip, catch-up within and
  beyond 24 h.
- Store: insert and the 32-item limit per origin, claim leases and their
  expiry, the 24 h failure, backoff values, retention, write-back selection by
  chat origin and by session.
- Tools: argument validation, the no-origin error, list and cancel limited to
  the origin, the `now:` line.
- Worker: text delivered, the late line, retry after a sink failure, prompt
  run success, a run failure without a tool call retried, a run failure after
  a tool call delivered as a failure text.
- Prompt run: a new session in the item's workspace, an approval request
  denied, the reply collected.
- Write-back: appended once at the next user turn, not on wake turns, the
  chat match follows the origin across sessions.
- ACP: `_meta.otto.chat` parsing, the client capability, the deliver request
  and its error result, the relay sending `origin`.
- Server: an invalid `origin` returns 400, and the claim and delivered routes.
- `otto-connect`: the initialize metadata, the prompt `chat` metadata, deliver
  to an allowed chat and refusal for another chat, and the reconnect after the
  agent exits.

## Known limitations

- A duplicate is possible: a send that timed out after the platform accepted
  it is retried.
- A `terminal` prompt item runs only in a TUI/REPL opened on the creating
  workspace, or in `otto serve`.
- An occurrence with no running process that can deliver it is marked failed
  after 24 h and is visible only in `schedule_list`.
- Two connectors that serve the same platform and chat id can both deliver
  that chat's items.
- A Web UI delivery counts as delivered when one tab has received it.
- The system prompt's `<date>` stays the UTC date of the session build; the
  tool results print the local time.
