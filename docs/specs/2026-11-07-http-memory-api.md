# HTTP memory management API

Status: approved 2026-11-07 and implemented. Current behavior lives in the
[HTTP memory management manual](../manual/servers-and-connectors.md#memory-management);
this note keeps the authority and error-mapping rationale.

## Problem

Web and desktop apps that use `otto serve` as their backend cannot manage
memory. The lifecycle (states, verification, history, restore, proposals,
candidate review, retraction) is reachable only from the CLI, the REPL/TUI
slash commands, and two ACP extensions (`_otto/memory/pending`,
`_otto/memory/review`). The OpenAPI document has no memory routes.

## Goals

- Expose every `/memory` and `/remember` operation of the built-in store as
  JSON routes under the existing bearer-token gate, described in
  `testdata/server/openapi.yaml`.
- Reuse the shared management layer (`otto::app::management`); do not copy
  memory semantics into `otto-server`.
- Give clients machine-readable errors and optimistic concurrency, which a
  multi-window UI needs and the string-returning slash path lacks.

Non-goals: memory change events over SSE (clients re-read after a write or
poll), editing a candidate during review, new memory semantics, plugin memory
record management (plugins manage their own records, as today).

## Routes

All routes are session-scoped, like skills and MCP: the session's controller
binds the user scope and the workspace scope, so the same session id gives the
same memory view as `/memory` in that session. Prefix:
`/v1/sessions/{id}/memory`.

| Method and path | Operation | Body / query | Success |
| --- | --- | --- | --- |
| `GET /records` | list | query `scope=current\|user\|workspace\|all`, `state` (repeatable, `active\|needs_review\|superseded\|retracted\|all`), `kind`, `label` (repeatable), `updated_after`, `verified_after` (RFC 3339 or age), `unverified`, `review_due`, `limit` (1-100, the store page maximum; default 50), `cursor` | `200 {records, next_cursor}` |
| `POST /records` | remember (human write) | `{scope: user\|workspace, kind?, key?, text, review_in?}` | `201 Record` |
| `GET /records/{record_id}` | show (any state) | | `200 Record` |
| `DELETE /records/{record_id}` | forget | query `expected_revision?` | `200 {tombstone, previous_revision}` |
| `POST /records/{record_id}/state` | set state | `{state: active\|needs_review\|retracted\|superseded, superseded_by?, reason?, expected_revision?}` | `200 Record` |
| `POST /records/{record_id}/verify` | verify | `{review_in?, expected_revision?}` | `200 Record` |
| `GET /records/{record_id}/history` | history | | `200 {revisions: [Record], skipped: [{revision, reason}]}` |
| `GET /records/{record_id}/diff` | diff | query `from`, `to?` | `200 {from: Record, to: Record}` |
| `POST /records/{record_id}/restore` | restore | `{revision, expected_revision?}` | `200 Record` |
| `POST /records/{record_id}/propose` | model proposal | `{evidence}` | `200 {outcome: proposed\|no_change\|rejected\|unavailable, candidate?, detail?}` |
| `GET /search` | active-record search | query `q` | `200 {records}` |
| `GET /candidates` | pending human candidates | query `limit` (1-50, default 20), `cursor` | `200 {candidates, next_cursor}` |
| `POST /candidates/{candidate_id}/review` | accept or reject | `{decision: accept\|reject}` | `200 {decision, candidate, record?, tombstone?}` |

`Record` JSON: `id, scope{namespace,id}, kind, key, text, labels, metadata,
origin, confidence, revision, state, state_reason, superseded_by, created_at,
updated_at, verified_at, review_at, expires_at, review_due, effective`.
`Candidate` JSON: the fields of the ACP `_otto/memory/pending` candidate plus
`target_id`, `base_revision`, `created_at` and the full proposed `Record`.
Snake case, as in the rest of the HTTP API. These DTOs live in `otto-server`.

`expected_revision` is optional: when set, the write fails with `409
memory_conflict` if the record has moved on, so a UI never overwrites a change
made in another window or by the REPL. Without it the current revision is used,
as the slash commands do.

## Authority

The bearer token (or Unix-socket permission) already authenticates the human
operator of `otto serve`; the model cannot issue HTTP requests to these routes
through its tools any more than through ACP. Writes are therefore recorded as
human operations: review uses `Controller::review_memory` with origin label
`http-client`, like `acp-client` for ACP. `propose` only queues a pending
candidate; it still needs a separate review.

## Errors

The management layer returns `Result<Reply, String>`, which loses the error
category. It gains a typed error, `ManagementError { kind, message }`, with
kinds `Unavailable`, `Unsupported` (plugin source), `NotFound`, `Conflict`,
`Invalid` and `Internal`, derived from `memory::ErrorKind`. Existing slash and
CLI callers keep their text through `Display`, so their output does not
change. The HTTP mapping:

| Kind | Status | `error.code` |
| --- | --- | --- |
| Unavailable | 503 | `memory_unavailable` |
| Unsupported | 501 | `memory_unsupported` |
| NotFound | 404 | `not_found` |
| Conflict | 409 | `memory_conflict` (also a decided or changed candidate) |
| Invalid | 400 | `bad_request` |
| Internal | 500 | `internal` (message only logged) |

An unknown session stays `404 not_found`.

## Implementation outline

- `otto::app::management`: add `expected_revision: Option<u64>` to
  `MemoryState`, `MemoryVerify`, `MemoryForget` and `MemoryRestore`; add a
  `limit`/`cursor` page to `MemoryReviewList` (reusing
  `human_pending_candidate_page`, as ACP does); introduce the typed error.
  Slash/CLI parsers pass `None` and the old page size.
- `otto-server/src/memory_routes.rs` (and a DTO child module if the file
  approaches the size limit): handlers, DTOs and error mapping; routes added in
  `Server::router`. `propose` runs the blocking management call on
  `spawn_blocking`.
- `openapi.yaml`: paths and `MemoryRecord`, `MemoryCandidate`,
  `MemoryRecordPage` schemas; the existing OpenAPI route-coverage test keeps
  routes and document in sync.
- Docs: HTTP API section of `docs/manual/servers-and-connectors.md`, a link
  from the memory manual, CHANGELOG entry.

## Tests (offline)

`crates/otto-server/src/tests/memory_route_cases.rs` against a temporary
file store: list filters and paging; remember then show; state to retracted
and superseded (with replacement validation); verify clears `needs_review`;
history/diff/restore; forget then show is 404; stale `expected_revision` is
409 and leaves the record unchanged; candidate list and accept/reject (second
decision is 409); unknown session/record 404; memory disabled is 503; plugin
source is 501; propose without a generator reports unavailable. Management
layer unit tests cover `expected_revision` and typed error kinds.
