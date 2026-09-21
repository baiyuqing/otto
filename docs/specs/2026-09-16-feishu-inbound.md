# Inbound contract (Feishu first)

Status: **approved** 2026-09-16. This change implements the contract below.

Superseded on 2026-09-21: `chat_ids` is an allowlist, not an optional
filter. An empty list admits nothing and `otto serve` starts no consumer.
The "empty → do not filter by chat" behavior described below is historical.

Otto's turn loop, inbox, and idle wake already accept later messages. The
missing piece is a host-side producer that pushes work-context into the
inbox. The first producer is Feishu. This is not a new Provider, skill, or
Connector trait.

## Objective

`otto serve` optionally consumes Feishu group and direct-chat text and turns
it into the existing inbox `Message` notification, which starts a wake turn
when the session is idle. Credentials, the long-lived connection, and event
subscription stay in `lark-cli`. Otto owns configuration, process lifetime,
NDJSON → `Notification`, and fan-out.

## Scope

- In: `[inbound.feishu]` (off by default), a `lark-cli event consume`
  supervisor, `Controller::notify`, fan-out to every open session, serve
  logs, and the user manual.
- Out: replying in Feishu, a TUI/REPL consumer, Telegram, a Connector trait,
  a new Provider, changing the HTTP `trigger: user|task` contract,
  `OTTO_TRACE`, automatic memory extraction, office skills, and per-workspace
  routing.

## Decisions

**1. The inbox is the only inlet.**

The payload is the existing
`Notification { kind: Some(Message), task_id: "", text, usage: None }`.
`text` starts with `[feishu]` (same shape as `[timer]`), then `chat_type`,
`chat_id`, `from sender_id`, and the body. The Feishu `message_id` lives in
that text only.

`ContextMetadata.task_id` accepts only `t` plus digits. Feishu `om_…` ids
and the remind tool's `"timer"` are invalid. `deliver_notifications` must
omit `context_metadata` when `task_id` fails `ContextMetadata::validate`, or
append rejects the message.

Host adapters call `Controller::notify`. A closed controller drops the
notification. No current runner returns false.

**2. Reuse the existing wake path.**

The inbox stays on the session `Tasks` registry (`on_change` → `updates`).
`otto serve`'s wake loop already watches that signal. `[agents].enabled =
false` disables wake the same way it disables `remind`. This change does not
decouple the two.

HTTP `trigger` stays `task`. Wake `turn_started` logs add `inbox_kind=message`
when a `Message` notification is pending. OpenAPI and wasm stay unchanged.

**3. Routing: every open session.**

`Server::notify_open_sessions` clones one notification to every open
controller. A non-empty `chat_ids` list filters on Feishu `chat_id`; empty
means every chat. TUI and REPL processes do not spawn `lark-cli`.

**4. `lark-cli` is a transport subprocess, not a skill.**

When enabled, `otto serve` spawns:

`lark-cli event consume im.message.receive_v1 --as bot`

stdin/stdout/stderr are pipes. Each stdout line is one NDJSON object.
Interactive cards, empty bodies, invalid JSON, and chats outside `chat_ids`
are skipped silently. A `ready` line on stderr is an info log; `"ok":false`
is an error log. `NotFound` logs an error and disables inbound; serve keeps
running. Other spawn failures wait 2s and retry. On cancel or exit: stdin
EOF plus SIGTERM, wait 5s, never SIGKILL. Credentials stay in `lark-cli`'s
own store.

**5. Tight config, no secrets.**

```toml
[inbound.feishu]
enabled = false          # default is off
binary = "lark-cli"      # empty or whitespace → this default
chat_ids = []            # empty → do not filter by chat
```

`deny_unknown_fields`. A `token` / `app_secret` key fails config load. Otto
does not read Feishu secrets from its environment.

## Ownership

| Layer | Owns |
|---|---|
| `otto-core` | Skip invalid `task_id` context metadata; Inbox / Event types stay |
| `crates/otto` `app` | `Controller::notify` |
| `crates/otto` `inbound` | NDJSON parse + supervisor (native only) |
| `crates/otto` `server` / `cli/serve` | Fan-out and process lifetime |
| `otto-web` / `ui/` | Unchanged |

## Errors and safety

- Missing `lark-cli` is not a serve failure.
- Parse skips do not enter the inbox.
- Logs may include the binary name, `chat_id`, and error strings; never
  message bodies or credentials.
- Notification text goes through the existing redactor, like every other
  inbox item.
- File tools, the sandbox, and the workspace boundary are unchanged. A model
  that wants to reply in Feishu uses a user-installed skill and `bash`; this
  change does not wire outbound.

## Acceptance

1. Enabled plus a fake `lark-cli` writing one text NDJSON line → every open
   session inbox has one `[feishu]` `Message`, and idle sessions wake.
2. `message_type=interactive`, empty content, and chats outside `chat_ids`
   do not wake.
3. A notification with `task_id=""` appends (no illegal metadata).
4. `enabled=true` with a missing binary → serve starts and logs an error.
5. `token = "..."` in the Feishu table → config load fails.
6. Cancelling serve sends SIGTERM to the child, not SIGKILL.
7. Default configuration matches today's behavior.

## Non-goals

Outbound replies, calendar/doc retrieval, per-session routing, a TUI
consumer, `OTTO_TRACE`, a new `NotificationKind`, and detaching the inbox
from `Tasks`.
