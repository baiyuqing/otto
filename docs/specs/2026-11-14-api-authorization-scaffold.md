# API authorization scaffold

Status: approved 2026-11-14.

## Problem

`otto serve` authenticates a caller but never records who the caller is:

- A TCP request passes when it carries the one bearer token
  (`crates/otto-server/src/auth.rs`).
- A Unix socket request, and every ACP request, passes because the socket
  router attaches the field-less marker `SocketRequest`
  (`crates/otto-server/src/middleware.rs`, `Server::socket_router`). ACP
  reaches the HTTP handlers through that router
  (`crates/otto-acp/src/attach.rs`, `Client::in_process(server.socket_router())`).

No handler can therefore make a decision based on the caller. Per-client
permissions (a chat connector that may not trust directories, change provider
configuration or switch a session to YOLO) need an identity on every request
and one place that decides. This step adds that place and the identity, but no
new permission yet.

Threat-model decision of 2026-11-14: a holder of `OTTO_SERVER_TOKEN` or
`OTTO_ACP_TOKEN` is equivalent to the Otto host account. This step keeps that:
every caller that can authenticate today is the owner.

## Goals

- Every API operation (each method of each route in `Server::router`, 101
  method handlers on 89 paths) passes one authorization function before its
  handler runs.
- Every authenticated request carries a principal, from every transport: TCP
  token, Unix socket, ACP TCP and ACP attach.
- The policy is real but minimal: the owner may do everything; any other
  principal is denied. Production authentication produces only the owner, so
  observable behavior for existing clients does not change.
- Coverage is enforced by construction and by tests, not by a hand-written
  list.

## Non-goals

- Model tool calls inside a session (`write`, bash, MCP) and runs without a
  request (schedules, timers, workflow steps, subagents). They are a later
  step and need the principal stored on sessions and those records.
- Configuring non-owner principals, per-principal tokens, scopes, session
  ownership records, OIDC.
- Checks on targets named in a request body (`POST /v1/sessions` with
  `resume`, `POST`/`DELETE /v1/workspaces`, `POST /v1/schedule/claim`). The
  authorization function receives path parameters only in this step; these
  routes are listed so the next step adds body targets at the handler.
- Transport encryption.

## Design

### Principal

`otto_server::authorization::Principal`:

- `Owner`
- `Client { name: String }`, constructed only by tests in this step.

Assignment:

| Transport | Principal |
| --- | --- |
| TCP with a valid bearer token | `Owner` |
| TCP when the server has no token (Unix-socket-only server, tests) | `Owner` |
| Unix socket router | `Owner`, inserted where `SocketRequest` is inserted today; `SocketRequest` is removed |
| ACP TCP after the `Authorization` preface succeeds (transport removed later in #655) | `Owner`, through a router built for that principal (`Server::router_for(principal)`) |
| ACP attach (`otto acp --attach`) | `Owner`, through the Unix socket |

The token gate keeps its current rules for which routes need a token
(`/healthz` and, on loopback, `/metrics` stay ungated). Authorization applies
to the `/v1/` routes; an ungated route is served without a principal, as
today.

### Operation registry

`OperationId` is an enum with one variant per method handler, named for
`grep` (for example `SessionHistoryRead`, `ConfigChangeConfirm`).

`Server::router` registers each handler through one builder call that takes
the method, the path, the handler and the `OperationId`. The builder adds the
axum route and records `(Method, path) -> OperationId` in a table owned by the
server. A route cannot be added without an operation id.

`OperationId::ALL` lists every variant. A unit test checks that the table and
`ALL` are the same set in both directions.

### Enforcement point

A middleware layer runs after the token gate and before the handler's
extractors:

1. Read the matched path and method.
2. Look up the `OperationId`. A matched route without a table entry is denied
   with `500 unregistered_operation`. This makes a route that bypassed the
   builder fail closed.
3. Call `authorize(principal, operation, path_params) -> Result<(), Denied>`.
4. On `Denied`, respond `403` with error code `forbidden` and log
   `authorization_denied` with `principal`, `operation` and `request_id`. The
   handler does not run.

Every `http_request` log line gains a `principal` field.

`authorize` in this step: `Owner` -> allow; `Client` -> deny.

Because the check runs before the extractors, a denied request gets `403`
whatever its body. The handler's validation errors (`400`) are only reachable
by an allowed principal.

### ACP paths that do not use the router

- Scheduled-delivery claim and acknowledgement in ACP use the schedule store
  directly (`crates/otto-acp/src/attach.rs`, `schedule: server.schedule_store()`).
  The worker calls `authorize` with `OperationId::ScheduleClaim` and
  `ScheduleDelivered`, using the connection's principal.
- `pin_session` and `unpin_session` are bookkeeping for sessions that the
  connection opened through the router. They get no separate check.

The implementation step lists every other direct `Server` call from
`otto-acp` and either routes it through `authorize` or records why it needs no
check.

### Wire format and documentation

- `testdata/server/openapi.yaml`:
  - Add a shared `403 forbidden` response to every `/v1/` operation. The
    existing `403` for workspace admission keeps its own error code.
  - Remove the five operations of removed features that the router no
    longer serves: `reflect`, `reflect/cancel`, `reflection/skills`,
    `reflection/skills/{name}/revert` and `notices` under
    `/v1/sessions/{id}`.
  - Document the five routes that the router serves but the document omits:
    - `/v1/sessions/{id}/approval-events`
    - `/v1/sessions/{id}/approval-mode`
    - `/v1/sessions/{id}/approval-policy`
    - `/v1/sessions/{id}/local-commands`
    - `/v1/sessions/{id}/tasks/observer`
- `docs/manual/servers-and-connectors.md`:
  - Add `403 forbidden` to the error table.
  - State that all authenticated callers are the owner.
- The hand-written `ROUTES` list in
  `crates/otto-server/src/tests/endpoint_cases.rs` lists 66 of the 89 paths.
  The reachability and OpenAPI tests use the registry table instead, and the
  list is removed.

## Tests

| Test | Fails without the change because |
| --- | --- |
| For every table entry, a request as `Client { name: "test" }` returns `403 forbidden`, and a sampled state-changing route (`POST /v1/sessions`) creates nothing | without the middleware the handler runs and returns `2xx`, `400` or `404` |
| Table and `OperationId::ALL` are the same set | a handler added without an id, or an id without a route, breaks the equality |
| A router with a route added outside the builder answers `500 unregistered_operation` | without the fail-closed branch the route is served |
| Socket, TCP-token and no-token requests reach handlers as `Owner` (existing endpoint tests pass unchanged; one test asserts `principal=owner` in the log line) | a transport that attaches no principal is denied |
| ACP connections use `Server::router_for(principal)`, so the `Client` test above covers the ACP request path; an ACP connection whose principal is a `Client` gets no schedule delivery worker | without the check the worker registers and claims items for a refused principal |
| `openapi.yaml` documents exactly the registered operations, in both directions | the five undocumented routes and the five operations of the removed reflection and notices features fail it today |

All tests are offline and use the existing `otto-server` and `otto-acp` test
harnesses.

## Delivery

One PR on `feat/api-authorization-scaffold`:

- `crates/otto-server/src/authorization.rs` and
  `authorization/operations.rs` hold the principal, the registry and the
  policy. Each file stays under 800 lines.
- Changes to `lib.rs` (router registration), `middleware.rs`, the `otto-acp`
  TCP and attach wiring, `openapi.yaml`, the manual and `CHANGELOG.md`.

## Next steps (not in this PR)

1. Principal configuration (`[server.principals.<name>]` with `token_env` and
   scopes; the key is forbidden in project configuration).
2. Scopes and the per-operation scope table.
3. Body targets at the handlers listed under Non-goals.
4. Session ownership records and the ownership check.
5. Principal on sessions and on derived runs, and per-principal tool sets.
