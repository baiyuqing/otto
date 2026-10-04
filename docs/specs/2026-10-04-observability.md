# Observability: correlation, logs, and metrics

Status: approved 2026-10-04.

This design extends the "Diagnostics and observability" section of the
[connect management and diagnostics design](2026-10-03-connect-management-and-diagnostics.md).
Its never-log list and its field table still apply; this document adds the
request path for normal chat prompts, the missing log events, and metrics for
`otto-connect`.

## Consumers

The data is read on the same host by `grep` over process logs and by a local
Prometheus that scrapes `/metrics`, with Grafana on top of Prometheus. There is
no log store, trace store, or OpenTelemetry collector. This design therefore
adds no OpenTelemetry SDK, OTLP exporter, or span store. "Tracing" here means:

1. one `request_id` that is carried from the chat message in `otto-connect`
   to the turn in `otto serve`; and
2. one log event per provider request and per tool call, each carrying
   `turn_id` and `duration_ms`.

`grep request_id=<id>` over the connect log returns `session_id` and
`turn_id`; `grep turn_id=<id>` over the serve log returns the turn's provider
requests and tool calls in order, each with its duration.

## Request ID path

| Hop | Carrier | Owner |
| --- | --- | --- |
| Platform message admitted by connect | New `request_id`: 32 lowercase hex characters from 16 random bytes | `connect/internal/bridge` |
| connect to ACP agent | `session/prompt` `_meta.otto.requestId`, next to `_meta.otto.chat` and `_meta.otto.channel` | `connect/internal/agent` |
| `otto acp` | Parsed with the same rule serve applies to `X-Request-Id` (`request_id`: printable ASCII, at most 64 bytes); an empty result is treated as absent | `crates/otto/src/acp` |
| ACP relay to serve (`--attach` and serve's `--acp-listen`) | `X-Request-Id` header on `POST /v1/sessions/{id}/turns` | `crates/otto/src/client`, `crates/otto/src/acp/attach.rs` |
| serve | The client-supplied `X-Request-Id` is stored on the turn and written on every turn and step event | `crates/otto/src/server` |

serve's HTTP middleware already reads `X-Request-Id`, logs it as `request_id`
on `http_request`, echoes it in the response, and generates a random value
when the client sent none. Using the same header for turns makes
`http_request.request_id` and `turn_*.request_id` equal for a relayed prompt.
Only a client-supplied value is stored on the turn; a turn posted without the
header has no `request_id`. The management API (`/v1/config/...`) also reads
`X-Request-Id`, and still accepts the `X-Otto-Request-Id` header that earlier
connect builds send.

Connect sends `requestId` whenever it has one, independent of the channel
fields. Otto ignores unknown `_meta.otto` keys, so an older Otto ignores it.
The request ID is not authority for anything: it is never used for identity,
access control, or deduplication. A management command uses the request ID of
the message that carried it.

Local `otto acp` (no `--attach`, no serve) parses the ID but writes no turn
events; its stderr is already logged by connect as `agent stderr:` lines.

## Log format

Both processes keep their existing line format: `time=<RFC 3339> level=<LEVEL>
msg=<event> key=value ...`, values quoted when they contain spaces or quotes.
Go's `slog.TextHandler` writes the same shape, so one `grep` expression works on
both logs. Levels are `INFO`, `WARN`, and `ERROR`. There is no level setting.

`msg` is a stable snake_case event name. Existing prose messages on the paths
below are renamed to event names; the old text moves to a field where it
carried information.

Both processes write to stderr only. Writing to a file and rotating it is the
process supervisor's job (launchd `StandardErrorPath` with `newsyslog`, or the
systemd journal).

### Otto logger

`server::Logger` moves to `crates/otto/src/log.rs` and gains `warn` and a
discarding constructor. One `Arc<Logger>` is built per process at the
composition root and passed to its consumers; there is no global logger.
`otto serve` and `otto acp` write to stderr. The terminal UI uses the
discarding logger, because stderr output would corrupt the terminal screen.

### otto serve events

Existing: `http_request`, `turn_started`, `turn_finished`, `turn_error`,
`wake_turn_error`, `session_close_error`, `error`.

Changes and additions:

| Event | Level | Fields |
| --- | --- | --- |
| `http_request` | INFO | unchanged: `method`, `route`, `status`, `duration_ms`, `request_id` |
| `turn_started` | INFO | `session_id`, `turn_id`, `trigger`, `request_id` when known, `inbox_kind` on wake turns |
| `provider_request_finished` | INFO | `session_id`, `turn_id`, `request_id`, `provider`, `model`, `status`, `attempts`, `duration_ms` |
| `tool_call_finished` | INFO | `session_id`, `turn_id`, `request_id`, `tool`, `status` (`ok` or `error`), `duration_ms` |
| `turn_finished` | INFO | existing fields, plus `session_id`, `request_id`, `provider_requests`, `provider_ms`, `tool_calls`, `tool_ms`, `input_tokens`, `output_tokens` |
| `turn_error` | ERROR | existing fields, plus `session_id`, `request_id` |
| `schedule_run_finished` | INFO | `schedule_id`, `due_at`, `outcome`, `duration_ms` |
| `schedule_delivery_finished` | INFO/WARN | `channel`, `outcome` (`ok` or `error`), and on failure at WARN the `error` text the store records |
| `schedule_store_error` | WARN | `operation`, `error` |
| `workflow_step_finished` | INFO | `run_id`, `workflow`, `step_id`, `status`, `duration_ms` |
| `workflow_run_finished` | INFO | `run_id`, `workflow`, `status`, `duration_ms` |
| `mcp_server_connected` | INFO | `server`, `transport`, `tools` |
| `mcp_server_failed` | WARN | `server`, `transport`, and the `error` text `/mcp` shows |

`request_id` is omitted, not written empty, when the turn has none. Events
never contain prompt text, model output, tool arguments, or tool results.

The schedule, workflow, and MCP events come from the process that runs that
code: `otto serve`, `otto acp`, and `otto workflow` each write them to stderr.
Field values:

- `schedule_run_finished.outcome` is `reply`, `failed_before_tools`, or
  `failed`. The event and `otto_schedule_runs_total` are written only after
  the store records the run; a failed store write is `schedule_store_error`
  instead. `schedule_delivery_finished` and `otto_schedule_deliveries_total`
  are written before the store write and do not depend on it. `channel` is the
  chat platform (`telegram`, `feishu`) or the origin (`terminal`, `web`).
  `schedule_store_error.operation` is one of `sweep`, `claim_run`,
  `claim_deliveries`, `finish_run`, `delivered`, `delivery_failed`.
- `workflow_step_finished.status` is `succeeded`, `failed`, `canceled`, or
  `interrupted` (the run's deadline passed). `workflow_run_finished` is
  written once, when the run reaches `succeeded`, `failed`, or `canceled`; a
  run paused by a deadline writes no run event. Its `duration_ms` is the time
  since the run was created, approval waits included, and is omitted when the
  stored creation time cannot be read.
- `mcp_server_connected.tools` is the number of tools kept after name
  filtering. A server that needs `otto mcp login` or is disabled writes no
  event.

### otto-connect events

| Event | Level | Fields |
| --- | --- | --- |
| `connector_started` | INFO | `platforms`, `agent_mode` (`process` or `address`), `metrics_listen` |
| `connector_stopping` | INFO | `reason` (`signal` or `error`) |
| `chat_received` | INFO | `request_id`, `platform`, `chat_id`, `sender_id`, `message_id` |
| `chat_rejected` | INFO | `platform`, `chat_id`, `sender_id`, `reason` |
| `chat_queued` / `chat_queue_full` | INFO / WARN | `request_id`, `platform`, `chat_id`, `queue_length` |
| `session_created` / `session_loaded` | INFO | `request_id`, `platform`, `chat_id`, `session_id` |
| `session_load_failed` | WARN | `request_id`, `platform`, `chat_id`, `session_id`, `error` |
| `turn_started` | INFO | `request_id`, `platform`, `chat_id`, `session_id` |
| `turn_finished` | INFO | `request_id`, `platform`, `chat_id`, `session_id`, `stop_reason`, `duration_ms`, `reply_bytes` |
| `turn_failed` | WARN | `request_id`, `platform`, `chat_id`, `session_id`, `duration_ms`, `error` |
| `chat_reply_sent` / `chat_reply_failed` | INFO / WARN | `request_id`, `platform`, `chat_id`, `bytes`, and `error` on failure |
| `agent_started` | INFO | `pid`, `command` (argv[0] only) |
| `agent_exited` | WARN | `status` |
| `agent_restart_scheduled` | INFO | `delay_ms` |
| `metrics_listen_failed` | WARN | `address`, `error` |

`chat_reply_sent` is written for agent replies only. `chat_reply_failed` is
written for every failed send; a connector notice has an empty `request_id`.
The Feishu SDK refuses some messages before they reach the bridge, so the
Feishu adapter writes `chat_rejected` and counts the rejection itself.

Existing management events keep their names and gain `chat_id` in place of
`chat`. Other log lines outside this table keep their prose messages and the
`chat` field. Platform SDK pass-through messages (`feishu sdk`, Telegram polling
errors) are out of scope.

## Metrics

### otto serve

Prometheus cannot scrape a Unix socket. To scrape serve while connect uses the
socket, start serve with both listeners:

```text
otto serve --socket ~/.otto/otto.sock --listen 127.0.0.1:9464
```

`/metrics` and `/healthz` need no token on the TCP listener. This uses existing
behavior; the config file still cannot select both listeners.

Additions to the existing families:

| Metric | Type | Labels |
| --- | --- | --- |
| `otto_build_info` | gauge, always 1 | `version` (`crate::VERSION`) |
| `process_start_time_seconds` | gauge | none |
| `otto_schedule_runs_total` | counter | `outcome` |
| `otto_schedule_deliveries_total` | counter | `channel`, `outcome` |

### otto-connect

Connect serves `GET /metrics` (Prometheus text format 0.0.4) and
`GET /healthz` on a loopback address. The text format is written directly, as
`crates/otto/src/server/metrics.rs` does; no Prometheus client library is added.

New configuration:

```toml
[metrics]
listen = "127.0.0.1:9465"   # default; "" turns the endpoint off
```

A non-loopback host is a configuration error. If the port cannot be bound,
connect logs `metrics_listen_failed` and keeps running without the endpoint.

| Metric | Type | Labels |
| --- | --- | --- |
| `otto_connect_build_info` | gauge, always 1 | `version` |
| `otto_connect_messages_total` | counter | `platform`, `outcome` (`accepted`, `rejected`, `queue_full`) |
| `otto_connect_turns_total` | counter | `platform`, `stop_reason` (ACP stop reason, or `error`) |
| `otto_connect_turn_duration_seconds` | histogram | `platform` |
| `otto_connect_replies_total` | counter | `platform`, `outcome` (`ok`, `error`) |
| `otto_connect_agent_restarts_total` | counter | none |
| `otto_connect_agent_up` | gauge | none |

`messages_total{outcome="accepted"}` counts every admitted message that a full
queue did not refuse, including commands and stale messages; the outcomes are
exclusive. `replies_total` counts sends of agent replies only.
`agent_restarts_total` increases each time an ended agent process or
connection is replaced. `agent_up` is 1 while at least one started agent has
not ended. `version` is the main module version plus `+` and the first 7
characters of `vcs.revision` when present.

No metric is labelled by chat, sender, session, or request ID; those values
grow without bound and belong in logs.

## Limitations

- Local `otto acp` without serve writes no turn or step events.
- `tool_call_finished.duration_ms` uses the turn's single tool start time, the
  same value `otto_tool_call_duration_seconds` already uses; for tool calls
  that overlap, the duration is measured from the most recent start.
- There is no log level setting and no debug-level frame logging.

## Tests

- serve: a turn posted with `X-Request-Id` writes `turn_started`,
  `provider_request_finished`, `tool_call_finished`, and `turn_finished` with
  that ID; a turn without the header writes no `request_id` field.
- acp relay: `_meta.otto.requestId` on `session/prompt` reaches serve as the
  header; an invalid value is dropped.
- schedule, workflow, MCP: each new event is written by the code path that
  owns it, checked through a captured logger.
- serve metrics: the new families render with the documented names and labels.
- connect: one admitted message produces `chat_received` through
  `chat_reply_sent` with the same `request_id`, and the prompt carries it in
  `_meta.otto.requestId`; `/metrics` renders every family; a busy port logs
  `metrics_listen_failed` and the connector still runs; a non-loopback
  `listen` is rejected by config validation.
