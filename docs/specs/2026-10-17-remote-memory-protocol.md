# Remote memory service protocol

Status: approved 2026-10-17.

## Problem

Otto stores memory on the local host (`~/.otto/memory/`, see the
[file memory store](2026-10-17-file-memory-store.md)). A memory service on a
different host has no way in. An embedder can inject a `MemoryExtension`
in Rust (`crates/otto/src/runtime/wiring.rs:75`), but the `otto` binary has no
configuration that reaches a network service, and no part of Otto sends
conversation content to a memory service. Writes reach the local store from
the model's `remember` and `forget` tool calls, from `/remember`, and from
reflection as candidates that wait for human review.

This document defines version 1 of an HTTP protocol between Otto and an
external memory service, and the Otto side of it. The service is called the
memory service below. Its implementation is out of scope.

## Decisions

- 2026-10-17 (user): the memory service runs on a different host and is
  reached over the network.
- 2026-10-17 (user): Otto keeps working when the memory service fails.
- 2026-10-17 (user): Otto never uses an incomplete recall result.
- 2026-10-17 (user): the memory service receives the user's input, the
  assistant's replies and tool results, in addition to explicit writes.
- 2026-10-17 (user): a turn that cannot be delivered (service failure, or
  over the size limit) is discarded. Otto keeps no spool and does not
  replay.
- 2026-10-17 (user): the memory service is the source of truth. Otto keeps
  no local copy of remote memory.
- 2026-10-17 (user): the memory service accepts writes directly and decides
  whether to adopt them. Otto does not review them.
- 2026-10-17 (user): Otto defines generic external memory service support,
  not a client for one product. The protocol has explicit versioning and
  compatibility rules.
- 2026-10-17 (user): scopes (user, workspace) are part of the protocol.
- 2026-10-17 (user): the memory service can place memory in Otto's context
  without a matching query and can withdraw it.
- 2026-10-17 (user): memory placed by the service is rendered as untrusted
  reference material, not as instructions.
- 2026-10-17 (user): HTTP with JSON bodies. No server-sent events in
  version 1.
- 2026-10-17 (user): this work covers Otto and the protocol only.

Approved 2026-10-17 (user), proposed in this document:

1. The backend is selected with `[memory] backend = "remote"` and configured in
   `[memory.remote]`.
2. The URL must use `https`, except that `http` is accepted when the host is a
   loopback address (`127.0.0.0/8`, `::1`, `localhost`).
3. Service-placed memory ("pinned items") is returned inside every recall
   response. There is no separate push channel. A withdrawn item stops
   appearing at the next recall, which runs at the next user input.
4. Turn upload is a separate endpoint (`/v1/turns`), not the existing
   `observe` port operation. See [Turn upload](#turn-upload) for the size
   reason.
5. A turn that contains a redaction marker or a credential shape is not
   uploaded.
6. The user scope id comes from configuration (`[memory.remote].user`), so
   several Otto hosts share one user scope.
7. The native memory management commands stay unsupported for the remote
   backend, except `/remember`, which is routed to the service.
8. With `backend = "remote"`, an unset `recall_tokens` defaults to 8192
   instead of 2000. At 2000 bytes, 12 workspace items leave 165 bytes of
   text in total (see [Limitations](#limitations)).

## Overview

```text
Otto host                                     memory service host
---------                                     -------------------
user input ── recall ──── POST /v1/recall ──► query match + pinned items
              (blocking, 3 s timeout)   ◄──── items, coverage
model tool  ── memory_search ─ /v1/recall or /v1/get
            ── remember ─────── /v1/remember ─► adopt or skip
            ── forget ───────── /v1/forget ───► suppress
/remember   ─────────────────── /v1/remember (origin "human")
turn end    ── upload worker ── POST /v1/turns ► extraction (service side)
              (background, no retry)
```

All five endpoints are `POST` with JSON bodies. Otto makes no network call at
startup, so an unreachable service does not delay or fail startup.

## Configuration

```toml
[memory]
backend = "remote"
recall_tokens = 8192     # existing key; byte bound of rendered recall; remote default 8192
max_results = 12         # existing key; item limit per recall

[memory.remote]
url = "https://mem.example.internal:8443"
user = "alice"
token_env = "OTTO_MEM_TOKEN"   # default
recall_timeout = "3s"          # default; Go duration syntax

[memory.workspace_ids]          # existing key
"/Users/alice/src/otto" = "otto"
```

- `url` is required, has no path, query or fragment, and follows decision 2.
  Otto appends `/v1/<endpoint>`.
- `user` is required. It is 1 to 128 printable ASCII bytes (the existing scope
  id rule, `valid_opaque_id` in `crates/otto-core/src/memory/validate.rs:28`).
  It becomes the user scope `{"namespace":"user","id":<user>}`.
- `token_env` names the environment variable that holds the bearer token. The
  token is never read from configuration. An empty or unset variable:
  startup fails when `[memory].required = true`; otherwise memory is
  disabled for the process and Otto prints one warning.
- `recall_timeout` is greater than zero and at most `10s`. Write calls use a
  fixed 30 s timeout.
- `[memory].require_encryption = true` with `backend = "remote"` fails
  startup: Otto cannot verify encryption at rest on another host.
- `[memory.remote]` is read only when `backend = "remote"`.
- An embedder that injects a `MemoryExtension` and also configures
  `backend = "remote"` gets a startup error.
- The workspace scope is the existing one: `new_workspace_scope`
  (`crates/otto/src/memory/scope.rs:23`) on the canonical workspace path, or
  the `[memory.workspace_ids]` override. Two hosts share a workspace scope
  only through the override, because the default id is a hash of the local
  path.

## Otto-side wiring

The remote backend is a `MemoryService` implementation
(`crates/otto/src/memory/remote/`) wrapped in a `MemoryExtension` with
`with_learning`:

```rust
LearningGrant {
    scopes: vec![user_scope, workspace_scope],
    input_classes: vec!["explicit_memory".into()],
    background: false,
}
```

This reuses the existing paths without change:

| Path | Existing code | Remote call |
| --- | --- | --- |
| Per-turn recall | `ServiceBinding::recall`, `wiring.rs:330` | `/v1/recall` |
| `memory_search` with a query | `tool/memory_service.rs` | `/v1/recall` (limit 20, budget 4000) |
| `memory_search` with an `id` | `InspectQuery::Get` | `/v1/get` |
| `remember` | `Client::remember` | `/v1/remember` |
| `forget` | `ForgetTarget::Item`, `ForgetLevel::Suppress` | `/v1/forget` |

`describe()` returns a description built locally, not fetched:
identity `remote`, operations recall, inspect (`Get` only), remember and
forget, each with `supports_delegated_learning = true` and effects
`[ChangeMemory]` for the two writes; forget declares target `Item` and level
`Suppress`; `encryption_at_rest = None`.

New code outside the adapter:

- The turn recorder and upload worker ([Turn upload](#turn-upload)).
- `/remember` routing in `app/management.rs`: with any memory service
  extension, including the remote backend, `Remember` goes through
  `Client::remember` with origin `human`; for the remote backend that is
  `/v1/remember`. The scope is the workspace scope, or the user scope for
  `--scope user`; `--kind` and `--key` go into `metadata`, as the model's
  `remember` tool does. The other native commands (`MemoryList`,
  `MemoryShow`, `MemorySearch`, `MemoryForget`, `MemoryReviewList`,
  `MemoryReview`) keep returning an unsupported error, because the service
  owns listing and review. `otto memory` subcommands fail with the remote
  backend instead of reading the unused local files store.
- The advice model (Jev memory review) is not started for the remote backend.
  Requested reflection already writes no memory candidates when an extension
  is set (`Controller::memory_manager` returns `None`,
  `app/management.rs:277`). Task 4 checks that automatic reflection does the
  same.

The model-callable tools keep their names, inputs and descriptions, so their
[capability descriptions](../development/contracts.md#capability-descriptions)
do not change.

## Wire protocol, version 1

### Transport and headers

- Base: `<url>/v1/`. Method `POST`. Body `application/json; charset=utf-8`.
- Request headers:
  - `Authorization: Bearer <token>`.
  - `Otto-Memory-Protocol: 1.0`.
  - `User-Agent: otto/<version>`.
- Response headers: `Otto-Memory-Protocol: 1.<minor>`. A response without
  this header, or with a different major, is an invalid response.
- Every response body is at most 1 MiB. Otto stops reading at 1 MiB and
  treats the response as invalid.
- Otto sends one request per operation. It does not retry any request.

### Common objects

```json
"scope":   {"namespace": "user" | "workspace", "id": "<1..128 printable ASCII>"}
"context": {"scopes": [<scope>, ...], "session_id": "<id>" | null}
```

`context.scopes` lists the scopes the request may read or write. In version 1
it is always `[user scope, workspace scope]`. `session_id` is the Otto
session id. It is null for `/remember` sent from a frontend with no session.

An item id is 1 to 256 printable ASCII bytes, chosen by the service and
unique within its scope. A `revision` is an optional opaque string of at most
256 bytes.

### `POST /v1/recall`

Request:

```json
{
  "context": {...},
  "query": "<redacted user text, 1..8192 bytes>",
  "limit": 12,
  "max_bytes": 8192
}
```

Response `200`:

```json
{
  "coverage": "complete",
  "items": [
    {"id": "m-17", "scope": {...}, "revision": "4", "text": "...", "pinned": true},
    {"id": "m-03", "scope": {...}, "text": "..."}
  ]
}
```

- `coverage` is `complete`, `partial` or `unknown`. Otto uses the result only
  when it is `complete`. For `partial` or `unknown`, Otto discards every item.
  The turn continues without memory and Otto emits `MemoryWarning`.
- Pinned items are items the service places in context regardless of the
  query. The service orders them before query matches. `pinned` defaults to
  `false`. Otto renders pinned and matched items the same way, under the
  existing untrusted-reference preamble (`MEMORY_CONTEXT_PREAMBLE`,
  `crates/otto-core/src/agent/memory.rs:23`). The model is told they are
  context, not instructions.
- `limit` counts pinned items. At most `limit` items.
- `max_bytes` bounds Otto's rendering of the returned items, defined in
  [Rendering](#rendering). Otto renders the items before use. If the rendering
  exceeds `max_bytes`, Otto discards every item and emits `MemoryWarning`.
- Each item: `scope` is one of `context.scopes`; `text` is 1 to 8192 bytes
  of UTF-8; `id` and `revision` follow [Common objects](#common-objects).
- Any violation of the rules above discards the whole result. Otto does not
  use a valid subset.

### Rendering

Otto renders recalled items with `render_memory_context`
(`crates/otto-core/src/agent/memory.rs:114`). Its output length in bytes is
what `max_bytes` bounds:

```text
<preamble>                       131 bytes, MEMORY_CONTEXT_PREAMBLE
for each item:
<memory id=J(id) scope=J(ns "/" scope_id) kind="reference" key="">E(text)</memory>\n
```

`J(s)` is the JSON string encoding of `s`, quotes included. `E(s)` replaces
`&` with `&amp;`, `'` with `&#39;`, `<` with `&lt;`, `>` with `&gt;` and `"`
with `&#34;`. No items render as the empty string. This rendering is part of
protocol 1.x and changes only with a new major version. The golden fixture
`fixtures/v1/recall_rendered.txt` is the rendering of
`fixtures/v1/recall_response.json` (384 bytes).

### `POST /v1/get`

Request `{"context": {...}, "scope": {...}, "id": "m-17"}`. Response `200`
`{"item": {<item>}}`, or `404` when the id does not exist or is withdrawn.
Otto returns no item for `404`.

### `POST /v1/remember`

Request:

```json
{
  "context": {...},
  "request_id": "<32 hex>",
  "origin": "model" | "human",
  "scope": {...},
  "text": "<1..8192 bytes>",
  "target": {"id": "m-17", "revision": "4"} | null,
  "metadata": {"kind": "...", "key": "...", "reason": "...", "labels": "...", "confidence": "..."}
}
```

`target` asks the service to replace that item; `revision` makes the
replacement conditional. `metadata` is a string-to-string map of at most 32
entries and 4 KiB, from the `remember` arguments. The service decides
whether to adopt the text.

### `POST /v1/forget`

Request `{"context": {...}, "request_id": "<32 hex>", "origin": "model" |
"human", "scope": {...}, "id": "m-17", "revision": "4" | null}`. Forget means
the item stops appearing in recall. Whether the service deletes it is the
service's decision.

### Receipts

`/v1/remember` and `/v1/forget` return `200` with:

```json
{"state": "applied" | "no_change" | "skipped" | "pending" | "failed",
 "id": "m-17", "revision": "5"}
```

| `state` | Meaning | Port `Receipt` (admission, completion, visibility, item) |
| --- | --- | --- |
| `applied` | stored or suppressed | acknowledged, complete, ready, applied |
| `no_change` | already in that state | acknowledged, complete, ready, no_change |
| `skipped` | the service declined to adopt | acknowledged, complete (remember) or partial (forget), not_applicable, skipped |
| `pending` | accepted, decided later | acknowledged, pending, pending, unknown |
| `failed` | not stored | rejected, failed, not_applicable, failed |

A skipped forget maps to `partial` because `Receipt::validate_forget`
accepts `complete` only when the achieved coverage includes `suppress`.

`id` is required for `applied` and `no_change`. A timeout or a transport
error after the request was sent maps to `OutcomeUnknown`: the write may or
may not have happened. The existing tool result already reports this
(`receipt_result` in `tool/memory_service.rs`).

### `POST /v1/turns`

Request:

```json
{
  "context": {...},
  "turn_id": "<32 hex>",
  "status": "success" | "cancelled" | "error",
  "messages": [
    {"id": "...", "role": "user", "created_at": "2026-10-17T08:00:00Z",
     "blocks": [{"type": "text", "text": "..."}]},
    {"id": "...", "role": "assistant", "created_at": "...",
     "blocks": [{"type": "text", "text": "..."},
                {"type": "tool_call", "id": "call_1", "name": "bash",
                 "arguments": {"command": "ls"}}]},
    {"id": "...", "role": "tool", "created_at": "...",
     "blocks": [{"type": "tool_result", "id": "call_1", "name": "bash",
                 "text": "...", "is_error": false}]}
  ]
}
```

Response `202` with an empty object. The service extracts memory from the
turn on its own schedule; the response carries no result.

`role` is `user`, `assistant`, `tool` or `context`. `created_at` is RFC 3339
UTC. The body is at most 4 MiB.

### Errors

A non-2xx response other than `404` from `/v1/get` has the body
`{"error": {"code": "<code>", "message": "<text>"}}`.

| HTTP | `code` | Port `ErrorKind` |
| --- | --- | --- |
| 400 | `invalid_input` | `InvalidInput` |
| 401 | `unauthorized` | `Unauthorized` |
| 403 | `forbidden` | `Unauthorized` |
| 409 | `conflict` | `Conflict` |
| 413 | `too_large` | `InvalidInput` |
| 429 | `rate_limited` | `RateLimited` |
| 503 | `unavailable` | `Unavailable` |
| 400 | `unsupported_version` | `Unsupported` |

Any other status, an unparseable body or an unknown code maps to
`Unavailable`. `message` is untrusted: Otto never shows it to the model or
the user. The Otto log records one `warn` line per failed request with the
endpoint, the HTTP status, the code and `message` truncated to 512 bytes.
`MemoryWarning` and tool errors show only the fixed text of the port
`ErrorKind` (for example `memory service unavailable`), because
`Client` replaces every error detail with that text
(`safe` in `crates/otto/src/memory/client.rs`). A recall discarded for
`coverage` or a rule violation is reported as `Unavailable`.

## Turn upload

### Capture

A turn is one prompt step: `run_prompt_step`, `run_followup_step` or the
image prompt in `crates/otto/src/app/mod.rs`. Each already ends in
`App::after_prompt` (`app/mod.rs:1548`).

- `SharedSession` (`runtime/builder.rs`) gains an optional recorder. While
  recording is on, `SharedSession::append` stores a clone of each appended
  message after the inner append succeeds. Reading `messages()` at turn end
  is not used, because compaction during the turn replaces that list.
- Recording starts before the runner call and stops in `after_prompt`, which
  passes the captured messages and the status (`success`, `cancelled` or
  `error`) to the upload worker.
- Request-local memory context (`state.memory_context`) is never appended to
  the session, so recalled items are not sent back to the service.
- Subagent sessions are not recorded. Their final result reaches the parent
  turn as a tool result.

### Conversion

- Kept: `Text` blocks; `ToolCall` (`tool_call_id`, `tool_name`, `arguments`
  as a JSON object; absent, unparseable or non-object arguments are sent as
  `{}`); `ToolResult` (`tool_call_id`, `tool_name`, `text`,
  `is_error`).
- Omitted: `Image`, `Reasoning` and `Other` blocks, and messages with role
  `Other`. A message with no kept block is omitted. A turn with no kept
  message is not sent.

### Filters

A turn is not sent, and the log records one line with the reason and the
byte size, when:

1. the encoded request is over 4 MiB; or
2. the `DefaultGuard` rules (`crates/otto/src/memory/guard.rs:43`) find a
   redaction marker, URI userinfo, a private key delimiter, a credential
   header or a credential assignment in any string of the request. The rules
   run on each string directly; `DefaultGuard::check` itself rejects input
   over 64 KiB or 512 fields, and 10.53% of measured turns are over 256 KiB.

Session text is already redacted, so filter 2 rejects every turn in which the
redactor replaced a value. In the local sessions measured on 2026-10-17,
39 of 1557 turns (2.50%) contain `[REDACTED]`. No turn is over 4 MiB (the
largest is 2,618,788 bytes).

### Delivery

- One upload worker per process. It sends one request at a time, in the order
  turns ended, so turns of one session arrive in order.
- The queue holds 8 turns. A turn that arrives when the queue is full is
  discarded and logged.
- A failed request (timeout, transport error or non-2xx) is logged and the
  turn is discarded. There is no retry, no spool and no replay.
- At shutdown Otto waits at most 30 s for the queue to drain, then discards
  the rest.

### Why not the `observe` operation

The port's `Observation` limits content to 8 KiB per observation, 64
observations and 256 KiB per request
(`crates/otto-core/src/memory/validate.rs:5-14`), and has no fields for tool
name, call id, arguments or error flag. In the 1557 measured turns the median
size is 9,121 bytes, 10.53% are over 256 KiB and the 99th percentile is
749,589 bytes.

## Versioning

- The major version is in the path (`/v1/`). The minor version is in the
  `Otto-Memory-Protocol` header of every request and response.
- A minor version only adds: optional request fields, response fields,
  endpoints, enum values and error codes.
- Both sides ignore unknown fields in JSON objects.
- Otto sends a new optional field, calls a new endpoint or sends a new enum
  value only when the minor version in the last response header from the
  service declares it. Before the first response, Otto assumes `1.0`.
- An unknown enum value in a response is an invalid response. A service must
  not send a value that the request's minor version does not declare.
- Removing a field, changing a field's type or meaning, changing the
  [rendering](#rendering) or making an optional field required needs a new
  major version and a new path (`/v2/`).
- One Otto release speaks one major version. A service that must support
  Otto releases on both sides of a major change serves both paths during the
  migration.
- The request and response examples in this document are stored as golden
  files in `crates/otto/src/memory/remote/fixtures/v1/`. Tests decode and
  re-encode them. A change that breaks a fixture is a protocol change.

## Compatibility

- `files` remains the default backend. Changing the backend migrates no data
  in either direction.
- An Otto release without this feature rejects `backend = "remote"` with
  `unsupported memory backend "remote": must be files`.
- Session files do not change.
- `docs/specs/2026-10-07-open-memory-contract.md` states that no network
  protocol is implemented. This document replaces that statement for the
  remote backend.
- Tool names, inputs and capability descriptions do not change.

## Failure behaviour

| Failure | Recall | Writes from tools and `/remember` | Turn upload |
| --- | --- | --- | --- |
| Service unreachable or timeout | no memory this turn, `MemoryWarning` | tool error, outcome unknown | discarded, logged |
| `401` / `403` | no memory this turn, `MemoryWarning` (`memory scope is not authorized`) | tool error | discarded, logged |
| `coverage` not `complete` | no memory this turn, `MemoryWarning` | n/a | n/a |
| Invalid response | no memory this turn, `MemoryWarning` | tool error | discarded, logged |
| Token variable unset | memory disabled for the process, or startup fails with `required = true` | | |

The turn always continues. A recall takes at most `recall_timeout` (3 s by
default) of the turn's latency.

## Limitations

- Recall runs only for user input with text. An image-only input gets no
  recalled or pinned memory.
- A withdrawn pinned item stays in the current turn's context until the next
  user input.
- Undelivered turns are lost (decided above).
- About 2.5% of turns are not uploaded because they contain a redaction
  marker (measured above).
- One memory service per configuration.
- `recall_tokens` is a byte bound of the rendering, at most 8192. The
  preamble takes 131 bytes and each item 71 to 142 bytes of markup (142 for a
  workspace item with the default `sha256:` scope id and a 4-byte item id).
  With 8192 bytes and 12 workspace items, 6,357 bytes remain for text.
- Workspace scopes match across hosts only through `[memory.workspace_ids]`.

## Task breakdown

Dependencies: 1 → 2 → 3 → 4 → (5, 6) → 7 → 8.

| # | Task | Files | Owner | Tests |
| --- | --- | --- | --- | --- |
| 1 | `backend = "remote"`, `[memory.remote]`, validation | `crates/otto-core/src/config/memory.rs`, config docs keys | parent | URL scheme and loopback rule; `user` rules; timeout bounds; `require_encryption` rejected; remote `recall_tokens` default 8192; files defaults unchanged |
| 2 | Wire types and golden fixtures | `crates/otto/src/memory/remote/wire.rs`, `fixtures/v1/` | worker A | fixtures decode and re-encode; unknown fields ignored; unknown enum rejected |
| 3 | HTTP client implementing `MemoryService`: describe, recall with coverage and rendering checks, get, remember, forget, receipts, errors | `crates/otto/src/memory/remote/mod.rs` | worker A | against an axum stub on loopback: `complete` used; `partial`, `unknown`, missing header, over `max_bytes`, out-of-scope item and timeout each yield no items; receipt table; error table; `message` not in the error text |
| 4 | Wiring: build the extension from config, token from env, `required` handling, no advice model, check automatic reflection | `crates/otto/src/runtime/wiring.rs`, `builder.rs` | parent | unset token disables memory; unset token with `required` fails; injected extension plus remote fails |
| 5 | Turn recorder and upload worker | `runtime/builder.rs` (`SharedSession`), `app/mod.rs` (`after_prompt` and the three call sites), `crates/otto/src/memory/remote/upload.rs` | worker B | one request per turn; no retry after `503`; a turn containing `[REDACTED]` is not sent; a turn over 4 MiB is not sent; image and reasoning blocks omitted; full queue discards; shutdown wait bounded |
| 6 | `/remember` routing | `app/management.rs` | parent | `/remember` sends origin `human` and the workspace scope; other native commands still unsupported |
| 7 | Documentation | `docs/manual/memory-and-extensions.md`, a protocol reference page linked from `docs/user-manual.md`, `CHANGELOG.md` | parent | none (documentation) |
| 8 | `cargo fmt --all`, focused tests, PR | | parent | list each command and result in the PR |

Workers A and B own only the files listed for their tasks. The parent owns
the interfaces between tasks (the `MemoryService` impl signature for task 3,
the recorder hand-off type for task 5), integration and acceptance. All
tests stay offline: the stub binds `127.0.0.1:0`.
