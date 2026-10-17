# Remote memory protocol

Protocol version 1 between Otto and an external memory service. This page is
for people who implement a compatible service. For configuration and Otto's
behavior see [Remote memory service](memory-and-extensions.md#remote-memory-service);
for the design rationale see the
[specification](../specs/2026-10-17-remote-memory-protocol.md).

## Transport and headers

- Base: `<url>/v1/`. All five endpoints use `POST` with a body of
  `application/json; charset=utf-8`.
- Request headers:
  - `Authorization: Bearer <token>`
  - `Otto-Memory-Protocol: 1.0`
  - `User-Agent: otto/<version>`
- Response header: `Otto-Memory-Protocol: 1.<minor>`. A response without this
  header, or with a different major version, is an invalid response.
- A response body is at most 1 MiB. Otto stops reading at 1 MiB and treats the
  response as invalid.
- Otto sends one request per operation and does not retry.
- Timeouts: `[memory.remote].recall_timeout` (default 3 s, at most 10 s) for
  `/v1/recall` and `/v1/get`; 30 s for `/v1/remember`, `/v1/forget` and
  `/v1/turns`.

## Common objects

```json
"scope":   {"namespace": "user" | "workspace", "id": "<1..128 printable ASCII>"}
"context": {"scopes": [<scope>, ...], "session_id": "<id>" | null}
```

- `context.scopes` lists the scopes the request may read or write. In version 1
  it is `[user scope, workspace scope]`. `session_id` is the Otto session id; it
  is null for `/remember` sent from a frontend with no session.
- The user scope id is `[memory.remote].user`.
- An item id is 1 to 256 printable ASCII bytes, chosen by the service and
  unique within its scope. A `revision` is an optional opaque string of at most
  256 bytes.
- An item is `{"id", "scope", "revision"?, "text", "pinned"?}`. `text` is 1 to
  8192 bytes of UTF-8. `pinned` defaults to `false`.

## Endpoints

### `POST /v1/recall`

Request:

```json
{
  "context": {"scopes": [...], "session_id": "..."},
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
    {"id": "m-17", "scope": {"namespace": "user", "id": "alice"}, "revision": "4", "text": "...", "pinned": true},
    {"id": "m-03", "scope": {"namespace": "workspace", "id": "sha256:..."}, "text": "..."}
  ]
}
```

- `coverage` is `complete`, `partial` or `unknown`. Otto uses the result only
  when it is `complete`; for the other values it discards every item.
- Pinned items are items the service places in context regardless of the query.
  Order them before query matches. Withdrawing a pinned item means omitting it
  from later responses.
- `limit` counts pinned items; return at most `limit` items.
- Each item's `scope` must be one of `context.scopes`.
- Any violation of these rules discards the whole result. Otto does not use a
  valid subset.
- `max_bytes` bounds Otto's rendering of the returned items (see
  [Rendering](#rendering)). If the rendering exceeds `max_bytes`, Otto discards
  every item.

### `POST /v1/get`

Request:

```json
{"context": {...}, "scope": {"namespace": "user", "id": "alice"}, "id": "m-17"}
```

Response `200`: `{"item": {<item>}}`. Response `404` when the id does not
exist or is withdrawn; Otto returns no item.

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

- `target` asks the service to replace that item; `revision` makes the
  replacement conditional.
- `metadata` is a string-to-string map of at most 32 entries and 4 KiB.
- The service decides whether to adopt the text. The response is a
  [receipt](#receipts).

### `POST /v1/forget`

Request:

```json
{"context": {...}, "request_id": "<32 hex>", "origin": "model" | "human",
 "scope": {...}, "id": "m-17", "revision": "4" | null}
```

Forget means the item stops appearing in recall. Whether the service deletes it
is the service's decision. The response is a [receipt](#receipts).

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

Response `202` with an empty object. The service extracts memory on its own
schedule; the response carries no result.

- `role` is `user`, `assistant`, `tool` or `context`. `created_at` is RFC 3339
  UTC.
- Block types are `text`, `tool_call` and `tool_result`. Image and reasoning
  blocks are not sent.
- The request body is at most 4 MiB. Otto does not send a larger turn, nor a
  turn that contains a redaction marker or a credential shape.
- Otto sends turns of one session in order, one request at a time, and does not
  retry a failed request.

## Receipts

`/v1/remember` and `/v1/forget` return `200` with:

```json
{"state": "applied" | "no_change" | "skipped" | "pending" | "failed",
 "id": "m-17", "revision": "5"}
```

| `state` | Meaning |
| --- | --- |
| `applied` | Stored or suppressed. |
| `no_change` | The item was already in that state. |
| `skipped` | The service declined to adopt the write. |
| `pending` | Accepted; the service decides later. |
| `failed` | Not stored. |

`id` is required for `applied` and `no_change`. A timeout or a transport error
after the request was sent is reported by Otto as an unknown outcome: the write
may or may not have happened.

## Rendering

Otto renders recalled items before use, and the byte length of the output is
what `max_bytes` bounds:

```text
<preamble>                       131 bytes
for each item:
<memory id=J(id) scope=J(ns "/" scope_id) kind="reference" key="">E(text)</memory>\n
```

`J(s)` is the JSON string encoding of `s`, quotes included. `E(s)` replaces `&`
with `&amp;`, `'` with `&#39;`, `<` with `&lt;`, `>` with `&gt;` and `"` with
`&#34;`. No items render as the empty string. The preamble marks the items as
untrusted reference material. The rendering changes only with a new major
version.

## Errors

A non-2xx response other than `404` from `/v1/get` has the body
`{"error": {"code": "<code>", "message": "<text>"}}`.

| HTTP | `code` | Otto error kind |
| --- | --- | --- |
| 400 | `invalid_input` | invalid input |
| 401 | `unauthorized` | unauthorized |
| 403 | `forbidden` | unauthorized |
| 409 | `conflict` | conflict |
| 413 | `too_large` | invalid input |
| 429 | `rate_limited` | rate limited |
| 503 | `unavailable` | unavailable |
| 400 | `unsupported_version` | unsupported |

Any other status, an unparseable body or an unknown code is treated as
unavailable. `message` is untrusted: Otto never shows it to the model or the
user, and records it, truncated to 512 bytes, in the Otto log.

## Versioning and compatibility

- The major version is in the path (`/v1/`). The minor version is in the
  `Otto-Memory-Protocol` header of every request and response.
- A minor version only adds optional request fields, response fields,
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
- One Otto release speaks one major version. A service that supports Otto
  releases on both sides of a major change serves both paths.

## Limits

| Limit | Value |
| --- | --- |
| Response body | 1 MiB |
| Turn upload request | 4 MiB |
| Item text, remember text, recall query | 8192 bytes |
| Scope id | 1 to 128 printable ASCII bytes |
| Item id | 1 to 256 printable ASCII bytes |
| `revision` | 256 bytes |
| `metadata` | 32 entries, 4 KiB |
| Recall and get timeout | `recall_timeout`, default 3 s, at most 10 s |
| Write timeout | 30 s |
