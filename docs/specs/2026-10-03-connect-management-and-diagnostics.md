# Connect management and diagnostics

Status: proposed 2026-10-03.

## Goal

Make `otto-connect` the conversational operations entry point for a trusted
Telegram or Feishu user:

1. a chat command can inspect and make narrowly defined changes to Otto's
   existing configuration; and
2. an operator can trace one inbound chat message through `otto-connect`, ACP,
   `otto serve`, the session, and its turn without exposing message content or
   credentials in logs.

This is not a general remote shell, TOML editor, provider-plugin mechanism, or
credential-entry channel. Otto continues to support only `openai-compatible`
and `chatgpt` providers.

## Non-goals

- Do not add a connector-specific administrator allowlist or any other new
  configuration key. The existing platform `chats` and `senders` allowlists
  remain the admission boundary for both normal turns and management commands.
- Do not accept, store, return, or log provider API keys, bot tokens, app
  secrets, OAuth credentials, or environment-variable values.
- Do not let a model interpret a chat message as an administrative action. All
  management commands are parsed, validated, and executed by connect and Otto
  code before a prompt could be queued.
- Do not offer arbitrary TOML patches, arbitrary file writes, arbitrary
  commands, or a way to introduce another provider implementation.
- Do not hot-switch an active turn or claim that a disk configuration change
  automatically updated an already-built runner.

## Deployment boundary

Management commands require the existing attached deployment:

```text
Telegram or Feishu
        |
        v
  otto-connect
    | ACP: sessions, prompts, streaming updates, approvals
    | Unix-socket HTTP: management operations
        v
  otto acp --attach ----> otto serve
```

The standard connector configuration remains unchanged:

```toml
[agent]
command = ["otto", "acp", "--attach"]
workspace = "/Users/me/work"
```

`otto-connect` derives the socket from the existing `--socket` argument in
`command`, or uses Otto's existing default socket resolution. It does not add a
new connector configuration setting.

A connector using direct `otto acp` can continue to chat, use sessions, and
handle approvals. Its management commands return a deterministic error:

> This command requires `otto acp --attach` connected to a running `otto serve`.

The connector must check server reachability before each management action and
report the socket path plus an actionable failure, rather than falling back to
a model prompt or attempting to edit a local config file.

## Authorization and interaction

The existing `chats` and `senders` lists already limit every admitted inbound
message. An admitted sender may use management commands. This is appropriate
for a connector deployed as one owner's controlled chat entry point; deployments
that need different conversational and administrative populations are out of
scope until a separately approved authorization design.

Management commands use a strict command parser. Unknown slash commands retain
current behavior and go to the model; the reserved commands below never do.
Command arguments are text, not shell syntax. Parsed values have length limits,
profile names use the existing profile-name validation, and URLs/models use the
same Otto validation used by local configuration.

Every configuration mutation is two-step:

1. A `/config ...` command validates the proposed change and returns a redacted
   preview plus a short-lived opaque confirmation token.
2. `/config confirm <token>` performs exactly that previewed operation once.

Tokens are bound to the platform, chat, sender, proposed operation, and the
version of the configuration read for the preview. They expire after 10 minutes
and are removed after success, rejection, or expiry. A newer preview for the
same chat replaces the old one. Confirmation from another chat or sender fails.

Destructive operations, including profile removal and changing the default
profile, always require confirmation. Creation and field edits use the same
uniform protocol so a chat typo cannot change a model or endpoint immediately.

## Configuration management surface

### Read commands

| Chat command | Result |
| --- | --- |
| `/config` | Current chat session id and its provider/profile/model/thinking, the configured default profile, and whether the connector is attached to serve. |
| `/config profiles` | Profile summaries: name, provider, model, thinking, and whether it is default. `base_url` is omitted by default. |
| `/config show <profile>` | One profile's non-sensitive fields: provider, model, thinking, `base_url`, context/compaction windows, and the API-key environment-variable **name** if configured. |
| `/models [profile]` | For an `openai-compatible` profile, list IDs returned by its endpoint. For `chatgpt`, explicitly state that the provider cannot enumerate models. |

`/models` returns a bounded, sorted result. If it is too long for the chat
platform, connect sends numbered chunks and names the profile in each chunk.
It never guesses a model ID, price, availability, or an account entitlement.

### Mutation commands

The initial command grammar is deliberately narrow:

```text
/config use <profile>
/config add <profile> --provider <openai-compatible|chatgpt> --model <model>
/config set <profile> model <model>
/config set <profile> thinking <low|medium|high|xhigh|max|unset>
/config set <profile> base-url <https-url>
/config set <profile> api-key-env <environment-variable-name>
/config remove <profile>
/config confirm <token>
/config cancel
```

Rules:

- `/config use <profile>` changes `default_profile` after confirmation. It does
  not alter any open session.
- `/config add` requires `--provider` and `--model`; an
  `openai-compatible` profile also requires a `base_url` before it can be
  confirmed. It may receive `base-url` and `api-key-env` in the creation
  command or through a future deliberately designed draft flow; it never
  receives the secret value.
- `/config set` changes exactly one whitelisted field. A `chatgpt` profile
  rejects `base-url` and `api-key-env`; an `openai-compatible` profile must
  retain a valid base URL and model.
- `/config remove` refuses the last profile and handles a default-profile
  removal only through an explicit preview that names the required replacement
  default. The first implementation may reject removal of the default profile
  rather than silently choosing a replacement.
- `/config cancel` drops this chat's outstanding confirmation without writing.
- Model selection guidance, when connect offers a recommendation, prefers the
  least expensive configured or endpoint-reported model adequate for ordinary
  work. It must not invent price data; it states when no verified cost data is
  available.

The initial scope does not expose generic configuration tables such as MCP,
memory, sandbox, server, or workflow configuration for writes. Existing
read/manage routes for those features can be added independently with their
own operation-specific validation and confirmation semantics.

### Current-session model selection

A default-profile edit affects only sessions created after the server has
loaded the updated configuration. It must never silently change an existing
session.

A follow-up operation will expose explicit current-session selection:

```text
/model
/model <profile>
```

`/model` reports the session's effective provider/profile/model/thinking.
`/model <profile>` requires an idle session, validates the target profile, and
rebuilds the session runner through Otto's existing application lifecycle.
It is not part of the first configuration-write endpoint unless that lifecycle
contract is implemented and tested at the same time.

### Apply and restart semantics

A confirmed edit writes configuration durably, but it is not automatically a
runtime reload. The response states one of these exact outcomes:

- **Saved; restart required:** the running `otto serve` keeps its startup
  profile catalog. Restart serve before using a newly added/edited profile.
- **Saved and applied:** only if a future server reload contract successfully
  rebuilt the applicable state and returned the effective profile list.
- **Not saved:** validation, stale-version, or writer failure; the response
  names how to recover.

The first implementation returns **Saved; restart required**. A separate
approved design may add an explicit server config-reload operation. Connect
never tries to kill or restart `otto serve` itself.

## Otto management API

The authoritative implementation belongs in `crates/otto`, next to existing
configuration resolution and its backed-up writer. Connect never reads or
writes `config.toml` directly.

All routes are private Unix-socket `/v1/` routes and use the existing bearer
protection when enabled. Request and response schemas are added to
`testdata/server/openapi.yaml` alongside handlers.

### Read routes

```text
GET /v1/config/profiles
GET /v1/config/profiles/{name}
GET /v1/config/models?profile={name}
```

Profile responses expose only:

```json
{
  "name": "work",
  "default": true,
  "provider": "openai-compatible",
  "model": "example-model",
  "thinking": "medium",
  "base_url": "https://api.example/v1",
  "api_key_env": "WORK_API_KEY",
  "context_window": 128000,
  "compaction_window": 16000
}
```

Absent optional fields are omitted or explicitly represented according to the
existing server DTO convention. Responses never include an environment value,
credential, authorization header, full request, or raw configuration text.

`GET /v1/config/models` resolves the named profile from the configuration
snapshot, validates it, and calls the existing OpenAI-compatible models client
only for that provider. It returns sorted unique IDs with a bounded response.
It does not retry provider failures and returns a typed unsupported result for
ChatGPT.

### Preview and commit routes

```text
POST /v1/config/changes
POST /v1/config/changes/{id}/confirm
DELETE /v1/config/changes/{id}
```

`POST /v1/config/changes` accepts a typed operation—not a TOML fragment—and
returns a redacted preview and opaque ID. The initial operation variants are:

```json
{"kind":"set_default_profile","profile":"work"}
{"kind":"create_profile","profile":"work","provider":"openai-compatible","model":"example-model","base_url":"https://api.example/v1","api_key_env":"WORK_API_KEY"}
{"kind":"set_profile_field","profile":"work","field":"model","value":"example-model"}
{"kind":"remove_profile","profile":"old"}
```

The server validates its input before it creates a preview. The preview carries
an opaque id, expiration, and a human-readable ordered field diff; it contains
no secrets. It also retains the exact original config bytes/version required by
the config writer's compare-and-swap.

`POST .../{id}/confirm` atomically performs exactly the previewed edit through
the existing configuration edit helpers and `config::write_bytes` path:

- an advisory lock serializes Otto writers;
- the original bytes must still match (stale requests fail rather than
  overwriting another editor's changes);
- replacement uses an atomic `0600` temp-file rename without following a
  symlink; and
- the old configuration is backed up under the existing `backups/` policy.

The server stores previews only in process memory. Its opaque preview ID is a
high-entropy, one-time, 10-minute capability token: confirmation requires the
exact token, succeeds once at most, and still fails if the captured config
bytes are stale. With bearer authentication, a server may additionally bind a
preview to that caller. A private Unix socket has no request-level identity, so
it relies on its existing same-user filesystem boundary; it must not pretend
that an untrusted `chat_id`, `sender_id`, or request header authenticates a
caller. Connect still binds the token to the originating platform/chat/sender
in memory and refuses cross-chat confirmation. The final API contract may add a
non-secret `actor` field supplied by connect for audit correlation, but it is
not an authorization mechanism.

A successful commit returns `{ "status": "saved_restart_required", ... }`.
On expiration, stale bytes, invalid configuration, or writer failure it returns
a typed error and leaves the file unchanged.

## Connect implementation

Add an internal Unix-socket management client separate from
`connect/internal/agent`, which remains an ACP client. It discovers only the
socket that the existing attach command names or Otto's existing default.

The bridge reserves `/config`, `/models`, and later `/model` before queueing a
prompt. It translates only the strict command grammar above to API requests.
It formats server errors into concise chat-safe messages and retains server
error codes in logs. It does not echo raw JSON or arbitrary error bodies to a
chat.

Confirmation state is stored in memory in the bridge, not in
`~/.otto/connect/state.json`. A connector restart invalidates previews; the
user simply submits the command again. This avoids persisting pending
management operations and preserves the current state-file contract.

Existing `/new`, `/stop`, `/allow`, `/deny`, `/sessions`, `/use`, and memory
commands retain their current behavior.

## Diagnostics and observability

### Principles

Both processes must produce line-oriented structured logs suitable for `grep`,
`log stream`, launchd/systemd collection, and incident correlation. Otto's
existing `key=value` server logger and Go's `slog` text handler are retained
initially; their event names and common fields become a compatibility contract.
No new runtime logging configuration is introduced in this change.

Never log:

- message text, assistant text, reasoning, prompts, tool arguments/results,
  or attachment content;
- API keys, bot tokens, app secrets, OAuth credentials, authorization headers,
  cookies, or environment-variable values;
- raw configuration bytes or raw HTTP/ACP frames.

A profile's `api_key_env` name and a validated base URL may appear in a
configuration audit event because neither is a secret by itself. Error strings
must pass existing Otto redaction before logging; connect must redact known
platform secrets and authorization-like substrings before logging external
errors.

### Correlation fields

On receiving an admitted chat message, connect creates a random opaque
`request_id` (at least 128 bits). It carries that ID through all connector
logs for the message and sends it to the management API in a non-authoritative
correlation header, for example `X-Otto-Request-Id`. Otto validates a bounded
ASCII format and echoes it in server logs; it never trusts it for identity or
access control.

The standard fields are:

| Field | Meaning |
| --- | --- |
| `component` | `connect`, `connect.agent`, `connect.bridge`, `otto.server`, `otto.acp`, or the owning Otto module |
| `event` | Stable machine-readable event name |
| `request_id` | Opaque connector-generated ID; absent for non-request lifecycle events |
| `platform` | `telegram` or `feishu` |
| `chat_id` | Platform chat identifier |
| `sender_id` | Platform sender identifier, only for admitted/rejected message and management audit events |
| `message_id` | Platform message identifier when available |
| `session_id` | Otto session identifier once known |
| `turn_id` | Otto turn identifier when available in attach/server operations |
| `workspace` | Canonical workspace path where relevant |
| `profile`, `provider`, `model` | Effective or target non-secret configuration values where relevant |
| `command` | Reserved connector command name, never its unbounded/raw arguments |
| `operation`, `change_id` | Typed configuration operation and opaque preview identifier |
| `outcome` | `started`, `accepted`, `succeeded`, `rejected`, `cancelled`, `failed`, or `timeout` |
| `duration_ms` | Completed operation duration |
| `error_code` | Stable local/API error category; raw error detail is redacted and separate |

Chat and sender IDs are operational identifiers already recorded by connect for
admission failures. Operators must protect logs accordingly. A later privacy
change may replace them with keyed hashes, but this design preserves current
troubleshooting usefulness and does not silently reduce observability.

### Required events

`otto-connect` logs at `INFO`:

- `connector_started` / `connector_stopping` with platform set, workspace,
  attach mode, and resolved socket path if attached;
- `chat_received`, `chat_rejected`, `chat_queued`, `chat_queue_full`;
- `session_created`, `session_loaded`, `session_load_failed`,
  `session_bound`, `session_unbound`;
- `turn_started`, `turn_finished`, `turn_cancel_requested` with stop reason,
  duration, and output byte/rune count only;
- `management_requested`, `management_previewed`, `management_confirmed`,
  `management_cancelled`, and `management_failed`;
- `chat_reply_sent` with response size and delivery outcome;
- `agent_started`, `agent_exited`, and `agent_restart_scheduled`.

Warnings/errors retain those fields plus a typed failure reason for ACP
transport failures, server-unreachable conditions, parse/validation failures,
state-write failures, platform send failures, and confirmation expiration.

`otto serve` logs at `INFO`:

- `management_request_started` and `management_request_finished` with route,
  method, request ID, status/outcome, duration, and a redacted error code;
- `config_change_previewed`, `config_change_confirmed`, `config_change_failed`,
  and `config_change_expired` with operation, change ID, profile(s), actor
  metadata when supplied, and restart-required result;
- model-list request start/finish with profile/provider, result count, and
  duration, never model provider response bodies;
- existing session/turn lifecycle events augmented with request ID when a
  request originated from connect and that ID is available.

`otto acp --attach` logs ACP-to-server forwarding lifecycle and includes the
request ID when ACP eventually carries it. The first management implementation
uses the direct connect-to-server client, so ACP correlation is best-effort for
normal prompts; connect still correlates ACP process/session and prompt events
on its side.

### Operator workflow

A failure response to chat includes a concise recovery action and its
`request_id`, for example:

> Configuration was not saved (`request_id=...`): Otto rejected the profile's
> base URL. Send `/config show work`, correct the URL, then submit the change
> again.

An operator can search both process logs by that ID. The runbook documents:

```text
# launchd/systemd or the process supervisor's logs
... | grep 'request_id=<id>'
```

At normal verbosity, logs show lifecycle and outcomes but not high-frequency
chunk/update events. Tool-call and ACP frame-level diagnostic events are only
emitted at debug level in a future explicit logging-mode design; adding a new
configuration key is not part of this work.

## Failure behavior

- A non-attach connector never sends management text to the model; it returns
  the attach requirement.
- An unreachable serve returns the socket path and request ID; it does not
  attempt a local config edit.
- A malformed command returns command-specific usage without creating a
  preview.
- A profile/model/base URL validation failure returns a safe summary and logs a
  stable error code.
- A stale or expired preview cannot write; connect asks the user to submit the
  request again.
- A confirmation response lost after a server write is resolved by querying the
  relevant profile/default state before telling the chat to retry, avoiding a
  duplicate mutation.
- A chat reply delivery failure is logged with the request ID; it does not
  roll back a successful configuration write.

## Tests and acceptance

Rust server tests cover:

- profile summaries and model-list redaction/no-secret guarantees;
- provider-specific model listing, including the explicit ChatGPT unsupported
  result;
- every typed preview validation case;
- confirmation, expiry, cancellation, one-time use, and stale-byte refusal;
- preservation of unrelated TOML bytes, existing atomic-write/backup behavior,
  and no config change on error;
- OpenAPI schema parity; and
- log event fields/redaction for all management outcomes.

Go connector tests cover:

- attach command/socket discovery and direct-mode rejection;
- exact parsing of reserved commands and non-forwarding to ACP;
- preview/confirm/cancel state binding to chat and sender, expiration, and
  connector restart behavior;
- management client error rendering without raw bodies or secrets;
- structured lifecycle events and shared request ID on connector calls; and
- end-to-end attach tests against a loopback Otto server for read, preview,
  confirm, stale-preview, and server-unreachable paths.

The default suite remains offline: fake provider/model-list and loopback server
fixtures replace external provider, Telegram, and Feishu calls. Run focused
Rust and Go tests first, then `make check` and `make check-linux` for the
implementation change.

## Rollout

1. Implement Otto's read, preview, and confirmation APIs with tests and
   OpenAPI updates.
2. Implement connect's Unix-socket management client, strict commands,
   confirmation state, and correlation logs.
3. Update the user manual and connector documentation with attach deployment,
   command syntax, restart-required behavior, and log troubleshooting.
4. Add explicit session model switching only after its application lifecycle
   contract is designed and tested.

No code or tests are authorized by this document until it is approved.
